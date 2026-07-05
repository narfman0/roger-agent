use crate::mcp::McpManager;
use crate::room_workdirs::RoomWorkdirStore;
use futures_util::future::BoxFuture;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

tokio::task_local! {
    /// The room id of the in-flight request, set by the orchestrator around the
    /// producer task so the `set_workdir` tool knows which room to record against.
    pub static ROOM_ID: String;

    /// Handle for delegating to named subagents (`run_subagent`), set by the
    /// orchestrator when subagents are configured. Implemented in the handler,
    /// which has the LLM registry; kept as a trait so `tools` stays decoupled.
    pub static SUBAGENT: Arc<dyn SubagentHost>;
}

/// Runs named subagents on behalf of the `run_subagent` tool. The handler provides
/// the implementation (it owns the LLM registry + agent config).
pub trait SubagentHost: Send + Sync {
    /// (name, description) of available subagents, for the tool schema.
    fn agents(&self) -> Vec<(String, String)>;
    /// Run a named subagent on a task, returning its text result.
    fn run<'a>(&'a self, name: &'a str, task: &'a str) -> BoxFuture<'a, String>;
}

// ── Tool definitions sent to the LLM ────────────────────────────────────────

/// Returns the tool definitions array to include in every chat request.
pub fn tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web for current information, news, and facts. Use this when you need up-to-date information or when you don't know something.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query"
                        }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch and read the contents of a web page. Use this to read articles, documentation, or any URL.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The full URL to fetch"
                        }
                    },
                    "required": ["url"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file on the local filesystem. Use ~ for the home directory.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or ~ path to the file"
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write (or overwrite) a file on the local filesystem. Creates parent directories as needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or ~ path to the file"
                        },
                        "content": {
                            "type": "string",
                            "description": "Text content to write"
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List the contents of a directory on the local filesystem.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or ~ path to the directory"
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_history",
                "description": "Search past conversation history across rooms for messages containing a keyword or phrase. Returns matching messages with room context. Use when the user asks about something discussed before, or to recall a past decision.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keyword or phrase to search for (case-insensitive)"
                        },
                        "room": {
                            "type": "string",
                            "description": "Limit search to a specific room ID (optional; omit to search all rooms)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max number of matching messages to return (default 10, max 50)"
                        }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_skill",
                "description": "Load the full steps of a named skill listed in the Skills section.",
                "parameters": {
                    "type": "object",
                    "properties": { "name": { "type": "string", "description": "The skill name" } },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_skill",
                "description": "Capture a reusable procedure as a skill (saved pending user approval). Use after solving a non-trivial task that would recur.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Short kebab-case skill name" },
                        "description": { "type": "string", "description": "One-line summary of what it does / when to use it" },
                        "steps": { "type": "string", "description": "The procedure, as markdown steps" }
                    },
                    "required": ["name", "description", "steps"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_workdir",
                "description": "Set the working directory (project) for this room's agentic coding agent. Call this when the user asks to work on, edit, or build a specific known project. The choice persists for the room until changed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "project": {
                            "type": "string",
                            "description": "The name of a known project to work in"
                        }
                    },
                    "required": ["project"]
                }
            }
        }
    ])
}

// ── Tool call / result wire types ───────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

// ── Executor ────────────────────────────────────────────────────────────────

pub struct ToolExecutor {
    http: Client,
    pub searxng_url: Option<String>,
    /// Known projects (name → expanded path) for the `set_workdir` tool.
    projects: HashMap<String, String>,
    /// Persists per-room workdir selections; shared with the handler.
    room_workdirs: Option<Arc<RoomWorkdirStore>>,
    /// Connected MCP servers whose tools are also exposed to the model.
    mcp: Option<Arc<McpManager>>,
    /// Skill store for read_skill / write_skill.
    skills: Option<Arc<crate::skills::SkillStore>>,
    /// History store for search_history.
    history: Option<Arc<crate::history::HistoryStore>>,
}

impl ToolExecutor {
    pub fn with_projects(
        searxng_url: Option<String>,
        projects: HashMap<String, String>,
        room_workdirs: Option<Arc<RoomWorkdirStore>>,
        mcp: Option<Arc<McpManager>>,
        skills: Option<Arc<crate::skills::SkillStore>>,
    ) -> Self {
        ToolExecutor {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("roger-bot/1.0")
                .build()
                .unwrap_or_default(),
            searxng_url,
            projects,
            room_workdirs,
            mcp,
            skills,
            history: None,
        }
    }

    pub fn with_history(mut self, history: Arc<crate::history::HistoryStore>) -> Self {
        self.history = Some(history);
        self
    }

    /// Connected MCP servers + total MCP tools (for `/status`).
    pub fn mcp_summary(&self) -> (usize, usize) {
        self.mcp.as_ref().map_or((0, 0), |m| m.summary())
    }

    /// The full tool list advertised to the model: roger's native tools, connected
    /// MCP server tools, and — when subagents are configured for this turn —
    /// `run_subagent`.
    pub fn tool_definitions(&self) -> Value {
        let mut defs = match tool_definitions() {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        if let Some(mcp) = &self.mcp {
            defs.extend(mcp.tool_definitions());
        }
        if let Ok(agents) = SUBAGENT.try_with(|h| h.agents()) {
            if !agents.is_empty() {
                let listed = agents
                    .iter()
                    .map(|(n, d)| if d.is_empty() { n.clone() } else { format!("{} — {}", n, d) })
                    .collect::<Vec<_>>()
                    .join("; ");
                defs.push(json!({
                    "type": "function",
                    "function": {
                        "name": "run_subagent",
                        "description": format!("Delegate a self-contained task to a named subagent and get its result back. Available agents: {}", listed),
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "agent": { "type": "string", "description": "The subagent name" },
                                "task": { "type": "string", "description": "The task, with all context the subagent needs" }
                            },
                            "required": ["agent", "task"]
                        }
                    }
                }));
            }
        }
        Value::Array(defs)
    }

    pub async fn execute(&self, call: &ToolCall) -> String {
        info!(tool = %call.function.name, "executing tool");
        let args: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(e) => return format!("error: bad arguments: {}", e),
        };

        if McpManager::handles(&call.function.name) {
            return match &self.mcp {
                Some(mcp) => mcp.call(&call.function.name, args).await,
                None => "error: MCP is not configured".to_string(),
            };
        }

        match call.function.name.as_str() {
            "web_search" => {
                let query = args["query"].as_str().unwrap_or("").to_string();
                self.web_search(&query).await
            }
            "web_fetch" => {
                let url = args["url"].as_str().unwrap_or("").to_string();
                self.web_fetch(&url).await
            }
            "read_file" => {
                let path = args["path"].as_str().unwrap_or("").to_string();
                read_file(&path)
            }
            "write_file" => {
                let path = args["path"].as_str().unwrap_or("").to_string();
                let content = args["content"].as_str().unwrap_or("").to_string();
                write_file(&path, &content)
            }
            "list_dir" => {
                let path = args["path"].as_str().unwrap_or("").to_string();
                list_dir(&path)
            }
            "set_workdir" => {
                let project = args["project"].as_str().unwrap_or("").to_string();
                self.set_workdir(&project)
            }
            "read_skill" => {
                let name = args["name"].as_str().unwrap_or("");
                match &self.skills {
                    Some(s) => s.read(name).unwrap_or_else(|| format!("error: no skill named '{}'", name)),
                    None => "error: skills are not configured".to_string(),
                }
            }
            "write_skill" => {
                let name = args["name"].as_str().unwrap_or("");
                let desc = args["description"].as_str().unwrap_or("");
                let steps = args["steps"].as_str().unwrap_or("");
                match &self.skills {
                    None => "error: skills are not configured".to_string(),
                    Some(s) => {
                        let content = format!("# {}\n\n{}\n\n{}", name, desc, steps);
                        match s.write_pending(name, &content) {
                            Ok(_) => format!(
                                "Drafted skill '{}' (pending). Ask the user to run `/skills approve {}`.",
                                name, name
                            ),
                            Err(e) => format!("error: {}", e),
                        }
                    }
                }
            }
            "search_history" => {
                let query = args["query"].as_str().unwrap_or("").to_string();
                let room_filter = args["room"].as_str().map(|s| s.to_string());
                let limit = args["limit"].as_u64().unwrap_or(10).min(50) as usize;
                match &self.history {
                    Some(h) => search_history(h, &query, room_filter.as_deref(), limit),
                    None => "error: history search is not configured".to_string(),
                }
            }
            "run_subagent" => {
                let agent = args["agent"].as_str().unwrap_or("").to_string();
                let task = args["task"].as_str().unwrap_or("").to_string();
                match SUBAGENT.try_with(|h| h.clone()) {
                    Ok(host) => host.run(&agent, &task).await,
                    Err(_) => "error: subagents are not available".to_string(),
                }
            }
            other => format!("error: unknown tool '{}'", other),
        }
    }

    /// Resolve a known project name to its path and record it as the in-flight
    /// room's workdir (read from the `ROOM_ID` task-local). Persists immediately.
    fn set_workdir(&self, project: &str) -> String {
        let store = match &self.room_workdirs {
            Some(s) => s,
            None => return "error: workdir routing is not configured".into(),
        };
        let room = match ROOM_ID.try_with(|r| r.clone()) {
            Ok(r) => r,
            Err(_) => return "error: no room context for set_workdir".into(),
        };
        let path = match self.projects.get(project) {
            Some(p) => p.clone(),
            None => {
                let mut names: Vec<&String> = self.projects.keys().collect();
                names.sort();
                let list = names
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return format!(
                    "error: unknown project '{}'. Known projects: {}",
                    project,
                    if list.is_empty() { "(none configured)" } else { &list }
                );
            }
        };
        match store.set(&room, &path) {
            Ok(_) => format!("Working directory for this room set to '{}' ({}).", project, path),
            Err(e) => format!("error: failed to save workdir: {}", e),
        }
    }

    async fn web_search(&self, query: &str) -> String {
        let base = match &self.searxng_url {
            Some(u) => u.clone(),
            None => return "error: web search is not configured (no SEARXNG_URL)".into(),
        };

        let url = format!("{}/search", base.trim_end_matches('/'));
        match self.http.get(&url)
            .query(&[("q", query), ("format", "json"), ("categories", "general")])
            .send()
            .await
        {
            Err(e) => {
                warn!("searxng request failed: {}", e);
                format!("error: search request failed: {}", e)
            }
            Ok(resp) if !resp.status().is_success() => {
                format!("error: search returned {}", resp.status())
            }
            Ok(resp) => match resp.json::<SearxngResponse>().await {
                Err(e) => format!("error: failed to parse search results: {}", e),
                Ok(results) => format_search_results(query, &results),
            },
        }
    }

    async fn web_fetch(&self, url: &str) -> String {
        if url.is_empty() {
            return "error: empty URL".into();
        }
        match self.http.get(url).send().await {
            Err(e) => format!("error: fetch failed: {}", e),
            Ok(resp) if !resp.status().is_success() => {
                format!("error: {} returned {}", url, resp.status())
            }
            Ok(resp) => match resp.text().await {
                Err(e) => format!("error: failed to read response: {}", e),
                Ok(html) => extract_text(&html, url),
            },
        }
    }
}

// ── File tools ───────────────────────────────────────────────────────────────

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path.starts_with("~/") || path == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        std::path::PathBuf::from(path.replacen('~', &home, 1))
    } else {
        std::path::PathBuf::from(path)
    }
}

fn read_file(path: &str) -> String {
    if path.is_empty() {
        return "error: path is required".into();
    }
    let p = expand_tilde(path);
    match std::fs::read_to_string(&p) {
        Err(e) => format!("error reading {}: {}", p.display(), e),
        Ok(content) => {
            const MAX: usize = 50_000;
            if content.len() > MAX {
                format!(
                    "[{}]\n\n{}\n\n[truncated: {} of {} bytes shown]",
                    p.display(),
                    &content[..MAX],
                    MAX,
                    content.len()
                )
            } else {
                format!("[{}]\n\n{}", p.display(), content)
            }
        }
    }
}

fn write_file(path: &str, content: &str) -> String {
    if path.is_empty() {
        return "error: path is required".into();
    }
    let p = expand_tilde(path);
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("error creating directories for {}: {}", p.display(), e);
        }
    }
    match std::fs::write(&p, content) {
        Ok(()) => format!("wrote {} bytes to {}", content.len(), p.display()),
        Err(e) => format!("error writing {}: {}", p.display(), e),
    }
}

fn list_dir(path: &str) -> String {
    let p = if path.is_empty() { expand_tilde("~") } else { expand_tilde(path) };
    let entries = match std::fs::read_dir(&p) {
        Err(e) => return format!("error listing {}: {}", p.display(), e),
        Ok(it) => it,
    };
    let mut lines = vec![format!("[{}]", p.display())];
    let mut items: Vec<(String, bool, u64)> = entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let meta = e.metadata().ok();
            let is_dir = meta.as_ref().map_or(false, |m| m.is_dir());
            let size = meta.as_ref().map_or(0, |m| m.len());
            (e.file_name().to_string_lossy().into_owned(), is_dir, size)
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, is_dir, size) in &items {
        if *is_dir {
            lines.push(format!("  {}/", name));
        } else {
            lines.push(format!("  {} ({} bytes)", name, size));
        }
    }
    if items.is_empty() {
        lines.push("  (empty)".into());
    }
    lines.join("\n")
}

/// Case-insensitive substring search across room history files.
/// Returns up to `limit` matching messages formatted for LLM consumption.
fn search_history(
    history: &crate::history::HistoryStore,
    query: &str,
    room_filter: Option<&str>,
    limit: usize,
) -> String {
    if query.is_empty() {
        return "error: query must not be empty".to_string();
    }
    let query_lower = query.to_lowercase();
    let rooms: Vec<String> = match room_filter {
        Some(r) => vec![r.to_string()],
        None => history.list_room_ids(),
    };
    if rooms.is_empty() {
        return "No history found.".to_string();
    }

    let mut results: Vec<String> = Vec::new();
    'outer: for room_id in &rooms {
        let msgs = history.load(room_id);
        for (i, msg) in msgs.iter().enumerate() {
            if msg.content.to_lowercase().contains(&query_lower) {
                // Include one message of context before and after the match.
                let start = i.saturating_sub(1);
                let end = (i + 2).min(msgs.len());
                let ctx: Vec<String> = msgs[start..end]
                    .iter()
                    .enumerate()
                    .map(|(j, m)| {
                        let marker = if start + j == i { ">>>" } else { "   " };
                        let snippet = if m.content.len() > 300 {
                            format!("{}…", &m.content[..300])
                        } else {
                            m.content.clone()
                        };
                        format!("{} [{}] {}", marker, m.role, snippet)
                    })
                    .collect();
                results.push(format!("Room: {}\n{}", room_id, ctx.join("\n")));
                if results.len() >= limit {
                    break 'outer;
                }
            }
        }
    }

    if results.is_empty() {
        format!("No messages found matching '{}'.", query)
    } else {
        format!(
            "Found {} match(es) for '{}':\n\n{}",
            results.len(),
            query,
            results.join("\n\n---\n\n")
        )
    }
}

// ── SearXNG response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearxngResponse {
    results: Vec<SearxngResult>,
    #[serde(default)]
    answers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    published_date: Option<String>,
}

fn format_search_results(query: &str, resp: &SearxngResponse) -> String {
    if resp.results.is_empty() && resp.answers.is_empty() {
        return format!("No results found for: {}", query);
    }

    let mut out = format!("Search results for: {}\n\n", query);

    for answer in &resp.answers {
        out.push_str(&format!("Answer: {}\n\n", answer));
    }

    for (i, r) in resp.results.iter().take(5).enumerate() {
        out.push_str(&format!("{}. **{}**\n", i + 1, r.title));
        out.push_str(&format!("   URL: {}\n", r.url));
        if !r.content.is_empty() {
            let snippet = r.content.chars().take(300).collect::<String>();
            out.push_str(&format!("   {}\n", snippet));
        }
        if let Some(date) = &r.published_date {
            out.push_str(&format!("   Published: {}\n", date));
        }
        out.push('\n');
    }

    out
}

// ── HTML → plain text extraction ────────────────────────────────────────────

fn extract_text(html: &str, url: &str) -> String {
    // Strip script/style blocks first
    let without_scripts = strip_tags_block(html, "script");
    let without_style = strip_tags_block(&without_scripts, "style");

    // Remove all remaining HTML tags
    let mut text = String::with_capacity(without_style.len());
    let mut in_tag = false;
    for ch in without_style.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }

    // Collapse whitespace and decode basic entities
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");

    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let collapsed = lines.join("\n");

    // Cap output to avoid overwhelming the context window
    const MAX_CHARS: usize = 8000;
    if collapsed.len() > MAX_CHARS {
        format!(
            "[Content from {}]\n\n{}\n\n[truncated at {} chars]",
            url,
            &collapsed[..MAX_CHARS],
            MAX_CHARS
        )
    } else {
        format!("[Content from {}]\n\n{}", url, collapsed)
    }
}

fn strip_tags_block(html: &str, tag: &str) -> String {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.to_lowercase().find(&open) {
        out.push_str(&rest[..start]);
        match rest[start..].to_lowercase().find(&close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => break,
        }
    }
    out.push_str(rest);
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_strips_html_tags() {
        let html = "<html><head><title>T</title></head><body><p>Hello world</p></body></html>";
        let text = extract_text(html, "http://example.com");
        assert!(text.contains("Hello world"));
        assert!(!text.contains("<p>"));
    }

    #[test]
    fn extract_text_strips_script_blocks() {
        let html = "<p>visible</p><script>alert('hidden')</script><p>also visible</p>";
        let text = extract_text(html, "http://example.com");
        assert!(text.contains("visible"));
        assert!(!text.contains("hidden"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn extract_text_decodes_entities() {
        let html = "<p>a &amp; b &lt;3 &gt; c</p>";
        let text = extract_text(html, "http://example.com");
        assert!(text.contains("a & b <3 > c"));
    }

    #[test]
    fn extract_text_truncates_long_content() {
        let html = format!("<p>{}</p>", "x".repeat(10000));
        let text = extract_text(&html, "http://example.com");
        assert!(text.contains("[truncated at 8000 chars]"));
    }

    #[test]
    fn format_search_results_empty() {
        let resp = SearxngResponse { results: vec![], answers: vec![] };
        let out = format_search_results("test", &resp);
        assert!(out.contains("No results found"));
    }

    #[test]
    fn format_search_results_shows_up_to_five() {
        let results = (0..8)
            .map(|i| SearxngResult {
                title: format!("Title {}", i),
                url: format!("http://example.com/{}", i),
                content: "snippet".into(),
                published_date: None,
            })
            .collect();
        let resp = SearxngResponse { results, answers: vec![] };
        let out = format_search_results("test", &resp);
        assert!(out.contains("Title 4"));
        assert!(!out.contains("Title 5"));
    }

    fn workdir_call(project: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            kind: "function".into(),
            function: ToolFunction {
                name: "set_workdir".into(),
                arguments: format!(r#"{{"project":"{}"}}"#, project),
            },
        }
    }

    #[tokio::test]
    async fn set_workdir_records_for_room() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RoomWorkdirStore::load(dir.path().join("rw.json")));
        let mut projects = HashMap::new();
        projects.insert("foo".to_string(), "/tmp/foo".to_string());
        let exec = ToolExecutor::with_projects(None, projects, Some(store.clone()), None, None);

        let out = ROOM_ID
            .scope("!room:s".to_string(), exec.execute(&workdir_call("foo")))
            .await;
        assert!(out.contains("/tmp/foo"), "got: {}", out);
        assert_eq!(store.get("!room:s").as_deref(), Some("/tmp/foo"));
    }

    #[tokio::test]
    async fn set_workdir_unknown_project_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RoomWorkdirStore::load(dir.path().join("rw.json")));
        let exec = ToolExecutor::with_projects(None, HashMap::new(), Some(store.clone()), None, None);

        let out = ROOM_ID
            .scope("!room:s".to_string(), exec.execute(&workdir_call("bar")))
            .await;
        assert!(out.starts_with("error: unknown project"), "got: {}", out);
        assert!(store.get("!room:s").is_none());
    }

    #[test]
    fn executor_tool_definitions_returns_native_without_mcp() {
        let exec = ToolExecutor::with_projects(None, HashMap::new(), None, None, None);
        let arr = exec.tool_definitions();
        assert_eq!(arr.as_array().unwrap().len(), 9);
    }

    struct MockHost;
    impl SubagentHost for MockHost {
        fn agents(&self) -> Vec<(String, String)> {
            vec![("coder".into(), "writes code".into())]
        }
        fn run<'a>(&'a self, _name: &'a str, _task: &'a str) -> BoxFuture<'a, String> {
            Box::pin(async { "subagent output".to_string() })
        }
    }

    #[tokio::test]
    async fn run_subagent_advertised_only_when_scoped() {
        let exec = ToolExecutor::with_projects(None, HashMap::new(), None, None, None);
        assert_eq!(exec.tool_definitions().as_array().unwrap().len(), 9);

        let host: Arc<dyn SubagentHost> = Arc::new(MockHost);
        let defs = SUBAGENT.scope(host, async { exec.tool_definitions() }).await;
        let names: Vec<String> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"run_subagent".to_string()));
    }

    #[tokio::test]
    async fn run_subagent_routes_to_host_or_errors() {
        let exec = ToolExecutor::with_projects(None, HashMap::new(), None, None, None);
        let call = ToolCall {
            id: "1".into(),
            kind: "function".into(),
            function: ToolFunction {
                name: "run_subagent".into(),
                arguments: r#"{"agent":"coder","task":"hi"}"#.into(),
            },
        };
        // Unscoped → not available.
        assert!(exec.execute(&call).await.contains("not available"));
        // Scoped → routed to the host.
        let host: Arc<dyn SubagentHost> = Arc::new(MockHost);
        let out = SUBAGENT.scope(host, exec.execute(&call)).await;
        assert_eq!(out, "subagent output");
    }

    #[test]
    fn tool_definitions_is_valid_json_array() {
        let defs = tool_definitions();
        assert!(defs.is_array());
        let arr = defs.as_array().unwrap();
        assert_eq!(arr.len(), 9);
        let names: Vec<&str> = arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            &[
                "web_search", "web_fetch", "read_file", "write_file", "list_dir",
                "search_history", "read_skill", "write_skill", "set_workdir"
            ]
        );
    }

    #[tokio::test]
    async fn search_history_returns_no_results_on_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let history = Arc::new(crate::history::HistoryStore::new(dir.path().join("h")).unwrap());
        let result = search_history(&history, "hello", None, 10);
        assert!(result.contains("No") || result.contains("no"), "got: {}", result);
    }

    #[tokio::test]
    async fn search_history_finds_message_in_room() {
        let dir = tempfile::tempdir().unwrap();
        let history = Arc::new(crate::history::HistoryStore::new(dir.path().join("h")).unwrap());
        let msg = crate::history::ChatMessage {
            role: "user".to_string(),
            content: "let's talk about rust and memory safety".to_string(),
        };
        history.append("!room1:server", msg).unwrap();

        let result = search_history(&history, "memory safety", None, 10);
        assert!(result.contains("memory safety"), "got: {}", result);
        assert!(result.contains("room1"), "got: {}", result);
    }

    #[tokio::test]
    async fn search_history_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let history = Arc::new(crate::history::HistoryStore::new(dir.path().join("h")).unwrap());
        let msg = crate::history::ChatMessage {
            role: "assistant".to_string(),
            content: "The quick brown FOX jumps".to_string(),
        };
        history.append("!room1:server", msg).unwrap();

        let result = search_history(&history, "quick brown fox", None, 10);
        assert!(result.contains("FOX"), "got: {}", result);
    }

    #[tokio::test]
    async fn search_history_room_filter_limits_results() {
        let dir = tempfile::tempdir().unwrap();
        let history = Arc::new(crate::history::HistoryStore::new(dir.path().join("h")).unwrap());
        let msg = crate::history::ChatMessage {
            role: "user".to_string(),
            content: "needle in room A".to_string(),
        };
        history.append("!roomA:server", msg).unwrap();
        let msg2 = crate::history::ChatMessage {
            role: "user".to_string(),
            content: "needle in room B".to_string(),
        };
        history.append("!roomB:server", msg2).unwrap();

        let result = search_history(&history, "needle", Some("!roomA:server"), 10);
        assert!(result.contains("room A"), "got: {}", result);
        assert!(!result.contains("room B"), "got: {}", result);
    }

    #[tokio::test]
    async fn search_history_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let history = Arc::new(crate::history::HistoryStore::new(dir.path().join("h")).unwrap());
        for i in 0..10 {
            let msg = crate::history::ChatMessage {
                role: "user".to_string(),
                content: format!("hit number {}", i),
            };
            history.append("!room:server", msg).unwrap();
        }
        let result = search_history(&history, "hit number", None, 3);
        // Each match block starts with "Room:" — count those.
        let count = result.matches("Room:").count();
        assert!(count <= 3, "expected ≤3 match blocks, got {}", count);
    }

    #[tokio::test]
    async fn search_history_empty_query_errors() {
        let dir = tempfile::tempdir().unwrap();
        let history = Arc::new(crate::history::HistoryStore::new(dir.path().join("h")).unwrap());
        let result = search_history(&history, "", None, 10);
        assert!(result.starts_with("error:"), "got: {}", result);
    }
}
