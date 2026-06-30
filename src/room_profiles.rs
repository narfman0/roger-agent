use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

/// Persists runtime per-room LLM profile overrides (set via `/model`) to a single
/// JSON file so they survive restarts. Keyed by room id → profile name.
pub struct RoomProfileStore {
    path: PathBuf,
}

impl RoomProfileStore {
    pub fn new(path: PathBuf) -> Self {
        RoomProfileStore { path }
    }

    /// Load the persisted overrides, or an empty map if the file is missing/invalid.
    pub fn load(&self) -> HashMap<String, String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist the full map, overwriting the file.
    pub fn save(&self, map: &HashMap<String, String>) -> Result<()> {
        std::fs::write(&self.path, serde_json::to_string_pretty(map)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        let store = RoomProfileStore::new(dir.path().join("rp.json"));
        assert!(store.load().is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = TempDir::new().unwrap();
        let store = RoomProfileStore::new(dir.path().join("rp.json"));
        let mut map = HashMap::new();
        map.insert("!a:srv".to_string(), "reason".to_string());
        store.save(&map).unwrap();
        assert_eq!(store.load(), map);
    }
}
