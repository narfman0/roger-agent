//! Size-triggered conversation compaction. When a room's history grows past a
//! token threshold, summarize the older turns into a single head summary (kept in
//! history) and distill durable facts into two-tier memory (per-room + global),
//! preserving the most recent turns verbatim. Runs detached; guarded so a room is
//! never compacted by two tasks at once.

use crate::history::{ChatMessage, HistoryStore};
use crate::llm::ProfileLlm;
use crate::memory::MemoryStore;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{info, warn};

pub struct CompactionParams {
    pub keep_recent_turns: usize,
    pub max_global_tokens: usize,
    pub max_room_tokens: usize,
}

static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Reserve a room for compaction; returns false if one is already running.
fn begin(room: &str) -> bool {
    ACTIVE
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .insert(room.to_string())
}

fn end(room: &str) {
    if let Some(set) = ACTIVE.get() {
        set.lock().unwrap().remove(room);
    }
}

/// Compact a room (spawn this detached). No-op if the room is already compacting
/// or has too little history.
pub async fn compact_room(
    history: Arc<HistoryStore>,
    memory: Arc<MemoryStore>,
    llm: Arc<ProfileLlm>,
    room_id: String,
    params: CompactionParams,
) {
    if !begin(&room_id) {
        return;
    }
    if let Err(e) = run(history.as_ref(), memory.as_ref(), llm.as_ref(), &room_id, &params).await {
        warn!("compaction failed for {}: {}", room_id, e);
    }
    end(&room_id);
}

async fn run(
    history: &HistoryStore,
    memory: &MemoryStore,
    llm: &ProfileLlm,
    room_id: &str,
    params: &CompactionParams,
) -> Result<()> {
    let msgs = history.load(room_id);
    if msgs.len() <= params.keep_recent_turns {
        return Ok(());
    }
    let split = msgs.len() - params.keep_recent_turns;
    let (old, recent) = msgs.split_at(split);

    let sys = "You compact a chat assistant's memory. Given an earlier conversation \
        excerpt, output three sections with EXACTLY these headers and nothing before them:\n\
        ### SUMMARY\n(a concise summary preserving decisions, facts, and open threads)\n\
        ### ROOM_MEMORY\n(durable facts specific to THIS room worth remembering long-term, \
        as short bullets; or 'none')\n\
        ### GLOBAL_MEMORY\n(durable facts useful across ALL rooms — stable preferences, \
        identities, standing instructions — as short bullets; or 'none')\n\
        Be terse. Do not invent facts.";
    let user = format!("Earlier conversation:\n\n{}", render(old));
    let out = llm
        .chat(&[ChatMessage::system(sys), ChatMessage::user(&user)])
        .await?;

    let summary = section(&out, "### SUMMARY").unwrap_or_default();
    let room_mem = section(&out, "### ROOM_MEMORY").unwrap_or_default();
    let global_mem = section(&out, "### GLOBAL_MEMORY").unwrap_or_default();

    // If the model returned nothing usable, don't destroy history.
    if summary.is_empty() {
        warn!("compaction produced no summary for {}; leaving history intact", room_id);
        return Ok(());
    }

    let mut new_hist = Vec::with_capacity(recent.len() + 1);
    new_hist.push(ChatMessage::system(format!(
        "[Earlier conversation summary]\n{}",
        summary
    )));
    new_hist.extend(recent.iter().cloned());
    history.rewrite(room_id, new_hist)?;
    info!(room = %room_id, compacted = old.len(), kept = recent.len(), "compacted history");

    if is_content(&room_mem) {
        memory.append_room(room_id, &room_mem)?;
        if memory.room_tokens(room_id) > params.max_room_tokens {
            if let Ok(c) = condense(llm, &memory.read_room(room_id), params.max_room_tokens).await {
                let _ = memory.rewrite_room(room_id, &c);
            }
        }
    }
    if is_content(&global_mem) {
        memory.append_global(&global_mem)?;
        if memory.global_tokens() > params.max_global_tokens {
            if let Ok(c) = condense(llm, &memory.read_global(), params.max_global_tokens).await {
                let _ = memory.rewrite_global(&c);
            }
        }
    }
    Ok(())
}

/// Re-summarize an over-cap memory file to bound its growth.
async fn condense(llm: &ProfileLlm, text: &str, target_tokens: usize) -> Result<String> {
    let sys = format!(
        "Condense the following notes to well under {} tokens, keeping the most \
         important durable facts as short bullets. Merge duplicates. Output only the \
         condensed notes.",
        target_tokens
    );
    llm.chat(&[ChatMessage::system(&sys), ChatMessage::user(text)]).await
}

fn is_content(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && !t.eq_ignore_ascii_case("none")
}

fn render(msgs: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in msgs {
        let label = match m.role.as_str() {
            "assistant" => "Assistant",
            "system" => "Note",
            _ => "User",
        };
        out.push_str(label);
        out.push_str(": ");
        out.push_str(&m.content);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// Extract the text under a `### HEADER` up to the next `### ` header (or end).
fn section(text: &str, header: &str) -> Option<String> {
    let start = text.find(header)? + header.len();
    let rest = &text[start..];
    let end = rest.find("\n### ").unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_extracts_between_headers() {
        let out = "### SUMMARY\nchatted about cats.\n### ROOM_MEMORY\n- likes cats\n### GLOBAL_MEMORY\nnone";
        assert_eq!(section(out, "### SUMMARY").unwrap(), "chatted about cats.");
        assert_eq!(section(out, "### ROOM_MEMORY").unwrap(), "- likes cats");
        assert_eq!(section(out, "### GLOBAL_MEMORY").unwrap(), "none");
        assert!(section(out, "### MISSING").is_none());
    }

    #[test]
    fn is_content_rejects_none_and_blank() {
        assert!(is_content("- a real fact"));
        assert!(!is_content("  none "));
        assert!(!is_content("None"));
        assert!(!is_content(""));
    }

    #[test]
    fn render_labels_roles() {
        let msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::system("[summary]"),
        ];
        let r = render(&msgs);
        assert!(r.contains("User: hi"));
        assert!(r.contains("Assistant: hello"));
        assert!(r.contains("Note: [summary]"));
    }
}
