//! Scheduled background tasks. Currently: nightly compaction of all known rooms.
//!
//! The scheduler wakes once per day at a configured UTC hour, iterates every room
//! that has ever had history written, and force-compacts each one. This ensures
//! TLDRs and memory files stay fresh even in rooms that don't hit the size trigger.

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
    if !cfg.nightly_compaction {
        info!("scheduler: nightly compaction disabled");
        return;
    }
    info!(hour = cfg.nightly_hour, "scheduler: nightly compaction enabled, target hour {}:00 UTC", cfg.nightly_hour);
    tokio::spawn(async move {
        loop {
            let secs = secs_until_hour(cfg.nightly_hour);
            info!(secs, "scheduler: sleeping {}s until nightly compaction", secs);
            sleep(Duration::from_secs(secs)).await;
            run_nightly_compaction(&state, &history, &memory).await;
            // Sleep at least 1 hour before checking again to avoid retriggering
            // within the same UTC hour on slow compaction runs.
            sleep(Duration::from_secs(3600)).await;
        }
    });
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
        // Next hour is always ≤ 1 hour away (plus a small buffer for test latency)
        assert!(s_next <= 3601, "next hour {} is {}s away (expected ≤ 3601)", next_hour, s_next);
    }
}
