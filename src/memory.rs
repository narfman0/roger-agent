//! Durable memory files, injected into the system prompt. Two tiers:
//! a **global** file shared across rooms and a **per-room** file. Files live under
//! the state dir (`~/.roger/memory/`) and are read fresh each turn. Compaction
//! (a later task) writes them; this module provides the reads used by injection.

use crate::config::expand_tilde;
use std::path::{Path, PathBuf};

pub struct MemoryStore {
    global_path: PathBuf,
    rooms_dir: PathBuf,
}

impl MemoryStore {
    /// `state_dir` is roger's state dir (`~/.roger`). `global_override` is
    /// `[memory].global_file` if configured, else `<state>/memory/global.md`.
    pub fn new(state_dir: &Path, global_override: Option<&str>) -> Self {
        let base = state_dir.join("memory");
        let global_path = match global_override {
            Some(p) => expand_tilde(p),
            None => base.join("global.md"),
        };
        MemoryStore {
            global_path,
            rooms_dir: base.join("rooms"),
        }
    }

    fn room_path(&self, room_id: &str) -> PathBuf {
        // Same sanitization as HistoryStore.
        let safe = room_id.replace(['!', ':', '/'], "_");
        self.rooms_dir.join(format!("{}.md", safe))
    }

    /// Global memory text (empty if the file is missing or blank).
    pub fn read_global(&self) -> String {
        read_trimmed(&self.global_path)
    }

    /// Per-room memory text (empty if the file is missing or blank).
    pub fn read_room(&self, room_id: &str) -> String {
        read_trimmed(&self.room_path(room_id))
    }
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_files_read_empty() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path(), None);
        assert!(store.read_global().is_empty());
        assert!(store.read_room("!r:s").is_empty());
    }

    #[test]
    fn reads_written_files_and_isolates_rooms() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path(), None);
        std::fs::create_dir_all(dir.path().join("memory/rooms")).unwrap();
        std::fs::write(dir.path().join("memory/global.md"), "  global fact\n").unwrap();
        std::fs::write(dir.path().join("memory/rooms/_r_s.md"), "room fact").unwrap();
        assert_eq!(store.read_global(), "global fact");
        assert_eq!(store.read_room("!r:s"), "room fact");
        assert!(store.read_room("!other:s").is_empty());
    }

    #[test]
    fn global_override_path_is_used() {
        let dir = TempDir::new().unwrap();
        let custom = dir.path().join("custom.md");
        std::fs::write(&custom, "custom").unwrap();
        let store = MemoryStore::new(dir.path(), Some(custom.to_str().unwrap()));
        assert_eq!(store.read_global(), "custom");
    }
}
