//! Background-job registry for the orchestrator. Tracks in-flight response jobs
//! (sync, async, and auto-promoted) so `/status` can report them, `/cancel` can
//! abort them, and agentic subprocess jobs can be serialized per room (one at a
//! time, to avoid two agents fighting over the same working directory).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tokio::task::AbortHandle;
use tracing::warn;

pub struct JobHandle {
    pub room: String,
    pub profile: String,
    pub model: String,
    pub started: Instant,
    /// Whether this job runs an agentic subprocess (for per-room serialization).
    pub agentic: bool,
    /// Set shortly after spawn; `None` only during the brief insert→spawn window.
    pub abort: Option<AbortHandle>,
}

/// A point-in-time view of one active job for display.
pub struct JobInfo {
    pub id: u64,
    pub room: String,
    pub profile: String,
    pub model: String,
    pub elapsed_secs: u64,
}

pub struct Workers {
    next_id: AtomicU64,
    jobs: Mutex<HashMap<u64, JobHandle>>,
    /// Soft advisory cap on total concurrent jobs (warn, don't block).
    soft_cap: usize,
}

impl Workers {
    pub fn new(soft_cap: usize) -> Self {
        Workers {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(HashMap::new()),
            soft_cap,
        }
    }

    /// Insert a job and return its id. The `abort` handle is filled in via
    /// [`set_abort`] right after the task is spawned.
    pub fn insert_pending(&self, handle: JobHandle) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut jobs = self.jobs.lock().unwrap();
        jobs.insert(id, handle);
        if jobs.len() > self.soft_cap {
            warn!(active = jobs.len(), soft_cap = self.soft_cap, "background jobs over soft cap");
        }
        id
    }

    /// Attach the abort handle. No-op if the job already completed (race-safe).
    pub fn set_abort(&self, id: u64, abort: AbortHandle) {
        if let Some(j) = self.jobs.lock().unwrap().get_mut(&id) {
            j.abort = Some(abort);
        }
    }

    pub fn remove(&self, id: u64) {
        self.jobs.lock().unwrap().remove(&id);
    }

    /// True if the room already has an active agentic (subprocess) job.
    pub fn agentic_active_in_room(&self, room: &str) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .any(|j| j.agentic && j.room == room)
    }

    pub fn count(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    /// Abort a job by id; returns true if it existed.
    pub fn cancel(&self, id: u64) -> bool {
        if let Some(j) = self.jobs.lock().unwrap().remove(&id) {
            if let Some(abort) = j.abort {
                abort.abort();
            }
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<JobInfo> {
        let jobs = self.jobs.lock().unwrap();
        let mut out: Vec<JobInfo> = jobs
            .iter()
            .map(|(&id, j)| JobInfo {
                id,
                room: j.room.clone(),
                profile: j.profile.clone(),
                model: j.model.clone(),
                elapsed_secs: j.started.elapsed().as_secs(),
            })
            .collect();
        out.sort_by_key(|j| j.id);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(room: &str, agentic: bool) -> JobHandle {
        JobHandle {
            room: room.into(),
            profile: "p".into(),
            model: "m".into(),
            started: Instant::now(),
            agentic,
            abort: None,
        }
    }

    #[test]
    fn ids_increase_and_count_tracks() {
        let w = Workers::new(4);
        let a = w.insert_pending(handle("!r:s", false));
        let b = w.insert_pending(handle("!r:s", false));
        assert!(b > a);
        assert_eq!(w.count(), 2);
        w.remove(a);
        assert_eq!(w.count(), 1);
    }

    #[test]
    fn agentic_serialization_is_per_room() {
        let w = Workers::new(4);
        w.insert_pending(handle("!coding:s", true));
        assert!(w.agentic_active_in_room("!coding:s"));
        assert!(!w.agentic_active_in_room("!other:s"));
        // A non-agentic job doesn't count as an agentic occupant.
        let w2 = Workers::new(4);
        w2.insert_pending(handle("!coding:s", false));
        assert!(!w2.agentic_active_in_room("!coding:s"));
    }

    #[test]
    fn cancel_missing_is_false() {
        let w = Workers::new(4);
        assert!(!w.cancel(999));
    }

    #[tokio::test]
    async fn cancel_aborts_the_task() {
        let w = Workers::new(4);
        let id = w.insert_pending(handle("!r:s", true));
        let task = tokio::spawn(async {
            // Long enough that it won't finish on its own during the test.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        w.set_abort(id, task.abort_handle());
        assert!(w.cancel(id));
        assert_eq!(w.count(), 0);
        // The aborted task resolves to a JoinError.
        assert!(task.await.is_err());
    }
}
