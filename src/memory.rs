//! Durable memory files, injected into the system prompt. Two tiers:
//! a **global** file shared across rooms and a **per-room** file. Files live under
//! the state dir (`~/.roger/memory/`) and are read fresh each turn. Compaction
//! (a later task) writes them; this module provides the reads used by injection.

use crate::config::expand_tilde;
use crate::history::estimate_tokens;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct MemoryStore {
    global_path: PathBuf,
    rooms_dir: PathBuf,
    tldr_dir: PathBuf,
    /// Serializes writes so concurrent compactions (e.g. two rooms appending to the
    /// shared global file) don't clobber each other's read-modify-write.
    write_lock: Mutex<()>,
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
            tldr_dir: base.join("tldr"),
            write_lock: Mutex::new(()),
        }
    }

    fn room_path(&self, room_id: &str) -> PathBuf {
        // Same sanitization as HistoryStore.
        let safe = room_id.replace(['!', ':', '/'], "_");
        self.rooms_dir.join(format!("{}.md", safe))
    }

    fn tldr_path(&self, room_id: &str) -> PathBuf {
        let safe = room_id.replace(['!', ':', '/'], "_");
        self.tldr_dir.join(format!("{}.md", safe))
    }

    /// Current TLDR for a room — a short cold-start summary written by compaction.
    /// Empty when no compaction has run yet.
    pub fn read_tldr(&self, room_id: &str) -> String {
        read_trimmed(&self.tldr_path(room_id))
    }

    /// Replace the room TLDR with a freshly compacted summary. Called by compaction;
    /// always a full rewrite (not append) so it never accumulates stale content.
    pub fn rewrite_tldr(&self, room_id: &str, content: &str) -> Result<()> {
        let path = self.tldr_path(room_id);
        let _g = self.write_lock.lock().unwrap();
        write_atomic(&path, content)
    }

    /// Delete the TLDR for a room (for `/forget`).
    pub fn clear_tldr(&self, room_id: &str) -> Result<()> {
        remove_if_exists(&self.tldr_path(room_id))
    }

    /// Global memory text (empty if the file is missing or blank).
    pub fn read_global(&self) -> String {
        read_trimmed(&self.global_path)
    }

    /// Per-room memory text (empty if the file is missing or blank).
    pub fn read_room(&self, room_id: &str) -> String {
        read_trimmed(&self.room_path(room_id))
    }

    /// Estimated token size of the global memory (0 when empty).
    pub fn global_tokens(&self) -> usize {
        tokens_of(&self.read_global())
    }

    /// Estimated token size of a room's memory (0 when empty).
    pub fn room_tokens(&self, room_id: &str) -> usize {
        tokens_of(&self.read_room(room_id))
    }

    /// Delete the global memory file (for `/forget global`).
    pub fn clear_global(&self) -> Result<()> {
        remove_if_exists(&self.global_path)
    }

    /// Delete a room's memory file (for `/forget`).
    pub fn clear_room(&self, room_id: &str) -> Result<()> {
        remove_if_exists(&self.room_path(room_id))
    }

    /// Append a block to a room's memory (blank-line separated), creating the file.
    pub fn append_room(&self, room_id: &str, block: &str) -> Result<()> {
        let path = self.room_path(room_id);
        self.append(&path, block)
    }

    /// Append a block to the global memory.
    pub fn append_global(&self, block: &str) -> Result<()> {
        let path = self.global_path.clone();
        self.append(&path, block)
    }

    /// Replace a room's memory with `content` (used by memory self-compaction).
    pub fn rewrite_room(&self, room_id: &str, content: &str) -> Result<()> {
        let path = self.room_path(room_id);
        let _g = self.write_lock.lock().unwrap();
        write_atomic(&path, content)
    }

    /// Replace the global memory with `content`.
    pub fn rewrite_global(&self, content: &str) -> Result<()> {
        let path = self.global_path.clone();
        let _g = self.write_lock.lock().unwrap();
        write_atomic(&path, content)
    }

    fn append(&self, path: &Path, block: &str) -> Result<()> {
        let block = block.trim();
        if block.is_empty() {
            return Ok(());
        }
        let _g = self.write_lock.lock().unwrap();
        let current = read_trimmed(path);
        let next = if current.is_empty() {
            block.to_string()
        } else {
            format!("{}\n{}", current, block)
        };
        write_atomic(path, &next)
    }
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn tokens_of(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        estimate_tokens(s)
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
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

    #[test]
    fn tldr_read_write_clear() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path(), None);
        // Missing → empty
        assert!(store.read_tldr("!r:s").is_empty());
        // Write then read back
        store.rewrite_tldr("!r:s", "  summary line  ").unwrap();
        assert_eq!(store.read_tldr("!r:s"), "summary line");
        // Rewrite replaces (not appends)
        store.rewrite_tldr("!r:s", "new summary").unwrap();
        assert_eq!(store.read_tldr("!r:s"), "new summary");
        // Clear
        store.clear_tldr("!r:s").unwrap();
        assert!(store.read_tldr("!r:s").is_empty());
    }

    #[test]
    fn tldr_rooms_are_isolated() {
        let dir = TempDir::new().unwrap();
        let store = MemoryStore::new(dir.path(), None);
        store.rewrite_tldr("!a:s", "room a").unwrap();
        store.rewrite_tldr("!b:s", "room b").unwrap();
        assert_eq!(store.read_tldr("!a:s"), "room a");
        assert_eq!(store.read_tldr("!b:s"), "room b");
        store.clear_tldr("!a:s").unwrap();
        assert!(store.read_tldr("!a:s").is_empty());
        assert_eq!(store.read_tldr("!b:s"), "room b");
    }
}
