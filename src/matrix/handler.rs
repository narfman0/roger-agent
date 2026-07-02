use crate::{
    audio::SpeachesClient,
    config::{AgentConfig, CommsConfig, CommsMode, CompactionConfig, RoomConfig},
    history::{ChatMessage, HistoryStore},
    llm::ProfileLlm,
    memory::MemoryStore,
    metrics::Metrics,
    room_profiles::RoomProfileStore,
    room_workdirs::RoomWorkdirStore,
    skills::SkillStore,
    subprocess::WORKDIR,
    tools::{SubagentHost, ToolExecutor, ROOM_ID, SUBAGENT},
    workers::{JobHandle, Workers},
};
use matrix_sdk::{
    event_handler::Ctx,
    media::{MediaFormat, MediaRequestParameters},
    room::Room,
    ruma::{
        events::room::{
            member::{MembershipState, StrippedRoomMemberEvent},
            message::{
                AudioMessageEventContent, MessageType, OriginalSyncRoomMessageEvent,
                ReplacementMetadata, RoomMessageEventContent,
            },
        },
        OwnedEventId,
    },
    Client,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio::time::sleep;
use tracing::{info, warn};

/// Streaming flush cadence, derived from `CommsConfig::edit_debounce_ms`. We
/// post/update the response when a sentence completes OR `max_wait` has passed
/// since the last update (whichever first), but never more often than `min_gap`.
/// The typing indicator covers the gap before the first flush — no placeholder
/// message in any mode.
#[derive(Clone, Copy)]
struct FlushCadence {
    min_gap: Duration,
    max_wait: Duration,
}

impl FlushCadence {
    fn from_comms(c: &CommsConfig) -> Self {
        let min_gap = Duration::from_millis(c.edit_debounce_ms);
        // Force a flush at least this often even without a sentence boundary.
        let max_wait = Duration::from_millis(c.edit_debounce_ms.max(1000));
        FlushCadence { min_gap, max_wait }
    }
}

/// Byte index of the last sentence-ending boundary in `s` (`.`, `!`, `?`, or a
/// newline), if any.
fn last_sentence_end(s: &str) -> Option<usize> {
    s.rfind(|c: char| matches!(c, '.' | '!' | '?' | '\n'))
}

/// Decide whether to flush a streamed update. Flush when the text changed, the
/// rate floor has passed, and either a new sentence boundary appeared or the time
/// ceiling was reached.
fn should_flush(
    changed: bool,
    elapsed: Duration,
    sentence_ready: bool,
    min_gap: Duration,
    max_wait: Duration,
) -> bool {
    changed && elapsed >= min_gap && (sentence_ready || elapsed >= max_wait)
}

/// Read a small context file (operating instructions), trimmed; empty if missing.
/// `~` is expanded; relative paths resolve against the process working directory.
fn read_context_file(path: &str) -> String {
    let expanded = crate::config::expand_tilde(path);
    std::fs::read_to_string(&expanded)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Layer operating instructions and durable memory onto the base persona to form
/// the full system prompt. Empty sections are skipped, so absent files add nothing.
#[allow(clippy::too_many_arguments)]
fn assemble_system_prompt(
    base: &str,
    operating_global: &str,
    operating_room: &str,
    memory_global: &str,
    memory_room: &str,
    skills: &str,
) -> String {
    let mut out = base.trim_end().to_string();
    let mut section = |title: Option<&str>, body: &str| {
        if body.is_empty() {
            return;
        }
        out.push_str("\n\n");
        if let Some(t) = title {
            out.push_str(t);
            out.push('\n');
        }
        out.push_str(body);
    };
    section(None, operating_global);
    section(None, operating_room);
    section(Some("## Memory (global)"), memory_global);
    section(Some("## Memory (this room)"), memory_room);
    section(
        Some("## Skills\nReusable procedures — call `read_skill(name)` to load one:"),
        skills,
    );
    out
}

/// Edit a previously sent message in place via an `m.replace` relation.
async fn edit_message(room: &Room, id: OwnedEventId, text: &str) {
    let edited = RoomMessageEventContent::text_plain(text)
        .make_replacement(ReplacementMetadata::new(id, None), None);
    if let Err(e) = room.send(edited).await {
        warn!("failed to edit message: {}", e);
    }
}

/// Config that can be swapped at runtime on SIGHUP without restarting the bot.
/// Everything reachable behind `BotCtx::state` is reloadable; fields directly on
/// `BotCtx` are fixed for the process lifetime.
pub struct ReloadableState {
    /// LLM (primary + fallbacks) per profile name (e.g. "chat", "reason", "code").
    pub llms: HashMap<String, Arc<ProfileLlm>>,
    pub system_prompt: String,
    pub room_configs: HashMap<String, RoomConfig>,
    /// Runtime per-room profile overrides set via `/model`, keyed by room id.
    pub room_profiles: HashMap<String, String>,
    /// Global comms budgets (sync_budget_ms, edit_debounce_ms, …).
    pub comms: CommsConfig,
    /// Per-profile comms mode (sync / async / auto), keyed by profile name.
    pub profile_comms: HashMap<String, CommsMode>,
    /// Global operating-instructions file, layered into every system prompt.
    pub operating_file: Option<String>,
    /// Whether durable-memory injection is enabled.
    pub memory_enabled: bool,
    /// Memory self-compaction caps (tokens).
    pub memory_max_global_tokens: usize,
    pub memory_max_room_tokens: usize,
    /// Size-triggered history compaction settings.
    pub compaction: CompactionConfig,
    /// Named subagents (for run_subagent / /agent).
    pub agents: HashMap<String, AgentConfig>,
}

impl ReloadableState {
    /// The configured profile name for a room: runtime `/model` override, else
    /// the room's `profile` config, else "chat".
    pub fn profile_name_for_room(&self, room_id: &str) -> String {
        if let Some(p) = self.room_profiles.get(room_id) {
            return p.clone();
        }
        self.room_configs
            .get(room_id)
            .and_then(|r| r.profile.clone())
            .unwrap_or_else(|| "chat".to_string())
    }

    /// Comms mode for a profile (sync / async / auto); defaults to sync for an
    /// unknown profile.
    pub fn comms_mode_for_profile(&self, profile: &str) -> CommsMode {
        self.profile_comms
            .get(profile)
            .cloned()
            .unwrap_or(CommsMode::Sync)
    }

    /// Resolve the LLM for a room. Returns the profile LLM and the profile name
    /// actually used (falls back to "chat" if the requested profile has no built
    /// client).
    pub fn llm_for_room(&self, room_id: &str) -> (Arc<ProfileLlm>, String) {
        let requested = self.profile_name_for_room(room_id);
        if let Some(c) = self.llms.get(&requested) {
            return (c.clone(), requested);
        }
        let chat = self.llms.get("chat").expect("chat client always present");
        (chat.clone(), "chat".to_string())
    }
}

#[derive(Clone)]
pub struct BotCtx {
    pub allowed_rooms: std::collections::HashSet<String>,
    pub bot_user_id: String,
    pub bot_localpart: String,
    pub speaches: Option<Arc<SpeachesClient>>,
    pub history: Arc<HistoryStore>,
    pub started_at: Arc<Instant>,
    pub state: Arc<RwLock<ReloadableState>>,
    /// Persists `/model` runtime overrides across restarts.
    pub room_profiles: Arc<RoomProfileStore>,
    /// Process-lifetime response counters.
    pub metrics: Arc<Metrics>,
    /// Tool executor for web_search / web_fetch / set_workdir.
    pub tool_executor: Arc<ToolExecutor>,
    /// Background-job registry (sync/async/auto response tasks).
    pub workers: Arc<Workers>,
    /// Per-room agentic workdir selections (set via set_workdir), persisted.
    pub room_workdirs: Arc<RoomWorkdirStore>,
    /// Durable memory files (global + per-room), injected into the system prompt.
    pub memory: Arc<MemoryStore>,
    /// Per-room FIFO workers that serialize turns within each room.
    pub rooms: Arc<RoomQueues>,
    /// Reusable skills (read/write/index), injected + editable.
    pub skills: Arc<SkillStore>,
}

impl BotCtx {
    fn is_mentioned(&self, body: &str) -> bool {
        let needle_full = format!("@{}", self.bot_user_id);
        let needle_local = format!("@{}", self.bot_localpart);
        body.contains(&needle_full) || body.contains(&needle_local)
    }
}

pub async fn handle_invite(
    event: StrippedRoomMemberEvent,
    room: Room,
    client: Client,
    ctx: Ctx<BotCtx>,
) {
    let room_id = room.room_id().to_string();

    if event.state_key != client.user_id().map(|u| u.to_string()).unwrap_or_default() {
        return;
    }
    if event.content.membership != MembershipState::Invite {
        return;
    }

    if ctx.allowed_rooms.contains(&room_id) {
        info!("accepting invite to allowed room {}", room_id);
        if let Err(e) = room.join().await {
            warn!("failed to join room {}: {}", room_id, e);
        } else {
            // Advertise bot commands to clients that support m.room.bot.options
            // (Fractal, some others — Element Web uses its own command registry).
            publish_bot_options(&room).await;
        }
    } else {
        warn!("declining invite to non-allowlisted room {}", room_id);
    }
}

/// Send an `m.room.bot.options` state event advertising Roger's slash commands.
/// This is a best-effort hint; clients that don't support it ignore the event.
pub async fn publish_bot_options(room: &Room) {
    let commands = serde_json::json!({
        "help":           {"description": "Show command reference"},
        "status":         {"description": "Uptime, model, history stats, active jobs"},
        "jobs":           {"description": "List background jobs"},
        "model":          {"description": "Show or switch LLM profile for this room"},
        "cancel":         {"description": "Abort a background job (/cancel <id>)"},
        "clear":          {"description": "Wipe conversation history for this room"},
        "forget":         {"description": "Wipe durable memory (/forget global for shared)"},
        "agents":         {"description": "List configured subagents"},
        "agent":          {"description": "Run a subagent (/agent <name> <task>)"},
        "skills":         {"description": "List active + pending skills"},
        "skills suggest": {"description": "Draft a skill from recent history"},
        "skills approve": {"description": "Promote a pending skill (/skills approve <name>)"},
        "skills forget":  {"description": "Remove a skill (/skills forget <name>)"}
    });
    let content = serde_json::json!({
        "prefix": "/",
        "commands": commands
    });
    if let Err(e) = room.send_state_event_raw("m.room.bot.options", "", content).await {
        warn!("failed to publish m.room.bot.options: {}", e);
    }
}

/// One queued user turn awaiting its room's serial worker.
struct Turn {
    room: Room,
    sender: String,
    body: String,
}

/// Per-room FIFO workers. Each room has a single spawned worker that processes its
/// turns in arrival order: sync work holds the room until it completes; async /
/// auto-promoted work detaches and lets the room advance to the next turn. Control
/// slash commands bypass this queue (handled directly in `handle_message`).
#[derive(Default)]
pub struct RoomQueues {
    senders: std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<Turn>>>,
}

impl RoomQueues {
    fn enqueue(&self, ctx: &BotCtx, room_id: String, turn: Turn) {
        let mut map = self.senders.lock().unwrap();
        let tx = map.entry(room_id.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(room_worker(rx, ctx.clone()));
            tx
        });
        let _ = tx.send(turn);
    }
}

/// A room's serial worker: pulls turns in order and processes them one at a time.
async fn room_worker(mut rx: mpsc::UnboundedReceiver<Turn>, ctx: BotCtx) {
    // One "agentic slot" per room: an agentic turn waits here until any running
    // agentic job in the room finishes (FIFO), while a backgrounded job holds the
    // permit for its lifetime.
    let agentic_gate = Arc::new(Semaphore::new(1));
    while let Some(turn) = rx.recv().await {
        process_turn(&ctx, &agentic_gate, turn).await;
    }
}

pub async fn handle_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    ctx: Ctx<BotCtx>,
) {
    let room_id = room.room_id().to_string();

    if !ctx.allowed_rooms.contains(&room_id) {
        return;
    }
    if event.sender.to_string() == ctx.bot_user_id {
        return;
    }

    // Resolve message body — text directly, audio via transcription.
    let body = match &event.content.msgtype {
        MessageType::Text(text) => text.body.clone(),

        MessageType::Audio(audio) => {
            match transcribe_audio(&client, audio, &ctx).await {
                Ok(transcript) => {
                    info!(room = %room_id, "transcribed audio: {}", transcript);
                    transcript
                }
                Err(e) => {
                    warn!("audio transcription failed: {}", e);
                    let _ = room
                        .send(RoomMessageEventContent::text_plain(
                            "Sorry, I couldn't transcribe that audio.",
                        ))
                        .await;
                    return;
                }
            }
        }

        _ => return,
    };

    // Control slash commands bypass the room queue (so `/cancel`, `/status`, etc.
    // respond immediately during a running job) and the mention gate.
    if body.trim_start().starts_with('/') {
        if let Some(cmd_reply) = handle_slash_command(&body, &room_id, &ctx).await {
            if let Err(e) = room.send(RoomMessageEventContent::text_plain(&cmd_reply)).await {
                warn!("failed to send command reply: {}", e);
            }
        }
        return;
    }

    // Everything else goes onto the room's serial worker, preserving order.
    let bot = &*ctx;
    bot.rooms.enqueue(
        bot,
        room_id,
        Turn { room, sender: event.sender.to_string(), body },
    );
}

/// Process one queued turn for a room: mention gate, persist, assemble context, and
/// dispatch the response job holding vs. releasing the room per the comms mode.
async fn process_turn(ctx: &BotCtx, agentic_gate: &Arc<Semaphore>, turn: Turn) {
    let Turn { room, sender, body } = turn;
    let room_id = room.room_id().to_string();

    // Mention gate — read the (reloadable) per-room config.
    let require_mention = ctx
        .state
        .read()
        .await
        .room_configs
        .get(&room_id)
        .map(|r| r.require_mention)
        .unwrap_or(true);
    if require_mention && !ctx.is_mentioned(&body) {
        return;
    }

    info!(room = %room_id, sender = %sender, "processing: {}", body);

    // Persist the user turn, then snapshot the reloadable bits and drop the read
    // lock before spawning the (possibly long) response job so SIGHUP reloads
    // aren't blocked.
    if let Err(e) = ctx.history.append(&room_id, ChatMessage::user(&body)) {
        warn!("failed to save user message to history: {}", e);
    }
    let (llm, base_prompt, profile, model, comms_mode, comms_cfg, op_global, op_room, mem_enabled) = {
        let st = ctx.state.read().await;
        let rc = st.room_configs.get(&room_id);
        let prompt = rc
            .and_then(|r| r.system_prompt.clone())
            .unwrap_or_else(|| st.system_prompt.clone());
        let op_room = rc.and_then(|r| r.operating_file.clone());
        let op_global = st.operating_file.clone();
        let (client, profile) = st.llm_for_room(&room_id);
        let model = client.model().to_string();
        let mode = st.comms_mode_for_profile(&profile);
        (client, prompt, profile, model, mode, st.comms.clone(), op_global, op_room, st.memory_enabled)
    };

    // Assemble the full system prompt: base persona + operating instructions
    // (global + per-room) + durable memory (global + per-room). All layered files
    // are read fresh each turn, so edits take effect without a reload.
    let operating_global = op_global.as_deref().map(read_context_file).unwrap_or_default();
    let operating_room = op_room.as_deref().map(read_context_file).unwrap_or_default();
    let (memory_global, memory_room) = if mem_enabled {
        (ctx.memory.read_global(), ctx.memory.read_room(&room_id))
    } else {
        (String::new(), String::new())
    };
    let skills_index = ctx
        .skills
        .index()
        .into_iter()
        .map(|(n, d)| if d.is_empty() { format!("- {}", n) } else { format!("- {}: {}", n, d) })
        .collect::<Vec<_>>()
        .join("\n");
    let system_prompt = assemble_system_prompt(
        &base_prompt,
        &operating_global,
        &operating_room,
        &memory_global,
        &memory_room,
        &skills_index,
    );

    let agentic = llm.is_subprocess();
    let budget = llm.history_token_budget(crate::history::estimate_tokens(&system_prompt));
    let mut messages = vec![ChatMessage::system(&system_prompt)];
    messages.extend(ctx.history.windowed_by_tokens(&room_id, budget));

    // Resolve the agentic workdir for this room: the set_workdir selection, else
    // the configured default. Passed to the subprocess via a task-local.
    let workdir: Option<PathBuf> = ctx
        .room_workdirs
        .get(&room_id)
        .map(PathBuf::from)
        .or_else(|| comms_cfg.default_workdir.as_deref().map(crate::config::expand_tilde));

    let flush = FlushCadence::from_comms(&comms_cfg);

    // Agentic serialization: an agentic turn waits here (holding the room) until the
    // room's agentic slot is free — i.e. behind any still-running agentic job. The
    // permit is moved into the job and held for its whole lifetime, so a backgrounded
    // (async) job keeps the slot until it finishes and the next agentic turn queues.
    let agentic_permit = if agentic {
        Some(agentic_gate.clone().acquire_owned().await.expect("agentic gate open"))
    } else {
        None
    };

    // The whole response pipeline runs as one self-contained task (produce →
    // stream → fallback → metrics → history → final render). The room worker then
    // awaits it (sync), detaches it (async), or races it against the sync budget and
    // detaches on timeout (auto) — releasing the room to the next turn once detached.
    let job_id = ctx.workers.insert_pending(JobHandle {
        room: room_id.clone(),
        profile: profile.clone(),
        model: model.clone(),
        started: Instant::now(),
        abort: None,
    });
    let workers = ctx.workers.clone();
    let bot = ctx.clone();
    let job = {
        let room = room.clone();
        tokio::spawn(async move {
            let _agentic_permit = agentic_permit; // held for the job's whole lifetime
            run_response_job(room, room_id, messages, llm, bot, profile, model, flush, workdir)
                .await;
            workers.remove(job_id);
        })
    };
    ctx.workers.set_abort(job_id, job.abort_handle());

    match comms_mode {
        // Detached: the job owns its message and lifecycle; the room advances now.
        CommsMode::Async => {}
        CommsMode::Sync => {
            let _ = job.await;
        }
        CommsMode::Auto => {
            // Hold the room up to the sync budget; if it runs long, detach and let
            // the job finish in the background (the room advances to the next turn).
            let sync_budget = Duration::from_millis(comms_cfg.sync_budget_ms);
            tokio::pin!(job);
            tokio::select! {
                _ = &mut job => {}
                _ = sleep(sync_budget) => {}
            }
        }
    }
}

/// The full response pipeline for one turn, run as a single self-contained task so
/// it is correct whether the handler awaits it (sync) or detaches it (async/auto).
/// Produces via the LLM, streams flushes into a Matrix message, falls back to a
/// non-streaming call if needed, records metrics, persists the reply, and renders
/// the final text.
#[allow(clippy::too_many_arguments)]
async fn run_response_job(
    room: Room,
    room_id: String,
    messages: Vec<ChatMessage>,
    llm: Arc<ProfileLlm>,
    bot: BotCtx,
    profile: String,
    model: String,
    flush: FlushCadence,
    workdir: Option<PathBuf>,
) {
    let _ = room.typing_notice(true).await;
    let req_start = Instant::now();

    // Snapshot subagent config so the producer can expose run_subagent.
    let agents = bot.state.read().await.agents.clone();

    let (tx, mut rx) = mpsc::channel::<String>(64);
    let stream_handle = {
        let llm = llm.clone();
        let messages = messages.clone();
        let executor = bot.tool_executor.clone();
        let room_scope = room_id.clone();
        let wd = workdir.clone();
        let bot2 = bot.clone();
        // Task-locals don't cross spawn boundaries, so scope them inside the
        // producer task: ROOM_ID lets set_workdir target this room; WORKDIR gives a
        // subprocess backend the room's workdir; SUBAGENT enables run_subagent.
        tokio::spawn(async move {
            let inner = ROOM_ID.scope(
                room_scope,
                WORKDIR.scope(wd, async move {
                    llm.chat_with_tools(&messages, Some(&executor), tx).await
                }),
            );
            if agents.is_empty() {
                inner.await
            } else {
                let host: Arc<dyn SubagentHost> =
                    Arc::new(SubagentHostImpl { bot: bot2, agents, depth: 0 });
                SUBAGENT.scope(host, inner).await
            }
        })
    };

    // No placeholder: the first content flush posts the message; the typing
    // indicator is the only "working" signal until then.
    let mut msg_id: Option<OwnedEventId> = None;
    let mut shown = String::new();
    let mut last_flush: Option<Instant> = None;
    while let Some(acc) = rx.recv().await {
        let elapsed = last_flush.unwrap_or(req_start).elapsed();
        let sentence_ready = last_sentence_end(&acc).map_or(false, |i| i >= shown.len());
        if !should_flush(acc != shown, elapsed, sentence_ready, flush.min_gap, flush.max_wait) {
            continue;
        }
        match &msg_id {
            None => match room.send(RoomMessageEventContent::text_plain(&acc)).await {
                Ok(resp) => msg_id = Some(resp.event_id),
                Err(e) => {
                    warn!("failed to send first response message: {}", e);
                    continue;
                }
            },
            Some(id) => edit_message(&room, id.clone(), &acc).await,
        }
        shown = acc;
        last_flush = Some(Instant::now());
    }

    let streamed = stream_handle.await;
    let _ = room.typing_notice(false).await;

    // For subprocess (agentic) backends the fallback `llm.chat()` would spawn the
    // CLI again — producing a second response. Suppress the fallback in that case.
    let is_subprocess = llm.is_subprocess();
    let result: anyhow::Result<String> = match streamed {
        Ok(Ok(text)) if !text.trim().is_empty() => Ok(text),
        Ok(Ok(_)) if is_subprocess => Err(anyhow::anyhow!("subprocess produced no output")),
        Ok(Ok(_)) => WORKDIR.scope(workdir.clone(), llm.chat(&messages)).await,
        Ok(Err(e)) if is_subprocess => Err(e),
        Ok(Err(e)) => {
            warn!("stream error, falling back to non-streaming: {}", e);
            WORKDIR.scope(workdir.clone(), llm.chat(&messages)).await
        }
        Err(e) => Err(anyhow::anyhow!("stream task failed: {}", e)),
    };

    let latency_ms = req_start.elapsed().as_millis() as u64;
    let ok = result.is_ok();
    bot.metrics.record(latency_ms, ok);
    info!(room = %room_id, profile = %profile, model = %model, latency_ms, ok, "responded");

    let final_text = match result {
        Ok(reply) => {
            if let Err(e) = bot.history.append(&room_id, ChatMessage::assistant(&reply)) {
                warn!("failed to save assistant reply to history: {}", e);
            }
            reply
        }
        Err(e) => {
            warn!("LLM error: {}", e);
            format!("Sorry, I hit an error: {}", e)
        }
    };

    match msg_id {
        Some(id) => {
            if final_text != shown {
                edit_message(&room, id, &final_text).await;
            }
        }
        None => {
            let text = if final_text.trim().is_empty() {
                "(no response)".to_string()
            } else {
                final_text
            };
            if let Err(e) = room.send(RoomMessageEventContent::text_plain(&text)).await {
                warn!("failed to send response: {}", e);
            }
        }
    }

    // The reply is persisted; compact the room if its history has grown too large.
    maybe_compact(&bot, &room_id).await;
}

/// If the room's history exceeds the compaction threshold, spawn a detached
/// compaction task (summarize old turns + distill memory). Fast to call: it only
/// reads config + the history size, then spawns.
async fn maybe_compact(bot: &BotCtx, room_id: &str) {
    let (cfg, llm, room_budget, max_global, max_room) = {
        let st = bot.state.read().await;
        let cfg = st.compaction.clone();
        // The trigger scales with the room's own model window.
        let (room_llm, _) = st.llm_for_room(room_id);
        let room_budget = room_llm.history_token_budget(0);
        let llm = st.llms.get(&cfg.profile).cloned();
        (cfg, llm, room_budget, st.memory_max_global_tokens, st.memory_max_room_tokens)
    };
    if !cfg.enabled {
        return;
    }
    let trigger = if cfg.trigger_tokens > 0 {
        cfg.trigger_tokens
    } else {
        (room_budget as f32 * cfg.trigger_fraction) as usize
    };
    if bot.history.token_count(room_id) <= trigger {
        return;
    }
    let Some(llm) = llm else {
        warn!("compaction profile '{}' not built; skipping", cfg.profile);
        return;
    };
    tokio::spawn(crate::compaction::compact_room(
        bot.history.clone(),
        bot.memory.clone(),
        llm,
        room_id.to_string(),
        crate::compaction::CompactionParams {
            keep_recent_turns: cfg.keep_recent_turns,
            max_global_tokens: max_global,
            max_room_tokens: max_room,
        },
    ));
}

/// Max nesting for subagents delegating to subagents.
const MAX_SUBAGENT_DEPTH: usize = 2;

/// Runs named subagents. Owns a `BotCtx` (for the LLM registry + tool executor) and
/// a snapshot of the agent config; nested calls increment `depth`.
struct SubagentHostImpl {
    bot: BotCtx,
    agents: HashMap<String, AgentConfig>,
    depth: usize,
}

impl SubagentHost for SubagentHostImpl {
    fn agents(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .agents
            .iter()
            .map(|(n, a)| (n.clone(), a.description.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn run<'a>(&'a self, name: &'a str, task: &'a str) -> futures_util::future::BoxFuture<'a, String> {
        Box::pin(self.run_impl(name, task))
    }
}

impl SubagentHostImpl {
    async fn run_impl(&self, name: &str, task: &str) -> String {
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return "error: subagent nesting limit reached".to_string();
        }
        let Some(agent) = self.agents.get(name) else {
            let avail = self.agents.keys().cloned().collect::<Vec<_>>().join(", ");
            return format!("error: unknown subagent '{}'. Available: {}", name, avail);
        };
        let llm = self.bot.state.read().await.llms.get(&agent.profile).cloned();
        let Some(llm) = llm else {
            return format!("error: subagent '{}' profile '{}' is unavailable", name, agent.profile);
        };
        let sys = agent
            .system_prompt
            .clone()
            .unwrap_or_else(|| format!("You are the '{}' subagent. Complete the task and report the result.", name));
        let messages = vec![ChatMessage::system(&sys), ChatMessage::user(task)];
        let executor = self.bot.tool_executor.clone();
        // Headless: drain the stream channel; we only want the returned text.
        let (tx, mut rx) = mpsc::channel::<String>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        // Deeper host so a subagent can itself delegate (bounded by depth).
        let deeper: Arc<dyn SubagentHost> = Arc::new(SubagentHostImpl {
            bot: self.bot.clone(),
            agents: self.agents.clone(),
            depth: self.depth + 1,
        });
        info!(agent = %name, profile = %agent.profile, depth = self.depth, "running subagent");
        let result = SUBAGENT
            .scope(deeper, async move {
                llm.chat_with_tools(&messages, Some(executor.as_ref()), tx).await
            })
            .await;
        drain.abort();
        match result {
            Ok(text) => text,
            Err(e) => format!("subagent error: {}", e),
        }
    }
}

/// Returns Some(reply) if the message is a slash command, None otherwise.
async fn handle_slash_command(body: &str, room_id: &str, ctx: &BotCtx) -> Option<String> {
    let trimmed = body.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();

    match cmd.as_str() {
        "/help" => Some(
            "**Roger commands**\n\
             \n\
             **Info**\n\
             `/status` — uptime, model, history stats, active jobs\n\
             `/jobs` — list background jobs with room and elapsed time\n\
             \n\
             **Config**\n\
             `/model [name]` — show or switch this room's LLM profile\n\
             `/model reset` — revert to the room's default profile\n\
             \n\
             **Jobs**\n\
             `/cancel <id>` — abort a background job (see `/jobs`)\n\
             \n\
             **Memory**\n\
             `/clear` — wipe this room's conversation history\n\
             `/forget` — wipe this room's durable memory\n\
             `/forget global` — wipe the shared global memory\n\
             \n\
             **Agents**\n\
             `/agents` — list configured subagents\n\
             `/agent <name> <task>` — run a named subagent manually\n\
             \n\
             **Skills**\n\
             `/skills` — list active + pending skills\n\
             `/skills suggest` — draft a new skill from recent history\n\
             `/skills approve <name>` — promote a pending skill to active\n\
             `/skills forget <name>` — remove a learned or pending skill\n\
             \n\
             `/help` — show this message"
                .to_string(),
        ),
        "/clear" => {
            if let Err(e) = ctx.history.clear(room_id) {
                Some(format!("Failed to clear history: {}", e))
            } else {
                Some("History cleared.".to_string())
            }
        }
        "/status" => {
            let uptime = ctx.started_at.elapsed();
            let secs = uptime.as_secs();
            let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
            let history_len = ctx.history.windowed(room_id, 100).len();
            let (client, profile) = ctx.state.read().await.llm_for_room(room_id);
            let m_snap = ctx.metrics.snapshot();
            let model_desc = if client.fallback_count() > 0 {
                format!("{} (+{} fallback)", client.model(), client.fallback_count())
            } else {
                client.model().to_string()
            };
            let (mcp_servers, mcp_tools) = ctx.tool_executor.mcp_summary();
            let (skills_active, skills_pending) = ctx.skills.list();
            Some(format!(
                "**Roger status**\nUptime: {}h {}m {}s\nProfile: {} ({})\nHistory: {} messages (this room)\nMemory: {}t global, {}t this room\nMCP: {} server(s), {} tool(s)\nSkills: {} active, {} pending\nRequests: {} ({} errors), avg {}ms\nActive jobs: {}",
                h, m, s, profile, model_desc, history_len,
                ctx.memory.global_tokens(), ctx.memory.room_tokens(room_id),
                mcp_servers, mcp_tools,
                skills_active.len(), skills_pending.len(),
                m_snap.requests, m_snap.errors, m_snap.avg_latency_ms,
                ctx.workers.count()
            ))
        }
        "/jobs" => {
            let jobs = ctx.workers.list();
            if jobs.is_empty() {
                Some("No active background jobs.".to_string())
            } else {
                let mut out = String::from("**Active jobs**\n");
                for j in jobs {
                    // Short room tag (localpart before ':').
                    let room_tag = j.room.trim_start_matches('!').split(':').next().unwrap_or(&j.room);
                    out.push_str(&format!(
                        "`{}` — {} ({}) in {}, {}s\n",
                        j.id, j.profile, j.model, room_tag, j.elapsed_secs
                    ));
                }
                out.push_str("`/cancel <id>` to stop one.");
                Some(out)
            }
        }
        "/cancel" => {
            let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
            match arg.parse::<u64>() {
                Ok(id) if ctx.workers.cancel(id) => Some(format!("Cancelled job `{}`.", id)),
                Ok(id) => Some(format!("No active job `{}`. Try `/jobs`.", id)),
                Err(_) => Some("Usage: `/cancel <id>` (see `/jobs`).".to_string()),
            }
        }
        "/forget" => {
            let scope = parts.get(1).map(|s| s.trim()).unwrap_or("");
            let (res, what) = if scope == "global" {
                (ctx.memory.clear_global(), "Global memory")
            } else {
                (ctx.memory.clear_room(room_id), "This room's memory")
            };
            match res {
                Ok(_) => Some(format!("{} cleared.", what)),
                Err(e) => Some(format!("Failed to clear memory: {}", e)),
            }
        }
        "/agents" => {
            let agents = ctx.state.read().await.agents.clone();
            if agents.is_empty() {
                Some("No subagents configured. Add `[agents.<name>]` in profiles.toml.".to_string())
            } else {
                let mut names: Vec<_> = agents.iter().collect();
                names.sort_by(|a, b| a.0.cmp(b.0));
                let mut out = String::from("**Subagents**\n");
                for (n, a) in names {
                    let desc = if a.description.is_empty() { "—" } else { &a.description };
                    out.push_str(&format!("`{}` ({}) — {}\n", n, a.profile, desc));
                }
                out.push_str("`/agent <name> <task>` to run one.");
                Some(out)
            }
        }
        "/agent" => Some(handle_agent_command(parts.get(1).copied(), room_id, ctx).await),
        "/skills" => Some(handle_skills_command(parts.get(1).copied(), room_id, ctx).await),
        "/model" => Some(handle_model_command(parts.get(1).copied(), room_id, ctx).await),
        _ => Some(format!("Unknown command `{}`. Try `/help`.", parts[0])),
    }
}

/// `/skills [list|approve <name>|forget <name>|suggest]`.
async fn handle_skills_command(arg: Option<&str>, room_id: &str, ctx: &BotCtx) -> String {
    let arg = arg.map(str::trim).unwrap_or("");
    let (sub, rest) = arg
        .split_once(char::is_whitespace)
        .map(|(s, r)| (s, r.trim()))
        .unwrap_or((arg, ""));
    match sub {
        "" | "list" => {
            let (active, pending) = ctx.skills.list();
            let mut out = String::from("**Skills**\n");
            out.push_str(&format!(
                "Active: {}\n",
                if active.is_empty() { "(none)".into() } else { active.join(", ") }
            ));
            if !pending.is_empty() {
                out.push_str(&format!("Pending: {}\n", pending.join(", ")));
            }
            out.push_str("`/skills approve <name>` · `/skills forget <name>` · `/skills suggest`");
            out
        }
        "approve" => match ctx.skills.approve(rest) {
            Ok(_) => format!("Approved skill `{}`.", rest),
            Err(e) => format!("error: {}", e),
        },
        "forget" => match ctx.skills.forget(rest) {
            Ok(true) => format!("Forgot skill `{}`.", rest),
            Ok(false) => format!("No learned/pending skill `{}`.", rest),
            Err(e) => format!("error: {}", e),
        },
        "suggest" => handle_skills_suggest(room_id, ctx).await,
        _ => "Usage: `/skills [approve|forget <name>|suggest]`".to_string(),
    }
}

/// `/skills suggest` — ask the compaction-profile LLM to propose one reusable skill
/// from recent history; save it to pending for review.
async fn handle_skills_suggest(room_id: &str, ctx: &BotCtx) -> String {
    let (llm, existing) = {
        let st = ctx.state.read().await;
        (st.llms.get(&st.compaction.profile).cloned(), ctx.skills.list().0)
    };
    let Some(llm) = llm else {
        return "error: compaction profile unavailable for suggestions".to_string();
    };
    let history = ctx.history.windowed_by_tokens(room_id, 4000);
    if history.len() < 3 {
        return "Not enough history yet to suggest a skill.".to_string();
    }
    let transcript = history
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");
    let sys = format!(
        "From this conversation, propose ONE reusable skill (a procedure worth saving \
         for recurring tasks). Output EXACTLY:\n### NAME\n<kebab-case>\n### DESCRIPTION\n\
         <one line>\n### STEPS\n<markdown steps>\nIf nothing is worth saving, output only \
         NONE. Do not duplicate existing skills: {}",
        if existing.is_empty() { "(none)".into() } else { existing.join(", ") }
    );
    let out = match llm.chat(&[ChatMessage::system(&sys), ChatMessage::user(&transcript)]).await {
        Ok(t) => t,
        Err(e) => return format!("error: suggestion failed: {}", e),
    };
    if out.trim().eq_ignore_ascii_case("none") || !out.contains("### NAME") {
        return "No skill worth suggesting right now.".to_string();
    }
    let name = extract_section(&out, "### NAME");
    let desc = extract_section(&out, "### DESCRIPTION");
    let steps = extract_section(&out, "### STEPS");
    if name.is_empty() || steps.is_empty() {
        return "Suggestion was malformed; skipped.".to_string();
    }
    let content = format!("# {}\n\n{}\n\n{}", name, desc, steps);
    match ctx.skills.write_pending(&name, &content) {
        Ok(_) => format!("Suggested skill `{}` — review with `/skills`, then `/skills approve {}`.", name, name),
        Err(e) => format!("error: {}", e),
    }
}

/// Extract the text under a `### HEADER` up to the next `### ` header (or end).
fn extract_section(text: &str, header: &str) -> String {
    match text.find(header) {
        None => String::new(),
        Some(i) => {
            let rest = &text[i + header.len()..];
            let end = rest.find("\n### ").unwrap_or(rest.len());
            rest[..end].trim().to_string()
        }
    }
}

/// `/agent <name> <task>` — run a named subagent on a task in this room's context.
async fn handle_agent_command(arg: Option<&str>, room_id: &str, ctx: &BotCtx) -> String {
    let rest = arg.map(str::trim).unwrap_or("");
    let (name, task) = rest
        .split_once(char::is_whitespace)
        .map(|(n, t)| (n, t.trim()))
        .unwrap_or((rest, ""));
    if name.is_empty() || task.is_empty() {
        return "Usage: `/agent <name> <task>` (see `/agents`).".to_string();
    }
    let (agents, default_wd) = {
        let st = ctx.state.read().await;
        (st.agents.clone(), st.comms.default_workdir.clone())
    };
    if !agents.contains_key(name) {
        return format!("Unknown subagent `{}`. See `/agents`.", name);
    }
    let workdir: Option<PathBuf> = ctx
        .room_workdirs
        .get(room_id)
        .map(PathBuf::from)
        .or_else(|| default_wd.as_deref().map(crate::config::expand_tilde));
    let host = SubagentHostImpl { bot: ctx.clone(), agents, depth: 0 };
    // Scope the room context so a subprocess subagent gets its workdir.
    ROOM_ID
        .scope(
            room_id.to_string(),
            WORKDIR.scope(workdir, host.run_impl(name, task)),
        )
        .await
}

/// `/model` — show or switch the LLM profile for this room. Persists overrides.
async fn handle_model_command(arg: Option<&str>, room_id: &str, ctx: &BotCtx) -> String {
    let arg = arg.map(str::trim).unwrap_or("");

    // No argument: report current profile + the available ones.
    if arg.is_empty() {
        let st = ctx.state.read().await;
        let current = st.profile_name_for_room(room_id);
        let mut names: Vec<String> = st.llms.keys().cloned().collect();
        names.sort();
        let list = names
            .iter()
            .map(|n| format!("`{}`", n))
            .collect::<Vec<_>>()
            .join(", ");
        return format!(
            "Current profile: `{}`\nAvailable: {}\n`/model <name>` to switch · `/model reset` to revert to default",
            current, list
        );
    }

    let reset = arg == "reset" || arg == "default";

    // Mutate the in-memory map under the write lock, then snapshot it for persistence.
    let snapshot = {
        let mut st = ctx.state.write().await;
        if reset {
            st.room_profiles.remove(room_id);
        } else if st.llms.contains_key(arg) {
            st.room_profiles.insert(room_id.to_string(), arg.to_string());
        } else {
            let mut names: Vec<String> = st.llms.keys().cloned().collect();
            names.sort();
            return format!(
                "Unknown profile `{}`. Available: {}",
                arg,
                names.join(", ")
            );
        }
        st.room_profiles.clone()
    };

    if let Err(e) = ctx.room_profiles.save(&snapshot) {
        warn!("failed to persist room profiles: {}", e);
    }

    let (client, profile) = ctx.state.read().await.llm_for_room(room_id);
    if reset {
        format!("Reset to default profile: `{}` ({})", profile, client.model())
    } else {
        format!("Switched to profile `{}` ({})", profile, client.model())
    }
}

async fn transcribe_audio(
    client: &Client,
    audio: &AudioMessageEventContent,
    ctx: &BotCtx,
) -> anyhow::Result<String> {
    let speaches = ctx
        .speaches
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("SPEACHES_URL not configured"))?;

    let bytes = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: audio.source.clone(),
                format: MediaFormat::File,
            },
            true,
        )
        .await?;

    speaches.transcribe(bytes, "audio.ogg").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryStore;
    use crate::llm::{Backend, LlmClient};
    use crate::room_profiles::RoomProfileStore;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn client(model: &str) -> Arc<ProfileLlm> {
        Arc::new(ProfileLlm::new(vec![Arc::new(Backend::Http(LlmClient::new(
            "http://localhost/v1".into(),
            model.into(),
            None,
            128,
            0.0,
            8192,
        )))]))
    }

    fn state() -> ReloadableState {
        let mut llms = HashMap::new();
        llms.insert("chat".to_string(), client("chat-model"));
        llms.insert("reason".to_string(), client("reason-model"));
        let mut room_configs = HashMap::new();
        room_configs.insert(
            "!coding:srv".to_string(),
            RoomConfig {
                name: "Coding".into(),
                require_mention: true,
                system_prompt: None,
                profile: Some("reason".into()),
                operating_file: None,
            },
        );
        let mut profile_comms = HashMap::new();
        profile_comms.insert("chat".to_string(), CommsMode::Sync);
        profile_comms.insert("reason".to_string(), CommsMode::Auto);
        ReloadableState {
            llms,
            system_prompt: "sys".into(),
            room_configs,
            room_profiles: HashMap::new(),
            comms: CommsConfig::default(),
            profile_comms,
            operating_file: None,
            memory_enabled: true,
            memory_max_global_tokens: 1500,
            memory_max_room_tokens: 3000,
            compaction: CompactionConfig::default(),
            agents: HashMap::new(),
        }
    }

    #[test]
    fn unconfigured_room_uses_chat() {
        let st = state();
        let (client, profile) = st.llm_for_room("!unknown:srv");
        assert_eq!(profile, "chat");
        assert_eq!(client.model(), "chat-model");
    }

    #[test]
    fn room_config_profile_is_used() {
        let st = state();
        let (client, profile) = st.llm_for_room("!coding:srv");
        assert_eq!(profile, "reason");
        assert_eq!(client.model(), "reason-model");
    }

    #[test]
    fn runtime_override_beats_room_config() {
        let mut st = state();
        st.room_profiles
            .insert("!coding:srv".to_string(), "chat".to_string());
        let (client, profile) = st.llm_for_room("!coding:srv");
        assert_eq!(profile, "chat");
        assert_eq!(client.model(), "chat-model");
    }

    #[test]
    fn missing_profile_falls_back_to_chat() {
        let mut st = state();
        st.room_profiles
            .insert("!coding:srv".to_string(), "nonexistent".to_string());
        let (client, profile) = st.llm_for_room("!coding:srv");
        assert_eq!(profile, "chat");
        assert_eq!(client.model(), "chat-model");
    }

    fn test_ctx(dir: &TempDir) -> BotCtx {
        BotCtx {
            allowed_rooms: HashSet::new(),
            bot_user_id: "@roger:srv".into(),
            bot_localpart: "roger".into(),
            speaches: None,
            history: Arc::new(HistoryStore::new(dir.path().join("history")).unwrap()),
            started_at: Arc::new(Instant::now()),
            state: Arc::new(RwLock::new(state())),
            room_profiles: Arc::new(RoomProfileStore::new(dir.path().join("rp.json"))),
            metrics: Arc::new(Metrics::default()),
            tool_executor: Arc::new(ToolExecutor::with_projects(None, HashMap::new(), None, None, None)),
            workers: Arc::new(Workers::new(4)),
            room_workdirs: Arc::new(RoomWorkdirStore::load(dir.path().join("rw.json"))),
            memory: Arc::new(MemoryStore::new(dir.path(), None)),
            rooms: Arc::new(RoomQueues::default()),
            skills: Arc::new(SkillStore::new(dir.path(), dir.path())),
        }
    }

    #[tokio::test]
    async fn model_no_arg_lists_available() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir);
        let out = handle_model_command(None, "!coding:srv", &ctx).await;
        assert!(out.contains("Current profile: `reason`"));
        assert!(out.contains("`chat`") && out.contains("`reason`"));
    }

    #[tokio::test]
    async fn model_switch_persists_and_applies() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir);
        let out = handle_model_command(Some("chat"), "!coding:srv", &ctx).await;
        assert!(out.contains("Switched to profile `chat`"));
        // In-memory state updated
        assert_eq!(
            ctx.state.read().await.profile_name_for_room("!coding:srv"),
            "chat"
        );
        // Persisted to disk
        assert_eq!(
            ctx.room_profiles.load().get("!coding:srv").map(String::as_str),
            Some("chat")
        );
    }

    #[tokio::test]
    async fn model_unknown_is_rejected() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir);
        let out = handle_model_command(Some("bogus"), "!coding:srv", &ctx).await;
        assert!(out.starts_with("Unknown profile `bogus`"));
        // Unchanged: still the room-config default
        assert_eq!(
            ctx.state.read().await.profile_name_for_room("!coding:srv"),
            "reason"
        );
        assert!(ctx.room_profiles.load().is_empty());
    }

    #[test]
    fn last_sentence_end_finds_boundaries() {
        assert_eq!(last_sentence_end("no boundary here"), None);
        assert_eq!(last_sentence_end("Hello there."), Some(11));
        // Returns the *last* boundary.
        assert_eq!(last_sentence_end("One. Two!"), Some(8));
        assert!(last_sentence_end("line one\nline two").is_some());
    }

    #[test]
    fn should_flush_respects_rate_floor() {
        let min = Duration::from_millis(250);
        let max = Duration::from_millis(1000);
        // Sentence ready but too soon since last flush → no.
        assert!(!should_flush(true, Duration::from_millis(100), true, min, max));
        // Sentence ready and past the floor → yes.
        assert!(should_flush(true, Duration::from_millis(300), true, min, max));
    }

    #[test]
    fn should_flush_time_ceiling_without_sentence() {
        let min = Duration::from_millis(250);
        let max = Duration::from_millis(1000);
        // No sentence, under the ceiling → no.
        assert!(!should_flush(true, Duration::from_millis(500), false, min, max));
        // No sentence, but ceiling reached → yes.
        assert!(should_flush(true, Duration::from_millis(1000), false, min, max));
    }

    #[test]
    fn should_flush_requires_change() {
        let min = Duration::from_millis(250);
        let max = Duration::from_millis(1000);
        // Unchanged text never flushes, even past the ceiling.
        assert!(!should_flush(false, Duration::from_secs(5), true, min, max));
    }

    #[tokio::test]
    async fn model_reset_clears_override() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir);
        handle_model_command(Some("chat"), "!coding:srv", &ctx).await;
        let out = handle_model_command(Some("reset"), "!coding:srv", &ctx).await;
        assert!(out.contains("Reset to default profile: `reason`"));
        assert!(ctx.room_profiles.load().is_empty());
    }
}
