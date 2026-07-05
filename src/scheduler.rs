//! Scheduled background tasks: nightly compaction + weekly cross-room digest.
//!
//! Nightly: wakes at a configured UTC hour, force-compacts every known room so
//! TLDRs stay fresh even in rooms that haven't hit the size trigger.
//!
//! Weekly: on a configured weekday/hour, reads all room TLDRs, asks the LLM to
//! synthesize a short cross-room digest, and writes it to `~/.roger/memory/digest.md`.
//! The handler injects that file into every system prompt as "## Weekly digest".

use crate::compaction::{compact_room, CompactionParams};
use crate::config::{CompactionConfig, SchedulerConfig};
use crate::history::HistoryStore;
use crate::llm::ProfileLlm;
use crate::memory::MemoryStore;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

/// State the scheduler reads each cycle (reloadable, same as handler).
pub struct SchedulerState {
    pub compaction: CompactionConfig,
    pub memory_max_global_tokens: usize,
    pub memory_max_room_tokens: usize,
    pub memory_max_tldr_tokens: usize,
    /// Compaction LLM profile, resolved from the registry each cycle.
    pub llms: std::collections::HashMap<String, Arc<ProfileLlm>>,
}

/// Spawn the scheduler; returns immediately. The spawned task runs for the process
/// lifetime. `state` is the same `Arc<RwLock<ReloadableState>>` the handler uses,
/// so config hot-reloads automatically apply to the next scheduled cycle.
pub async fn run_scheduler(
    cfg: SchedulerConfig,
    state: Arc<RwLock<crate::matrix::handler::ReloadableState>>,
    history: Arc<HistoryStore>,
    memory: Arc<MemoryStore>,
) {
    if cfg.nightly_compaction {
        info!(hour = cfg.nightly_hour, "scheduler: nightly compaction enabled, target hour {}:00 UTC", cfg.nightly_hour);
        let state2 = state.clone();
        let history2 = history.clone();
        let memory2 = memory.clone();
        let nightly_hour = cfg.nightly_hour;
        tokio::spawn(async move {
            loop {
                let secs = secs_until_hour(nightly_hour);
                info!(secs, "scheduler: sleeping {}s until nightly compaction", secs);
                sleep(Duration::from_secs(secs)).await;
                run_nightly_compaction(&state2, &history2, &memory2).await;
                sleep(Duration::from_secs(3600)).await;
            }
        });
    } else {
        info!("scheduler: nightly compaction disabled");
    }

    if cfg.weekly_digest {
        let day = cfg.weekly_digest_day % 7;
        let hour = cfg.weekly_digest_hour;
        info!(day, hour, "scheduler: weekly digest enabled, target day {} hour {}:00 UTC", day, hour);
        let state2 = state.clone();
        let memory2 = memory.clone();
        tokio::spawn(async move {
            loop {
                let secs = secs_until_weekday_hour(day, hour);
                info!(secs, "scheduler: sleeping {}s until weekly digest", secs);
                sleep(Duration::from_secs(secs)).await;
                run_weekly_digest(&state2, &memory2).await;
                sleep(Duration::from_secs(3600)).await;
            }
        });
    } else {
        info!("scheduler: weekly digest disabled");
    }
}

async fn run_nightly_compaction(
    state: &Arc<RwLock<crate::matrix::handler::ReloadableState>>,
    history: &Arc<HistoryStore>,
    memory: &Arc<MemoryStore>,
) {
    let (compaction_cfg, llm, max_global, max_room, max_tldr) = {
        let st = state.read().await;
        let llm = st.llms.get(&st.compaction.profile).cloned();
        (
            st.compaction.clone(),
            llm,
            st.memory_max_global_tokens,
            st.memory_max_room_tokens,
            st.memory_max_tldr_tokens,
        )
    };
    let Some(llm) = llm else {
        warn!("scheduler: compaction profile '{}' not built, skipping nightly run", compaction_cfg.profile);
        return;
    };

    let rooms = history.list_room_ids();
    info!(count = rooms.len(), "scheduler: starting nightly compaction for {} room(s)", rooms.len());

    for room_id in rooms {
        let params = CompactionParams {
            keep_recent_turns: compaction_cfg.keep_recent_turns,
            max_global_tokens: max_global,
            max_room_tokens: max_room,
            max_tldr_tokens: max_tldr,
            force: true,
        };
        compact_room(
            history.clone(),
            memory.clone(),
            llm.clone(),
            room_id.clone(),
            params,
        ).await;
        info!(room = %room_id, "scheduler: nightly compaction done");
    }
    info!("scheduler: nightly compaction cycle complete");
}

async fn run_weekly_digest(
    state: &Arc<RwLock<crate::matrix::handler::ReloadableState>>,
    memory: &Arc<MemoryStore>,
) {
    let llm = {
        let st = state.read().await;
        st.llms.get(&st.compaction.profile).cloned()
    };
    let Some(llm) = llm else {
        warn!("scheduler: weekly digest — compaction profile not built, skipping");
        return;
    };

    let tldrs = memory.all_tldrs();
    if tldrs.is_empty() {
        info!("scheduler: weekly digest — no TLDRs found, skipping");
        return;
    }

    info!(rooms = tldrs.len(), "scheduler: generating weekly digest from {} room TLDR(s)", tldrs.len());

    let mut prompt = "You are a helpful assistant maintaining a personal assistant's weekly memory digest.\n\
        Below are short summaries (TLDRs) of recent conversations from different rooms/contexts.\n\
        Write a concise cross-room weekly digest (≤400 words) that:\n\
        1. Highlights recurring themes, ongoing projects, and open questions across all rooms.\n\
        2. Calls out anything that needs follow-up or was left unresolved.\n\
        3. Notes any significant decisions or changes.\n\
        Keep it brief and scannable — this is injected into every future system prompt.\n\n\
        Room TLDRs:\n\n".to_string();
    for (room, tldr) in &tldrs {
        prompt.push_str(&format!("### {}\n{}\n\n", room, tldr));
    }
    prompt.push_str("Write the weekly digest now:");

    let messages = vec![crate::history::ChatMessage::user(&prompt)];
    match llm.chat(&messages).await {
        Ok(digest) => {
            let digest = digest.trim().to_string();
            if digest.is_empty() {
                warn!("scheduler: weekly digest — LLM returned empty response");
                return;
            }
            if let Err(e) = memory.rewrite_digest(&digest) {
                warn!("scheduler: weekly digest write failed: {}", e);
            } else {
                info!("scheduler: weekly digest written ({} chars)", digest.len());
            }
        }
        Err(e) => warn!("scheduler: weekly digest LLM call failed: {}", e),
    }
}

/// Seconds from now until the next occurrence of `hour:00:00 UTC`.
fn secs_until_hour(hour: u32) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_per_day: u64 = 86400;
    let secs_per_hour: u64 = 3600;
    let hour_offset = (hour as u64 % 24) * secs_per_hour;
    let today_start = (now / secs_per_day) * secs_per_day;
    let today_trigger = today_start + hour_offset;
    if today_trigger > now {
        today_trigger - now
    } else {
        // Already past today's trigger; schedule for tomorrow.
        today_trigger + secs_per_day - now
    }
}

/// Seconds from now until the next occurrence of `weekday` (0=Sun…6=Sat) at `hour:00:00 UTC`.
/// UNIX epoch day 0 was a Thursday (day 4), so (epoch_days + 4) % 7 gives the weekday.
fn secs_until_weekday_hour(weekday: u32, hour: u32) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_per_day: u64 = 86400;
    let secs_per_hour: u64 = 3600;
    let hour_offset = (hour as u64 % 24) * secs_per_hour;
    // Day number since epoch and current weekday (0=Sun).
    let epoch_day = now / secs_per_day;
    let current_weekday = ((epoch_day + 4) % 7) as u32; // Thu=4 at epoch
    // Days ahead until target weekday.
    let days_ahead = if weekday >= current_weekday {
        (weekday - current_weekday) as u64
    } else {
        (7 - current_weekday + weekday) as u64
    };
    let target_day_start = (epoch_day + days_ahead) * secs_per_day;
    let trigger = target_day_start + hour_offset;
    // If days_ahead==0 but the hour already passed today, jump a full week ahead.
    if trigger <= now {
        trigger + 7 * secs_per_day - now
    } else {
        trigger - now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secs_until_hour_is_within_a_day() {
        for h in 0..24u32 {
            let s = secs_until_hour(h);
            assert!(s <= 86400, "hour {} gave {} secs (>24h)", h, s);
            assert!(s > 0, "hour {} gave 0 secs", h);
        }
    }

    #[test]
    fn secs_until_hour_future_is_less_than_present() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let current_hour = ((now_secs % 86400) / 3600) as u32;
        let next_hour = (current_hour + 1) % 24;
        let s_next = secs_until_hour(next_hour);
        assert!(s_next <= 3601, "next hour {} is {}s away (expected ≤ 3601)", next_hour, s_next);
    }

    #[test]
    fn secs_until_weekday_hour_is_within_a_week() {
        let secs_per_week = 7 * 86400u64;
        for day in 0..7u32 {
            let s = secs_until_weekday_hour(day, 4);
            assert!(s > 0, "day {} gave 0 secs", day);
            assert!(s <= secs_per_week, "day {} gave {}s (>7 days)", day, s);
        }
    }

    #[test]
    fn secs_until_weekday_hour_next_day_is_within_25h() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let epoch_day = now / 86400;
        let tomorrow_weekday = ((epoch_day + 1 + 4) % 7) as u32;
        let s = secs_until_weekday_hour(tomorrow_weekday, 4);
        // Tomorrow is always ≤ 25 hours away (24h + 1h buffer for DST/slow CI).
        assert!(s <= 25 * 3600 + 60, "tomorrow weekday {} is {}s away", tomorrow_weekday, s);
    }
}
