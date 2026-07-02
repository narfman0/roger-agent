//! Thin HTTP control panel for Roger.
//!
//! Enabled via `[web] enabled = true` in profiles.toml.
//! Routes: GET / (HTML dashboard), GET /api/status, GET /api/rooms,
//!         GET /api/cmds, POST /api/send.

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Router,
    extract::{Json, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use matrix_sdk::Client as MatrixClient;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::matrix::handler::BotCtx;

#[derive(Clone)]
pub struct WebState {
    pub bot: BotCtx,
    pub client: MatrixClient,
    pub started_at: Arc<Instant>,
    pub auth_token: String,
}

/// Check bearer token if one is configured; pass through if none.
fn check_auth(state: &WebState, headers: &HeaderMap) -> bool {
    if state.auth_token.is_empty() {
        return true;
    }
    let expected = format!("Bearer {}", state.auth_token);
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false)
}

fn auth_err() -> Response {
    (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
}

// ── /api/status ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    active_jobs: usize,
    uptime_secs: u64,
    bot_user_id: String,
    profiles: Vec<String>,
}

async fn api_status(State(ws): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if !check_auth(&ws, &headers) { return auth_err(); }
    let active_jobs = ws.bot.workers.count();
    let uptime_secs = ws.started_at.elapsed().as_secs();
    let bot_user_id = ws.bot.bot_user_id.clone();
    let profiles = {
        let st = ws.bot.state.read().await;
        let mut names: Vec<String> = st.llms.keys().cloned().collect();
        names.sort();
        names
    };
    Json(StatusResponse { active_jobs, uptime_secs, bot_user_id, profiles }).into_response()
}

// ── /api/rooms ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RoomInfo {
    id: String,
    name: String,
    profile: String,
    require_mention: bool,
}

async fn api_rooms(State(ws): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if !check_auth(&ws, &headers) { return auth_err(); }
    let st = ws.bot.state.read().await;
    let mut rooms: Vec<RoomInfo> = st
        .room_configs
        .iter()
        .map(|(id, cfg)| RoomInfo {
            id: id.clone(),
            name: cfg.name.clone(),
            profile: st.profile_name_for_room(id),
            require_mention: cfg.require_mention,
        })
        .collect();
    rooms.sort_by(|a, b| a.name.cmp(&b.name));
    Json(rooms).into_response()
}

// ── /api/cmds ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct CmdInfo {
    name: &'static str,
    args: &'static str,
    description: &'static str,
    category: &'static str,
}

const COMMANDS: &[CmdInfo] = &[
    CmdInfo { name: "/help",            args: "",                description: "Show this command reference",                     category: "Help" },
    CmdInfo { name: "/status",          args: "",                description: "Show active jobs, profile, memory sizes",         category: "Info" },
    CmdInfo { name: "/jobs",            args: "",                description: "List background jobs with room and model",        category: "Info" },
    CmdInfo { name: "/model",           args: "<profile>",       description: "Switch LLM profile for this room (persisted)",   category: "Config" },
    CmdInfo { name: "/cancel",          args: "<job-id>",        description: "Abort a background job",                         category: "Jobs" },
    CmdInfo { name: "/clear",           args: "",                description: "Drop this room's conversation history",          category: "Memory" },
    CmdInfo { name: "/forget",          args: "",                description: "Wipe durable memory for this room",              category: "Memory" },
    CmdInfo { name: "/agents",          args: "",                description: "List configured subagents",                      category: "Agents" },
    CmdInfo { name: "/agent",           args: "<name> <task>",   description: "Run a named subagent manually",                  category: "Agents" },
    CmdInfo { name: "/skills",          args: "",                description: "List active + pending skills",                   category: "Skills" },
    CmdInfo { name: "/skills suggest",  args: "",                description: "Ask Roger to draft a skill from recent history", category: "Skills" },
    CmdInfo { name: "/skills approve",  args: "<name>",          description: "Promote a pending skill to active",              category: "Skills" },
    CmdInfo { name: "/skills forget",   args: "<name>",          description: "Remove a learned or pending skill",              category: "Skills" },
];

async fn api_cmds(State(ws): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if !check_auth(&ws, &headers) { return auth_err(); }
    Json(COMMANDS).into_response()
}

// ── POST /api/send ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendRequest {
    room_id: String,
    message: String,
}

#[derive(Serialize)]
struct SendResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn api_send(
    State(ws): State<Arc<WebState>>,
    headers: HeaderMap,
    Json(req): Json<SendRequest>,
) -> Response {
    if !check_auth(&ws, &headers) { return auth_err(); }
    if req.message.trim().is_empty() {
        return Json(SendResponse { ok: false, error: Some("message is empty".into()) }).into_response();
    }
    match ws.client.get_room(
        <&matrix_sdk::ruma::RoomId>::try_from(req.room_id.as_str())
            .map_err(|e| e.to_string())
            .and_then(|id| Ok(id))
            .unwrap_or_else(|_| {
                // fallback: parse will fail and get_room returns None
                // we handle None below
                return <&matrix_sdk::ruma::RoomId>::try_from("!invalid:local").unwrap();
            })
    ) {
        Some(room) => {
            match room.send(RoomMessageEventContent::text_plain(&req.message)).await {
                Ok(_) => Json(SendResponse { ok: true, error: None }).into_response(),
                Err(e) => Json(SendResponse { ok: false, error: Some(e.to_string()) }).into_response(),
            }
        }
        None => Json(SendResponse { ok: false, error: Some(format!("room not found: {}", req.room_id)) }).into_response(),
    }
}

// ── GET / (HTML dashboard) ───────────────────────────────────────────────────

async fn index(State(ws): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if !check_auth(&ws, &headers) { return auth_err(); }

    // Gather rooms for the send form
    let rooms = {
        let st = ws.bot.state.read().await;
        let mut r: Vec<(String, String)> = st.room_configs.iter()
            .map(|(id, cfg)| (id.clone(), cfg.name.clone()))
            .collect();
        r.sort_by(|a, b| a.1.cmp(&b.1));
        r
    };

    let room_options: String = rooms.iter()
        .map(|(id, name)| format!("<option value=\"{}\">{}</option>", id, name))
        .collect::<Vec<_>>()
        .join("\n");

    // Build command table rows grouped by category
    let mut cats: Vec<&str> = Vec::new();
    for cmd in COMMANDS {
        if !cats.contains(&cmd.category) { cats.push(cmd.category); }
    }
    let cmd_rows: String = cats.iter().map(|cat| {
        let rows: String = COMMANDS.iter()
            .filter(|c| c.category == *cat)
            .map(|c| format!(
                "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
                c.name, c.args, c.description
            ))
            .collect::<Vec<_>>()
            .join("\n");
        format!("<tr><th colspan=\"3\" class=\"cat\">{}</th></tr>\n{}", cat, rows)
    }).collect::<Vec<_>>().join("\n");

    let bot_id = &ws.bot.bot_user_id;
    let uptime = ws.started_at.elapsed().as_secs();
    let jobs = ws.bot.workers.count();

    let html = format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Roger</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{background:#1a1a1a;color:#e0e0e0;font:14px/1.5 'Courier New',monospace;padding:1.5rem}}
h1{{color:#7eb8f7;margin-bottom:0.25rem}}
.meta{{color:#888;font-size:12px;margin-bottom:1.5rem}}
h2{{color:#a8d8a8;margin:1.5rem 0 0.5rem}}
table{{width:100%;border-collapse:collapse;margin-bottom:1rem}}
th,td{{padding:0.35rem 0.6rem;text-align:left;border-bottom:1px solid #333}}
th.cat{{background:#2a2a2a;color:#f0b840;font-size:12px;letter-spacing:0.05em;text-transform:uppercase}}
code{{color:#f7c948;background:#2a2a2a;padding:0.1em 0.3em;border-radius:3px;font-size:13px}}
.send-form{{background:#222;border:1px solid #333;border-radius:6px;padding:1rem;max-width:600px}}
.send-form label{{display:block;margin-bottom:0.25rem;color:#aaa;font-size:12px}}
.send-form select,.send-form textarea,.send-form button{{width:100%;padding:0.5rem;background:#1a1a1a;color:#e0e0e0;border:1px solid #444;border-radius:4px;font:inherit;margin-bottom:0.75rem}}
.send-form textarea{{height:80px;resize:vertical}}
.send-form button{{background:#2a4a7f;color:#fff;cursor:pointer;border:none}}
.send-form button:hover{{background:#3a5a9f}}
#send-result{{font-size:12px;color:#aaa;margin-top:0.25rem}}
</style>
</head>
<body>
<h1>Roger</h1>
<div class="meta">
  {bot_id} &nbsp;|&nbsp;
  uptime {uptime}s &nbsp;|&nbsp;
  {jobs} active job(s) &nbsp;|&nbsp;
  <a href="/api/status" style="color:#888">status JSON</a>
</div>

<h2>Send a Message</h2>
<div class="send-form">
  <label>Room</label>
  <select id="room">{room_options}</select>
  <label>Message</label>
  <textarea id="msg" placeholder="Type a message…"></textarea>
  <button onclick="sendMsg()">Send</button>
  <div id="send-result"></div>
</div>

<h2>Commands</h2>
<table>
<thead><tr><th>Command</th><th>Args</th><th>Description</th></tr></thead>
<tbody>
{cmd_rows}
</tbody>
</table>

<script>
async function sendMsg() {{
  const room_id = document.getElementById('room').value;
  const message = document.getElementById('msg').value.trim();
  const res = document.getElementById('send-result');
  if (!message) {{ res.textContent = 'Message is empty.'; return; }}
  res.textContent = 'Sending…';
  try {{
    const r = await fetch('/api/send', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{room_id, message}})
    }});
    const j = await r.json();
    res.textContent = j.ok ? 'Sent.' : ('Error: ' + (j.error || 'unknown'));
    if (j.ok) document.getElementById('msg').value = '';
  }} catch(e) {{
    res.textContent = 'Network error: ' + e;
  }}
}}
</script>
</body>
</html>"#);

    Html(html).into_response()
}

// ── Server startup ────────────────────────────────────────────────────────────

pub async fn start(bot: BotCtx, client: MatrixClient, bind: String, auth_token: String) {
    let ws = Arc::new(WebState {
        started_at: bot.started_at.clone(),
        bot,
        client,
        auth_token,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/rooms", get(api_rooms))
        .route("/api/cmds", get(api_cmds))
        .route("/api/send", post(api_send))
        .with_state(ws);

    match TcpListener::bind(&bind).await {
        Ok(listener) => {
            info!("web UI listening on http://{}", bind);
            if let Err(e) = axum::serve(listener, app).await {
                warn!("web server error: {}", e);
            }
        }
        Err(e) => warn!("failed to bind web server to {}: {}", bind, e),
    }
}
