use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

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
}

impl ToolExecutor {
    pub fn new(searxng_url: Option<String>) -> Self {
        ToolExecutor {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("roger-bot/1.0")
                .build()
                .unwrap_or_default(),
            searxng_url,
        }
    }

    pub async fn execute(&self, call: &ToolCall) -> String {
        info!(tool = %call.function.name, "executing tool");
        let args: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(e) => return format!("error: bad arguments: {}", e),
        };

        match call.function.name.as_str() {
            "web_search" => {
                let query = args["query"].as_str().unwrap_or("").to_string();
                self.web_search(&query).await
            }
            "web_fetch" => {
                let url = args["url"].as_str().unwrap_or("").to_string();
                self.web_fetch(&url).await
            }
            other => format!("error: unknown tool '{}'", other),
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

    #[test]
    fn tool_definitions_is_valid_json_array() {
        let defs = tool_definitions();
        assert!(defs.is_array());
        let arr = defs.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let names: Vec<&str> = arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, &["web_search", "web_fetch"]);
    }
}
