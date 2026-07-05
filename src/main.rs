mod audio;
mod compaction;
mod config;
mod history;
mod llm;
mod markdown;
mod matrix;
mod mcp;
mod memory;
mod metrics;
mod room_profiles;
mod room_workdirs;
mod scheduler;
mod skills;
mod subprocess;
mod tools;
mod web;
mod workers;

use anyhow::Result;
use matrix_sdk::config::SyncSettings;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;
use crate::matrix::handler::{BotCtx, ReloadableState};

/// Resolve roger's state directory: `ROGER_STATE_DIR` if set, else `~/.roger`.
/// A leading `~/` (or a bare `~`) is expanded against `$HOME`. This holds all
/// mutable state (crypto store, session token, history, logs, room overrides),
/// kept separate from the install location and from any agent working directory.
fn resolve_state_dir() -> PathBuf {
    let raw = std::env::var("ROGER_STATE_DIR").unwrap_or_else(|_| "~/.roger".to_string());
    crate::config::expand_tilde(&raw)
}

/// Resolve roger's config directory: `ROGER_CONFIG_DIR` if set, else `~/.roger/config`.
/// Defaults to under the state dir so all user-specific files live in one place.
fn resolve_config_dir() -> PathBuf {
    let raw = std::env::var("ROGER_CONFIG_DIR").unwrap_or_else(|_| "~/.roger/config".to_string());
    crate::config::expand_tilde(&raw)
}

/// Initialize tracing: human-readable to stderr, JSON with daily rotation to a
/// log directory (`ROGER_LOG_DIR`, default `<state_dir>/logs`). The returned
/// guard must be kept alive for the lifetime of the process so the non-blocking
/// writer flushes on shutdown.
fn init_logging(state_dir: &Path) -> WorkerGuard {
    let log_dir = std::env::var("ROGER_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| state_dir.join("logs"));
    let file_appender = tracing_appender::rolling::daily(&log_dir, "roger.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "roger=info,matrix_sdk=warn".into());

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().json().with_ansi(false).with_writer(file_writer))
        .init();

    guard
}

/// Listen for SIGHUP and hot-reload the reloadable parts of the config
/// (LLM client, system prompt, per-room settings). Matrix credentials and the
/// room allowlist are fixed for the process lifetime and require a restart.
async fn reload_on_sighup(config_dir: PathBuf, state: Arc<RwLock<ReloadableState>>) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            warn!("failed to install SIGHUP handler, hot-reload disabled: {}", e);
            return;
        }
    };
    info!("SIGHUP hot-reload enabled (send `kill -HUP <pid>` or `systemctl --user reload roger`)");

    while sighup.recv().await.is_some() {
        info!("SIGHUP received — reloading config");
        let cfg = match Config::load(&config_dir) {
            Ok(c) => c,
            Err(e) => {
                warn!("config reload failed (keeping current config): {}", e);
                continue;
            }
        };
        let llms = match cfg.build_all_llms() {
            Ok(v) => v,
            Err(e) => {
                warn!("config reload failed building LLMs (keeping current config): {}", e);
                continue;
            }
        };
        let profile_comms = cfg
            .profiles
            .iter()
            .map(|(k, p)| (k.clone(), p.comms.clone()))
            .collect();
        let mut st = state.write().await;
        st.llms = llms.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        st.system_prompt = cfg.system_prompt;
        st.room_configs = cfg.rooms;
        st.comms = cfg.comms;
        st.profile_comms = profile_comms;
        st.operating_file = cfg.context.operating_file;
        st.memory_enabled = cfg.memory.enabled;
        st.memory_max_global_tokens = cfg.memory.max_global_tokens;
        st.memory_max_room_tokens = cfg.memory.max_room_tokens;
        st.memory_max_tldr_tokens = cfg.memory.max_tldr_tokens;
        st.compaction = cfg.compaction;
        st.agents = cfg.agents;
        // Drop runtime /model overrides that point at a profile that no longer builds.
        let valid: HashSet<String> = st.llms.keys().cloned().collect();
        st.room_profiles.retain(|_, profile| valid.contains(profile));
        info!(
            "config reloaded — profiles: {}, rooms: {}",
            st.llms.len(),
            st.room_configs.len()
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let state_dir = resolve_state_dir();
    std::fs::create_dir_all(&state_dir)?;
    let _log_guard = init_logging(&state_dir);

    info!("state dir: {}", state_dir.display());

    let config_dir = resolve_config_dir();
    std::fs::create_dir_all(&config_dir)?;
    info!("config dir: {}", config_dir.display());
    let cfg = Config::load(&config_dir)?;

    let db_dir = state_dir.join("db");
    std::fs::create_dir_all(&db_dir)?;

    info!("roger starting — homeserver: {}", cfg.matrix_homeserver);
    info!("allowlist: {:?}", cfg.room_allowlist);

    // Process-wide cap on concurrent subprocess children (set once; survives reloads).
    subprocess::set_child_limit(cfg.comms.max_concurrent_children);

    // Build an LLM per profile (primary + fallbacks; chat required)
    let llms: HashMap<String, Arc<llm::ProfileLlm>> = cfg
        .build_all_llms()?
        .into_iter()
        .map(|(k, v)| (k, Arc::new(v)))
        .collect();
    for (name, client) in &llms {
        info!("profile '{}' → {}", name, client.model_chain().join(" → "));
    }

    // Build speaches client if configured
    let speaches = cfg.speaches_url.as_ref().map(|url| {
        info!("speaches: {}", url);
        Arc::new(audio::SpeachesClient::new(url.clone()))
    });

    // Per-room agentic workdir store (set via the set_workdir tool), persisted in
    // the state dir. Shared between the tool executor (writes) and handler (reads).
    let room_workdirs = Arc::new(room_workdirs::RoomWorkdirStore::load(
        state_dir.join("room_workdirs.json"),
    ));

    // Durable-memory store (global + per-room files under the state dir).
    let memory = Arc::new(memory::MemoryStore::new(
        &state_dir,
        cfg.memory.global_file.as_deref(),
    ));

    // Known projects (name → expanded path) selectable via set_workdir.
    let projects: HashMap<String, String> = cfg
        .projects
        .iter()
        .map(|(k, v)| (k.clone(), config::expand_tilde(v).to_string_lossy().into_owned()))
        .collect();
    if !projects.is_empty() {
        info!("projects: {}", projects.keys().cloned().collect::<Vec<_>>().join(", "));
    }

    // Connect MCP servers (once at startup; restart to change them).
    let mcp = Arc::new(mcp::McpManager::connect(&cfg.mcp.servers).await);
    if !mcp.is_empty() {
        let (servers, tools) = mcp.summary();
        info!("mcp: {} server(s), {} tool(s)", servers, tools);
    }

    // Reusable skills store (committed config/skills + learned ~/.roger/skills).
    let skills = Arc::new(skills::SkillStore::new(&config_dir, &state_dir));

    // Build tool executor (web/file tools, set_workdir, MCP, skills)
    let tool_executor = Arc::new(tools::ToolExecutor::with_projects(
        cfg.searxng_url.clone(),
        projects,
        Some(room_workdirs.clone()),
        Some(mcp.clone()),
        Some(skills.clone()),
    ));
    if let Some(url) = &cfg.searxng_url {
        info!("searxng: {} (web_search enabled)", url);
    } else {
        info!("SEARXNG_URL not set — web_search will return an error when called");
    }

    let session_dir = state_dir;
    let client = matrix::client::build_client(&cfg.matrix_homeserver, &session_dir, &db_dir).await?;
    matrix::client::login(&client, &cfg.matrix_user, &cfg.matrix_password, &session_dir).await?;

    let bot_user_id = client
        .user_id()
        .ok_or_else(|| anyhow::anyhow!("not logged in"))?
        .to_string();

    let bot_localpart = bot_user_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(&bot_user_id)
        .to_string();

    for (room_id, room_cfg) in &cfg.rooms {
        info!(
            room = %room_id,
            name = %room_cfg.name,
            require_mention = room_cfg.require_mention,
            "room config"
        );
    }

    let history = Arc::new(history::HistoryStore::new(session_dir.join("history"))?);
    info!("history store initialized");

    // Load persisted /model overrides, dropping any that name a profile that
    // isn't built on this host.
    let room_profile_store =
        Arc::new(room_profiles::RoomProfileStore::new(session_dir.join("room_profiles.json")));
    let mut room_profiles_map = room_profile_store.load();
    room_profiles_map.retain(|_, profile| llms.contains_key(profile));
    if !room_profiles_map.is_empty() {
        info!("loaded {} persisted /model override(s)", room_profiles_map.len());
    }

    let profile_comms = cfg
        .profiles
        .iter()
        .map(|(k, p)| (k.clone(), p.comms.clone()))
        .collect();
    let workers = Arc::new(workers::Workers::new(cfg.comms.soft_worker_cap));

    // Root dir for saved attachments (per-room subdirs created under it).
    let attachment_dir = Arc::new(
        cfg.comms
            .attachment_dir
            .as_deref()
            .map(config::expand_tilde)
            .unwrap_or_else(|| session_dir.join("attachments")),
    );

    let state = Arc::new(RwLock::new(ReloadableState {
        llms,
        system_prompt: cfg.system_prompt,
        room_configs: cfg.rooms,
        room_profiles: room_profiles_map,
        comms: cfg.comms,
        profile_comms,
        operating_file: cfg.context.operating_file,
        memory_enabled: cfg.memory.enabled,
        memory_max_global_tokens: cfg.memory.max_global_tokens,
        memory_max_room_tokens: cfg.memory.max_room_tokens,
        memory_max_tldr_tokens: cfg.memory.max_tldr_tokens,
        compaction: cfg.compaction,
        agents: cfg.agents,
    }));

    // Spawn the SIGHUP hot-reload listener
    tokio::spawn(reload_on_sighup(config_dir, state.clone()));

    // Spawn the nightly compaction scheduler
    scheduler::run_scheduler(cfg.scheduler.clone(), state.clone(), history.clone(), memory.clone()).await;

    let web_cfg = cfg.web.clone();
    let bot_ctx = BotCtx {
        allowed_rooms: HashSet::from_iter(cfg.room_allowlist.iter().cloned()),
        bot_user_id: bot_user_id.clone(),
        bot_localpart,
        speaches,
        history,
        started_at: Arc::new(Instant::now()),
        state,
        room_profiles: room_profile_store,
        metrics: Arc::new(metrics::Metrics::default()),
        tool_executor,
        workers,
        room_workdirs,
        memory,
        rooms: Arc::new(matrix::handler::RoomQueues::default()),
        skills,
        attachment_dir,
    };

    // Spawn the optional web control panel (after bot_ctx is built so we can clone it)
    if web_cfg.enabled {
        let web_bot = bot_ctx.clone();
        let web_client = client.clone();
        tokio::spawn(async move {
            web::start(web_bot, web_client, web_cfg.bind, web_cfg.auth_token).await;
        });
    }

    client.add_event_handler_context(bot_ctx);
    client.add_event_handler(matrix::handler::handle_invite);
    client.add_event_handler(matrix::handler::handle_message);

    // Publish m.room.bot.options to all already-joined rooms so clients that
    // support it show command hints without needing a fresh invite.
    {
        let c = client.clone();
        let startup_bot_id = bot_user_id.clone();
        tokio::spawn(async move {
            // Brief delay to let the initial sync complete before sending state events.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            for room in c.joined_rooms() {
                matrix::handler::publish_bot_options(&room, &startup_bot_id).await;
            }
        });
    }

    info!("sync loop starting");
    client.sync(SyncSettings::default()).await?;

    Ok(())
}
