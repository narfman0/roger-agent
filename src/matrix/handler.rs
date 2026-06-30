use crate::{
    audio::SpeachesClient,
    config::RoomConfig,
    history::{ChatMessage, HistoryStore},
    llm::LlmClient,
};
use matrix_sdk::{
    event_handler::Ctx,
    media::{MediaFormat, MediaRequestParameters},
    room::Room,
    ruma::events::room::{
        member::{MembershipState, StrippedRoomMemberEvent},
        message::{
            AudioMessageEventContent, MessageType, OriginalSyncRoomMessageEvent,
            ReplacementMetadata, RoomMessageEventContent,
        },
    },
    Client,
};
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Config that can be swapped at runtime on SIGHUP without restarting the bot.
/// Everything reachable behind `BotCtx::state` is reloadable; fields directly on
/// `BotCtx` are fixed for the process lifetime.
pub struct ReloadableState {
    /// LLM client per profile name (e.g. "chat", "reason", "code").
    pub llms: HashMap<String, Arc<LlmClient>>,
    pub system_prompt: String,
    pub room_configs: HashMap<String, RoomConfig>,
    /// Runtime per-room profile overrides set via `/model`, keyed by room id.
    pub room_profiles: HashMap<String, String>,
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

    /// Resolve the LLM client for a room. Returns the client and the profile
    /// name actually used (falls back to "chat" if the requested profile has no
    /// built client).
    pub fn llm_for_room(&self, room_id: &str) -> (Arc<LlmClient>, String) {
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
        }
    } else {
        warn!("declining invite to non-allowlisted room {}", room_id);
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

    // Resolve message body — text directly, audio via transcription
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

    // Mention gate — read the (reloadable) per-room config
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

    info!(room = %room_id, sender = %event.sender, "processing: {}", body);

    // Handle slash commands without touching the LLM
    if let Some(cmd_reply) = handle_slash_command(&body, &room_id, &ctx).await {
        let _ = room.typing_notice(true).await;
        let ack_id = match room.send(RoomMessageEventContent::text_plain("…")).await {
            Ok(resp) => resp.event_id,
            Err(e) => { warn!("failed to send cmd ack: {}", e); return; }
        };
        let _ = room.typing_notice(false).await;
        let edited = RoomMessageEventContent::text_plain(&cmd_reply)
            .make_replacement(ReplacementMetadata::new(ack_id, None), None);
        let _ = room.send(edited).await;
        return;
    }

    // Send typing indicator
    let _ = room.typing_notice(true).await;

    // Send immediate ack so the user sees activity right away
    let ack_id = match room
        .send(RoomMessageEventContent::text_plain("Working on it…"))
        .await
    {
        Ok(resp) => resp.event_id,
        Err(e) => {
            warn!("failed to send ack: {}", e);
            let _ = room.typing_notice(false).await;
            return;
        }
    };

    // Build context: system prompt + history + current message.
    // Snapshot the reloadable bits (LLM client + resolved system prompt) and drop
    // the read lock before the long-running LLM call so SIGHUP reloads aren't blocked.
    if let Err(e) = ctx.history.append(&room_id, ChatMessage::user(&body)) {
        warn!("failed to save user message to history: {}", e);
    }
    let (llm, system_prompt) = {
        let st = ctx.state.read().await;
        let prompt = st
            .room_configs
            .get(&room_id)
            .and_then(|r| r.system_prompt.clone())
            .unwrap_or_else(|| st.system_prompt.clone());
        let (client, _profile) = st.llm_for_room(&room_id);
        (client, prompt)
    };
    let mut messages = vec![ChatMessage::system(&system_prompt)];
    messages.extend(ctx.history.windowed(&room_id, 20));

    // Call LLM with full room history
    let result = llm.chat(&messages).await;

    let _ = room.typing_notice(false).await;

    match result {
        Ok(reply) => {
            if let Err(e) = ctx.history.append(&room_id, ChatMessage::assistant(&reply)) {
                warn!("failed to save assistant reply to history: {}", e);
            }

            let edited = RoomMessageEventContent::text_plain(&reply)
                .make_replacement(ReplacementMetadata::new(ack_id, None), None);

            if let Err(e) = room.send(edited).await {
                warn!("failed to edit ack with reply: {}", e);
            }
        }
        Err(e) => {
            warn!("LLM error: {}", e);
            let error_text = format!("Sorry, I hit an error: {}", e);
            let edited = RoomMessageEventContent::text_plain(&error_text)
                .make_replacement(ReplacementMetadata::new(ack_id, None), None);
            let _ = room.send(edited).await;
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
             `/help` — show this list\n\
             `/clear` — wipe conversation history for this room\n\
             `/status` — show uptime, model, and history stats"
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
            Some(format!(
                "**Roger status**\nUptime: {}h {}m {}s\nProfile: {} ({})\nHistory: {} messages (this room)",
                h, m, s, profile, client.model(), history_len
            ))
        }
        _ => Some(format!("Unknown command `{}`. Try `/help`.", parts[0])),
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

    fn client(model: &str) -> Arc<LlmClient> {
        Arc::new(LlmClient::new(
            "http://localhost/v1".into(),
            model.into(),
            None,
            128,
            0.0,
        ))
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
            },
        );
        ReloadableState {
            llms,
            system_prompt: "sys".into(),
            room_configs,
            room_profiles: HashMap::new(),
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
}
