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
use std::{collections::HashMap, sync::Arc};
use tracing::{info, warn};

#[derive(Clone)]
pub struct BotCtx {
    pub allowed_rooms: std::collections::HashSet<String>,
    pub room_configs: HashMap<String, RoomConfig>,
    pub bot_user_id: String,
    pub bot_localpart: String,
    pub llm: Arc<LlmClient>,
    pub speaches: Option<Arc<SpeachesClient>>,
    pub history: Arc<HistoryStore>,
}

impl BotCtx {
    pub fn require_mention(&self, room_id: &str) -> bool {
        self.room_configs
            .get(room_id)
            .map(|r| r.require_mention)
            .unwrap_or(true)
    }

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

    // Mention gate
    if ctx.require_mention(&room_id) && !ctx.is_mentioned(&body) {
        return;
    }

    info!(room = %room_id, sender = %event.sender, "processing: {}", body);

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

    // Record user message and build context window
    if let Err(e) = ctx.history.append(&room_id, ChatMessage::user(&body)) {
        warn!("failed to save user message to history: {}", e);
    }
    let messages = ctx.history.windowed(&room_id, 20);

    // Call LLM with full room history
    let result = ctx.llm.chat(&messages).await;

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
