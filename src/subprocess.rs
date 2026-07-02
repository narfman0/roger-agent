//! Subprocess backends: spawn an agentic CLI (`claude`, `opencode`) for a turn,
//! stream its output into roger's accumulated-text channel, and enforce a process
//! lifecycle (idle / absolute timeouts, concurrency cap, whole-tree kill).
//!
//! Unlike the HTTP backend, the subprocess owns its own agentic tool loop (file
//! edits, bash, web), so roger's `ToolExecutor` is irrelevant here. History is
//! passed statelessly each turn (rendered into the prompt); we do not use the
//! CLI's own session persistence.

use crate::history::ChatMessage;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{timeout, Instant};
use tracing::{info, warn};

/// Process-wide cap on concurrent subprocess children. Set once at startup from
/// `CommsConfig::max_concurrent_children`; persists across config reloads so
/// in-flight accounting isn't reset. Defaults to 3 if never set.
static CHILD_SEM: OnceLock<Semaphore> = OnceLock::new();

/// Initialize the concurrency cap. Call once at startup before building backends.
pub fn set_child_limit(n: usize) {
    if CHILD_SEM.set(Semaphore::new(n.max(1))).is_err() {
        warn!("subprocess child limit already set; ignoring");
    }
}

fn child_sem() -> &'static Semaphore {
    CHILD_SEM.get_or_init(|| Semaphore::new(3))
}

tokio::task_local! {
    /// Per-request working directory override for subprocess backends, set by the
    /// orchestrator around the producer task (the room's resolved workdir). Avoids
    /// threading a workdir param through the whole chat-call chain.
    pub static WORKDIR: Option<PathBuf>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubprocessKind {
    ClaudeCode,
    OpenCode,
}

/// Git-worktree isolation policy for agentic jobs. When enabled and the workdir is
/// a git repo, each job runs in a throwaway worktree on a fresh branch; changes are
/// committed to that branch for review and the worktree is removed.
#[derive(Debug, Clone)]
pub struct WorktreePolicy {
    pub enabled: bool,
    pub base_dir: PathBuf,
    pub branch_prefix: String,
}

/// Lifecycle limits for one subprocess run.
#[derive(Debug, Clone)]
pub struct ProcLimits {
    /// Kill if no output line arrives within this window.
    pub idle: Duration,
    /// Hard wall-clock kill regardless of output.
    pub ceiling: Duration,
    /// `--max-budget-usd` for claude (cost guard); `None` = unset.
    pub max_budget_usd: Option<f64>,
    /// `--max-turns` cap; `None` = unset.
    pub max_turns: Option<u32>,
}

pub struct SubprocessBackend {
    flavor: SubprocessKind,
    /// Display/log model name; also passed via `--model`.
    model: String,
    /// `ANTHROPIC_BASE_URL` for the spawned process (the gateway).
    base_url: String,
    /// `ANTHROPIC_AUTH_TOKEN` (gateway vkey); `None` falls back to the CLI's own auth.
    auth_token: Option<String>,
    /// cwd for the run. `None` => misconfigured; runs error out.
    workdir: Option<PathBuf>,
    /// Extra reachable roots (`--add-dir`), e.g. known projects.
    extra_dirs: Vec<PathBuf>,
    /// `--permission-mode` (e.g. acceptEdits, bypassPermissions).
    permission_mode: String,
    limits: ProcLimits,
    worktree: WorktreePolicy,
}

impl SubprocessBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        flavor: SubprocessKind,
        model: String,
        base_url: String,
        auth_token: Option<String>,
        workdir: Option<PathBuf>,
        extra_dirs: Vec<PathBuf>,
        permission_mode: String,
        limits: ProcLimits,
        worktree: WorktreePolicy,
    ) -> Self {
        SubprocessBackend {
            flavor,
            model,
            base_url,
            auth_token,
            workdir,
            extra_dirs,
            permission_mode,
            limits,
            worktree,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        self.run(messages, None).await
    }

    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tx: mpsc::Sender<String>,
    ) -> Result<String> {
        self.run(messages, Some(tx)).await
    }

    /// Spawn the CLI, stream output to `tx` (accumulated snapshots, matching the
    /// HTTP `chat_stream` contract), and return the authoritative final text.
    async fn run(&self, messages: &[ChatMessage], tx: Option<mpsc::Sender<String>>) -> Result<String> {
        // Per-request override (room's resolved workdir) wins over the configured
        // default baked in at build time.
        let workdir = WORKDIR
            .try_with(|w| w.clone())
            .ok()
            .flatten()
            .or_else(|| self.workdir.clone())
            .ok_or_else(|| anyhow!("subprocess backend has no workdir (set comms.default_workdir or use set_workdir)"))?;
        if !workdir.is_dir() {
            return Err(anyhow!("workdir does not exist: {}", workdir.display()));
        }

        // Concurrency cap — released when `_permit` drops at end of the run.
        let _permit = child_sem()
            .acquire()
            .await
            .map_err(|_| anyhow!("child semaphore closed"))?;

        // Worktree isolation: for an agentic job in a git repo, run in a throwaway
        // worktree on a fresh branch so the live checkout is never touched. Changes
        // are committed to that branch and surfaced; the worktree is then removed.
        let room = crate::tools::ROOM_ID
            .try_with(|r| r.clone())
            .unwrap_or_else(|_| "job".to_string());
        let (cwd, guard) = if self.worktree.enabled && is_git_repo(&workdir).await {
            match WorktreeGuard::create(&workdir, &self.worktree, &room) {
                Ok(g) => (g.path().to_path_buf(), Some(g)),
                Err(e) => {
                    warn!("worktree setup failed ({}); running in the live workdir", e);
                    (workdir.clone(), None)
                }
            }
        } else {
            (workdir.clone(), None)
        };

        let result = self.execute_in(&cwd, messages, tx).await;

        match guard {
            Some(mut g) => {
                let note = g.finalize().await.unwrap_or(None);
                match (result, note) {
                    (Ok(text), Some(n)) => Ok(format!("{}\n\n🌿 {}", text, n)),
                    (Ok(text), None) => Ok(text),
                    (Err(e), _) => Err(e),
                }
            }
            None => result,
        }
    }

    /// Spawn the CLI in `cwd`, stream output to `tx`, and return the final text.
    async fn execute_in(
        &self,
        cwd: &Path,
        messages: &[ChatMessage],
        tx: Option<mpsc::Sender<String>>,
    ) -> Result<String> {
        let (system, prompt) = render_prompt(messages);
        let program = match self.flavor {
            SubprocessKind::ClaudeCode => "claude",
            SubprocessKind::OpenCode => "opencode",
        };
        let mut cmd = Command::new(program);
        match self.flavor {
            SubprocessKind::ClaudeCode => {
                cmd.arg("--print")
                    .arg("--output-format").arg("stream-json")
                    .arg("--verbose")
                    .arg("--include-partial-messages")
                    .arg("--permission-mode").arg(&self.permission_mode)
                    .arg("--model").arg(&self.model);
                if let Some(sys) = &system {
                    cmd.arg("--append-system-prompt").arg(sys);
                }
                for d in &self.extra_dirs {
                    cmd.arg("--add-dir").arg(d);
                }
                if let Some(b) = self.limits.max_budget_usd {
                    cmd.arg("--max-budget-usd").arg(format!("{}", b));
                }
                if let Some(t) = self.limits.max_turns {
                    cmd.arg("--max-turns").arg(format!("{}", t));
                }
                cmd.arg(&prompt);
                // Empty base_url => let the CLI use its own auth (logged-in session).
                if !self.base_url.is_empty() {
                    cmd.env("ANTHROPIC_BASE_URL", &self.base_url);
                }
                if let Some(token) = &self.auth_token {
                    cmd.env("ANTHROPIC_AUTH_TOKEN", token);
                }
            }
            SubprocessKind::OpenCode => {
                // opencode is self-configured (provider + baseURL live in its own
                // config), so the gateway env vars don't apply. It has no system-
                // prompt flag, so the system prompt is folded into the message.
                // `--format json` emits one text event with the full reply.
                cmd.arg("run").arg("--format").arg("json").arg("--model").arg(&self.model);
                if self.permission_mode == "bypassPermissions" {
                    cmd.arg("--dangerously-skip-permissions");
                }
                let msg = match &system {
                    Some(sys) => format!("{}\n\n{}", sys, prompt),
                    None => prompt.clone(),
                };
                cmd.arg(msg);
            }
        }

        // Augment PATH so subprocess tools installed in non-standard locations
        // (e.g. ~/.opencode/bin, ~/.local/bin, ~/go/bin) are found even when
        // roger was started without those dirs on its PATH.
        {
            let current_path = std::env::var("PATH").unwrap_or_default();
            let home = std::env::var("HOME").unwrap_or_default();
            let extra = [
                format!("{}/.opencode/bin", home),
                format!("{}/.local/bin", home),
                format!("{}/.cargo/bin", home),
                format!("{}/go/bin", home),
                format!("{}/bin", home),
            ]
            .into_iter()
            .filter(|p| !current_path.contains(p.as_str()))
            .collect::<Vec<_>>()
            .join(":");
            if !extra.is_empty() {
                cmd.env("PATH", format!("{}:{}", extra, current_path));
            }
        }

        cmd.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0); // own group so we can kill the whole tree

        info!(program, model = %self.model, workdir = %cwd.display(), "spawning subprocess");
        let mut child = cmd.spawn().map_err(|e| anyhow!("failed to spawn {}: {}", program, e))?;
        let pid = child.id().map(|p| p as i32);
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut lines = BufReader::new(stdout).lines();

        let deadline = Instant::now() + self.limits.ceiling;
        let mut full = String::new();
        let mut final_text: Option<String> = None;
        let mut run_error: Option<String> = None;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                kill_tree(pid, &mut child).await;
                return Err(anyhow!("claude run exceeded absolute ceiling"));
            }
            let step = self.limits.idle.min(remaining);
            match timeout(step, lines.next_line()).await {
                Err(_) => {
                    kill_tree(pid, &mut child).await;
                    return Err(if Instant::now() >= deadline {
                        anyhow!("{} run exceeded absolute ceiling", program)
                    } else {
                        anyhow!("{} produced no output for {:?} (idle timeout)", program, self.limits.idle)
                    });
                }
                Ok(Ok(None)) => break, // EOF
                Ok(Err(e)) => {
                    kill_tree(pid, &mut child).await;
                    return Err(anyhow!("error reading {} output: {}", program, e));
                }
                Ok(Ok(Some(line))) => {
                    let ev = match self.flavor {
                        SubprocessKind::ClaudeCode => parse_claude_line(&line),
                        SubprocessKind::OpenCode => parse_opencode_line(&line),
                    };
                    match ev {
                        StreamEvent::Text(t) => {
                            full.push_str(&t);
                            if let Some(tx) = &tx {
                                if tx.send(full.clone()).await.is_err() {
                                    // Receiver gone (request abandoned) — stop and reap.
                                    kill_tree(pid, &mut child).await;
                                    return Err(anyhow!("output receiver dropped"));
                                }
                            }
                        }
                        StreamEvent::Final { text, is_error } => {
                            if is_error {
                                run_error = Some(text.unwrap_or_else(|| full.clone()));
                            } else {
                                final_text = Some(text.unwrap_or_else(|| full.clone()));
                            }
                        }
                        StreamEvent::Other => {}
                    }
                }
            }
        }

        let status = child.wait().await.ok();
        // If we have valid text, return it even if there was also an error event.
        // Some runtimes (opencode) emit a transient UnknownError on startup and then
        // continue to produce a valid response; silently discarding valid output in
        // favour of the error message causes double-responses and lost replies.
        if let Some(err) = run_error {
            if final_text.is_none() && full.is_empty() {
                return Err(anyhow!("{} reported error: {}", program, err));
            }
            warn!("{} emitted an error event but also produced text; using text (error: {})", program, err);
        }
        if let Some(text) = final_text {
            return Ok(text);
        }
        // No result event: fall back to accumulated deltas, else surface stderr/exit.
        if !full.is_empty() {
            return Ok(full);
        }
        let stderr = read_stderr(&mut child).await;
        Err(anyhow!(
            "{} produced no result (exit {:?}){}",
            program,
            status.and_then(|s| s.code()),
            if stderr.is_empty() { String::new() } else { format!(": {}", stderr) }
        ))
    }
}

/// Render roger's message list into a (system prompt, user prompt) pair for the CLI.
/// System-role messages become the appended system prompt; the rest are rendered as
/// a labeled transcript (or, for a single turn, just that message's text).
fn render_prompt(messages: &[ChatMessage]) -> (Option<String>, String) {
    let mut sys = String::new();
    let mut convo: Vec<&ChatMessage> = Vec::new();
    for m in messages {
        if m.role == "system" {
            if !sys.is_empty() {
                sys.push_str("\n\n");
            }
            sys.push_str(&m.content);
        } else {
            convo.push(m);
        }
    }

    let prompt = if convo.len() == 1 {
        convo[0].content.clone()
    } else {
        let mut out = String::new();
        for m in &convo {
            let label = if m.role == "assistant" { "Assistant" } else { "User" };
            out.push_str(label);
            out.push_str(": ");
            out.push_str(&m.content);
            out.push_str("\n\n");
        }
        out.trim_end().to_string()
    };

    (if sys.is_empty() { None } else { Some(sys) }, prompt)
}

/// A normalized event from either CLI's JSON output stream.
enum StreamEvent {
    /// Incremental assistant text to append (claude: a delta; opencode: a full part).
    Text(String),
    /// Terminal outcome. `text` = authoritative final (claude `result`); `None`
    /// means "use the accumulated text" (opencode, whose text lives in `Text`s).
    Final { text: Option<String>, is_error: bool },
    Other,
}

/// Parse one line of claude `stream-json` output. Schema verified against
/// claude 2.1.196: text deltas are `stream_event` →
/// `event.type=="content_block_delta"` → `event.delta.type=="text_delta"` →
/// `event.delta.text`; the terminal `result` event carries the authoritative
/// `result` string plus `is_error`/`subtype`.
fn parse_claude_line(line: &str) -> StreamEvent {
    let line = line.trim();
    if line.is_empty() {
        return StreamEvent::Other;
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return StreamEvent::Other,
    };
    match v.get("type").and_then(Value::as_str) {
        Some("stream_event") => {
            let ev = &v["event"];
            if ev.get("type").and_then(Value::as_str) == Some("content_block_delta") {
                let delta = &ev["delta"];
                if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                    if let Some(t) = delta.get("text").and_then(Value::as_str) {
                        return StreamEvent::Text(t.to_string());
                    }
                }
            }
            StreamEvent::Other
        }
        Some("result") => {
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                || v.get("subtype").and_then(Value::as_str).map_or(false, |s| s != "success");
            let text = v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            StreamEvent::Final { text: Some(text), is_error }
        }
        _ => StreamEvent::Other,
    }
}

/// Parse one line of opencode `run --format json` output. Schema verified against
/// the installed CLI: `{"type":"text","part":{"type":"text","text":"…"}}` carries
/// the (full) assistant text part; `{"type":"error",…}` signals failure. Other
/// events (`step_start`, `step_finish`, tool parts) are ignored; success is the
/// accumulated text at EOF. Note: `--format json` is not token-streamed — the full
/// reply arrives in one text event.
fn parse_opencode_line(line: &str) -> StreamEvent {
    let line = line.trim();
    if line.is_empty() {
        return StreamEvent::Other;
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return StreamEvent::Other,
    };
    match v.get("type").and_then(Value::as_str) {
        Some("text") => {
            let t = v["part"].get("text").and_then(Value::as_str).unwrap_or("");
            StreamEvent::Text(t.to_string())
        }
        Some("error") => {
            // Surface whatever message-ish field is present.
            let msg = v
                .get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .map(|m| m.to_string())
                .unwrap_or_else(|| v.to_string());
            StreamEvent::Final { text: Some(msg), is_error: true }
        }
        _ => StreamEvent::Other,
    }
}

/// Kill the child and its whole process group (these CLIs spawn their own children).
async fn kill_tree(pid: Option<i32>, child: &mut tokio::process::Child) {
    if let Some(pid) = pid {
        // We launched with process_group(0), so the child leads group `pid`.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

async fn read_stderr(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt;
    if let Some(mut err) = child.stderr.take() {
        let mut buf = String::new();
        let _ = err.read_to_string(&mut buf).await;
        buf.trim().chars().take(500).collect()
    } else {
        String::new()
    }
}

// ── Worktree isolation ───────────────────────────────────────────────────────

static WORKTREE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Is `dir` inside a git work tree?
async fn is_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A per-job git worktree. `finalize` commits any changes to the branch and removes
/// the worktree; `Drop` force-removes it as a safety net on cancel/panic.
struct WorktreeGuard {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    base_rev: String,
    armed: bool,
}

impl WorktreeGuard {
    fn create(repo: &Path, policy: &WorktreePolicy, room: &str) -> Result<Self> {
        let id = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
        let safe = room.replace(['!', ':', '/', ' '], "_");
        let branch = format!("{}/{}/{}", policy.branch_prefix, safe, id);
        let path = policy.base_dir.join(format!("{}-{}", safe, id));
        std::fs::create_dir_all(&policy.base_dir).ok();
        // Base revision the worktree branches from, so finalize can tell whether
        // anything changed even if the agent made its own commits.
        let base = std::process::Command::new("git")
            .arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output()
            .map_err(|e| anyhow!("git rev-parse: {}", e))?;
        let base_rev = String::from_utf8_lossy(&base.stdout).trim().to_string();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "add", "-b", &branch])
            .arg(&path)
            .arg("HEAD")
            .output()
            .map_err(|e| anyhow!("git worktree add: {}", e))?;
        if !out.status.success() {
            return Err(anyhow!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        info!(branch = %branch, path = %path.display(), "created worktree");
        Ok(WorktreeGuard { repo: repo.to_path_buf(), path, branch, base_rev, armed: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Commit anything the agent left uncommitted, then decide (vs the base rev)
    /// whether the branch has real changes: if so keep it and return a note; if not
    /// remove the worktree and delete the empty branch.
    async fn finalize(&mut self) -> Result<Option<String>> {
        run_git(&self.path, &["add", "-A"]).await;
        if !git_ok(&self.path, &["diff", "--cached", "--quiet"]).await {
            run_git(
                &self.path,
                &[
                    "-c", "user.name=roger",
                    "-c", "user.email=roger@localhost",
                    "commit", "-m", &format!("roger: {}", self.branch),
                ],
            )
            .await;
        }
        let head = git_output(&self.path, &["rev-parse", "HEAD"]).await;
        let changed = !head.is_empty() && head != self.base_rev;
        let note = if changed {
            let range = format!("{}..HEAD", self.base_rev);
            let log = git_output(&self.path, &["log", "--oneline", &range]).await;
            let stat = git_output(&self.path, &["diff", "--stat", &range]).await;
            let body = log
                .lines()
                .chain(stat.lines())
                .take(10)
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!("branch `{}`\n{}", self.branch, body))
        } else {
            None
        };
        let path = self.path.to_string_lossy();
        run_git(&self.repo, &["worktree", "remove", "--force", path.as_ref()]).await;
        if !changed {
            run_git(&self.repo, &["branch", "-D", &self.branch]).await;
        }
        self.armed = false;
        Ok(note)
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort synchronous cleanup if we were cancelled before finalize.
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path)
                .output();
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "prune"])
                .output();
        }
    }
}

async fn run_git(dir: &Path, args: &[&str]) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

async fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn git_output(dir: &Path, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#;
        match parse_claude_line(line) {
            StreamEvent::Text(t) => assert_eq!(t, "hi"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn parse_result_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"final answer"}"#;
        match parse_claude_line(line) {
            StreamEvent::Final { text, is_error } => {
                assert_eq!(text.as_deref(), Some("final answer"));
                assert!(!is_error);
            }
            _ => panic!("expected final"),
        }
    }

    #[test]
    fn parse_result_error() {
        let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"hit limit"}"#;
        match parse_claude_line(line) {
            StreamEvent::Final { is_error, .. } => assert!(is_error),
            _ => panic!("expected final"),
        }
    }

    #[test]
    fn ignores_other_events() {
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"x"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
            "not json",
            "",
        ] {
            assert!(matches!(parse_claude_line(line), StreamEvent::Other));
        }
    }

    #[test]
    fn parse_opencode_text() {
        // Verified shape from `opencode run --format json`.
        let line = r#"{"type":"text","timestamp":1,"sessionID":"s","part":{"id":"p","messageID":"m","sessionID":"s","type":"text","text":"ok"}}"#;
        match parse_opencode_line(line) {
            StreamEvent::Text(t) => assert_eq!(t, "ok"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn parse_opencode_ignores_steps() {
        for line in [
            r#"{"type":"step_start","part":{"type":"step-start"}}"#,
            r#"{"type":"step_finish","part":{"type":"step-finish","reason":"stop"}}"#,
            "not json",
        ] {
            assert!(matches!(parse_opencode_line(line), StreamEvent::Other));
        }
    }

    #[test]
    fn parse_opencode_error() {
        let line = r#"{"type":"error","error":{"message":"boom"}}"#;
        match parse_opencode_line(line) {
            StreamEvent::Final { is_error, .. } => assert!(is_error),
            _ => panic!("expected final error"),
        }
    }

    #[test]
    fn render_prompt_single_turn() {
        let msgs = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("hello"),
        ];
        let (sys, prompt) = render_prompt(&msgs);
        assert_eq!(sys.as_deref(), Some("be terse"));
        assert_eq!(prompt, "hello");
    }

    // Real spawn through the full backend path (OAuth, cheap model). Run with:
    //   cargo test --release claude_subprocess_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "spawns a real claude subprocess via the logged-in session"]
    async fn claude_subprocess_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let b = SubprocessBackend::new(
            SubprocessKind::ClaudeCode,
            "haiku".into(),
            String::new(), // no gateway → use the CLI's own auth
            None,
            Some(dir.path().to_path_buf()),
            Vec::new(),
            "acceptEdits".into(),
            ProcLimits {
                idle: Duration::from_secs(90),
                ceiling: Duration::from_secs(120),
                max_budget_usd: None,
                max_turns: Some(1),
            },
            WorktreePolicy { enabled: false, base_dir: PathBuf::from("/tmp"), branch_prefix: "roger".into() },
        );
        let msgs = vec![
            ChatMessage::system("Be terse."),
            ChatMessage::user("Reply with exactly the single word: ok"),
        ];
        let out = b.chat(&msgs).await.expect("claude run should succeed");
        assert!(!out.trim().is_empty(), "expected non-empty result, got {:?}", out);
    }

    // Real spawn through the opencode backend (free hosted model). Run with:
    //   cargo test --release opencode_subprocess_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "spawns a real opencode subprocess (free hosted model)"]
    async fn opencode_subprocess_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let b = SubprocessBackend::new(
            SubprocessKind::OpenCode,
            "opencode/deepseek-v4-flash-free".into(),
            String::new(),
            None,
            Some(dir.path().to_path_buf()),
            Vec::new(),
            "acceptEdits".into(),
            ProcLimits {
                idle: Duration::from_secs(120),
                ceiling: Duration::from_secs(180),
                max_budget_usd: None,
                max_turns: None,
            },
            WorktreePolicy { enabled: false, base_dir: PathBuf::from("/tmp"), branch_prefix: "roger".into() },
        );
        let msgs = vec![ChatMessage::user("Reply with exactly the single word: ok")];
        let out = b.chat(&msgs).await.expect("opencode run should succeed");
        assert!(!out.trim().is_empty(), "expected non-empty result, got {:?}", out);
    }

    #[test]
    fn render_prompt_transcript() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("first"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("second"),
        ];
        let (_sys, prompt) = render_prompt(&msgs);
        assert!(prompt.contains("User: first"));
        assert!(prompt.contains("Assistant: reply"));
        assert!(prompt.contains("User: second"));
    }

    fn git_init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(p).args(args).output().unwrap();
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(p.join("README.md"), "hi").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        dir
    }

    fn branch_list(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C").arg(repo).args(["branch", "--list"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[tokio::test]
    async fn is_git_repo_detects_repos() {
        let repo = git_init_repo();
        assert!(is_git_repo(repo.path()).await);
        let plain = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(plain.path()).await);
    }

    #[tokio::test]
    async fn worktree_commits_changes_and_keeps_branch() {
        let repo = git_init_repo();
        let base = tempfile::tempdir().unwrap();
        let policy = WorktreePolicy {
            enabled: true,
            base_dir: base.path().to_path_buf(),
            branch_prefix: "roger".into(),
        };
        let mut g = WorktreeGuard::create(repo.path(), &policy, "!room:s").unwrap();
        let wt = g.path().to_path_buf();
        assert!(wt.is_dir());
        std::fs::write(wt.join("new.txt"), "content").unwrap();

        let note = g.finalize().await.unwrap();
        assert!(note.unwrap().contains("roger/_room_s/"));
        assert!(!wt.exists(), "worktree dir should be removed");
        assert!(branch_list(repo.path()).contains("roger/_room_s/"), "branch should remain");
    }

    #[tokio::test]
    async fn worktree_keeps_agents_own_commits() {
        // The agent may commit inside the worktree itself; finalize must detect that
        // (vs the base rev) and keep the branch, not delete it as "unchanged".
        let repo = git_init_repo();
        let base = tempfile::tempdir().unwrap();
        let policy = WorktreePolicy {
            enabled: true,
            base_dir: base.path().to_path_buf(),
            branch_prefix: "roger".into(),
        };
        let mut g = WorktreeGuard::create(repo.path(), &policy, "!c:s").unwrap();
        let wt = g.path().to_path_buf();
        std::fs::write(wt.join("a.txt"), "x").unwrap();
        for args in [
            ["add", "-A"].as_slice(),
            ["-c", "user.name=a", "-c", "user.email=a@a", "commit", "-qm", "agent"].as_slice(),
        ] {
            std::process::Command::new("git").arg("-C").arg(&wt).args(args).output().unwrap();
        }
        let note = g.finalize().await.unwrap();
        assert!(note.is_some(), "agent's own commit must be kept");
        assert!(branch_list(repo.path()).contains("roger/_c_s/"));
    }

    #[tokio::test]
    async fn worktree_no_changes_removes_branch() {
        let repo = git_init_repo();
        let base = tempfile::tempdir().unwrap();
        let policy = WorktreePolicy {
            enabled: true,
            base_dir: base.path().to_path_buf(),
            branch_prefix: "roger".into(),
        };
        let mut g = WorktreeGuard::create(repo.path(), &policy, "!empty:s").unwrap();
        let wt = g.path().to_path_buf();

        let note = g.finalize().await.unwrap();
        assert!(note.is_none(), "no changes → no note");
        assert!(!wt.exists());
        assert!(!branch_list(repo.path()).contains("roger/_empty_s/"), "empty branch should be deleted");
    }
}
