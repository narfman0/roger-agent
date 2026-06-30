mod audio;
mod config;
mod error;
mod history;
mod llm;
mod matrix;

use anyhow::Result;
use matrix_sdk::config::SyncSettings;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::info;

use crate::matrix::handler::BotCtx;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "roger=info,matrix_sdk=warn".into()),
        )
        .init();

    let config_dir = PathBuf::from("config");
    let cfg = config::Config::load(&config_dir)?;

    info!("roger starting — homeserver: {}", cfg.matrix_homeserver);
    info!("allowlist: {:?}", cfg.room_allowlist);

    // Build LLM client from the "chat" profile
    let chat_backend = cfg.backend_for_profile("chat")?;
    let chat_profile = cfg.profiles.get("chat").expect("chat profile required");
    let llm = Arc::new(llm::LlmClient::new(
        format!("{}/v1", chat_backend.base_url.trim_end_matches('/').trim_end_matches("/v1")),
        chat_backend.model.clone(),
        chat_backend.api_key(),
        chat_profile.max_tokens.unwrap_or(1024),
        chat_profile.temperature.unwrap_or(0.7),
    ));
    info!("LLM: {} @ {}", chat_backend.model, chat_backend.base_url);

    // Build speaches client if configured
    let speaches = cfg.speaches_url.as_ref().map(|url| {
        info!("speaches: {}", url);
        Arc::new(audio::SpeachesClient::new(url.clone()))
    });

    let session_dir = PathBuf::from("roger_session");
    let client = matrix::client::build_client(&cfg.matrix_homeserver, &session_dir).await?;
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

    let bot_ctx = BotCtx {
        allowed_rooms: HashSet::from_iter(cfg.room_allowlist.iter().cloned()),
        room_configs: cfg.rooms,
        bot_user_id,
        bot_localpart,
        llm,
        speaches,
        history,
        system_prompt: cfg.system_prompt,
        started_at: Arc::new(Instant::now()),
    };

    client.add_event_handler_context(bot_ctx);
    client.add_event_handler(matrix::handler::handle_invite);
    client.add_event_handler(matrix::handler::handle_message);

    info!("sync loop starting");
    client.sync(SyncSettings::default()).await?;

    Ok(())
}
