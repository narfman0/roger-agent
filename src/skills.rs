//! Skills: reusable procedures roger can load and author. A skill is a markdown
//! file (`<name>.md`) with a title and a one-line description followed by steps.
//! Active skills live in `config/skills/` (committed) and `~/.roger/skills/active/`
//! (learned); a small index (name + description) is injected into the system
//! prompt, and full bodies are loaded on demand via the `read_skill` tool.
//!
//! Self-improvement is approval-gated: `write_skill` and `/skills suggest` write to
//! `~/.roger/skills/pending/`; a skill only becomes active after `/skills approve`.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

pub struct SkillStore {
    committed_dir: PathBuf,
    learned_dir: PathBuf,
    pending_dir: PathBuf,
}

impl SkillStore {
    pub fn new(config_dir: &Path, state_dir: &Path) -> Self {
        SkillStore {
            committed_dir: config_dir.join("skills"),
            learned_dir: state_dir.join("skills").join("active"),
            pending_dir: state_dir.join("skills").join("pending"),
        }
    }

    fn active_dirs(&self) -> [&Path; 2] {
        [self.committed_dir.as_path(), self.learned_dir.as_path()]
    }

    /// `(name, description)` for every active skill (learned overrides committed).
    pub fn index(&self) -> Vec<(String, String)> {
        let mut map: std::collections::BTreeMap<String, String> = Default::default();
        for dir in self.active_dirs() {
            for (name, content) in read_md_dir(dir) {
                map.insert(name, description_of(&content));
            }
        }
        map.into_iter().collect()
    }

    /// Full body of an active skill by name.
    pub fn read(&self, name: &str) -> Option<String> {
        let safe = sanitize(name);
        for dir in self.active_dirs() {
            let p = dir.join(format!("{}.md", safe));
            if let Ok(s) = std::fs::read_to_string(&p) {
                return Some(s);
            }
        }
        None
    }

    /// Draft a skill into the pending area (awaiting `/skills approve`).
    pub fn write_pending(&self, name: &str, content: &str) -> Result<()> {
        let safe = sanitize(name);
        if safe.is_empty() {
            return Err(anyhow!("invalid skill name"));
        }
        std::fs::create_dir_all(&self.pending_dir)?;
        std::fs::write(self.pending_dir.join(format!("{}.md", safe)), content)?;
        Ok(())
    }

    /// Promote a pending skill to the learned (active) set.
    pub fn approve(&self, name: &str) -> Result<()> {
        let safe = sanitize(name);
        let from = self.pending_dir.join(format!("{}.md", safe));
        if !from.exists() {
            return Err(anyhow!("no pending skill '{}'", name));
        }
        std::fs::create_dir_all(&self.learned_dir)?;
        let content = std::fs::read_to_string(&from)?;
        std::fs::write(self.learned_dir.join(format!("{}.md", safe)), content)?;
        let _ = std::fs::remove_file(&from);
        Ok(())
    }

    /// Delete a learned or pending skill (committed skills are read-only).
    pub fn forget(&self, name: &str) -> Result<bool> {
        let safe = sanitize(name);
        let mut removed = false;
        for dir in [&self.learned_dir, &self.pending_dir] {
            let p = dir.join(format!("{}.md", safe));
            if p.exists() {
                std::fs::remove_file(&p)?;
                removed = true;
            }
        }
        Ok(removed)
    }

    /// `(active_names, pending_names)`.
    pub fn list(&self) -> (Vec<String>, Vec<String>) {
        let active: Vec<String> = self.index().into_iter().map(|(n, _)| n).collect();
        let pending: Vec<String> = read_md_dir(&self.pending_dir).into_iter().map(|(n, _)| n).collect();
        (active, pending)
    }
}

fn sanitize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// The skill's one-line description: the first non-empty, non-heading line.
fn description_of(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_start_matches('>').trim().chars().take(120).collect())
        .unwrap_or_default()
}

fn read_md_dir(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) == Some("md") {
                if let (Some(stem), Ok(content)) =
                    (path.file_stem().and_then(|s| s.to_str()), std::fs::read_to_string(&path))
                {
                    out.push((stem.to_string(), content));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_pending_approve_read_forget_cycle() {
        let cfg = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let store = SkillStore::new(cfg.path(), state.path());

        assert!(store.index().is_empty());
        store.write_pending("deploy-flow", "# deploy-flow\nHow to deploy\n\n1. build\n2. ship").unwrap();
        // Pending, not active yet.
        assert!(store.index().is_empty());
        assert_eq!(store.list().1, vec!["deploy-flow"]);

        store.approve("deploy-flow").unwrap();
        let idx = store.index();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0], ("deploy-flow".to_string(), "How to deploy".to_string()));
        assert!(store.read("deploy-flow").unwrap().contains("1. build"));

        assert!(store.forget("deploy-flow").unwrap());
        assert!(store.index().is_empty());
    }

    #[test]
    fn description_skips_headings() {
        assert_eq!(description_of("# Title\n\n> does a thing\n\nsteps"), "does a thing");
        assert_eq!(description_of("plain first line"), "plain first line");
    }

    #[test]
    fn learned_overrides_committed() {
        let cfg = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::fs::create_dir_all(cfg.path().join("skills")).unwrap();
        std::fs::write(cfg.path().join("skills").join("x.md"), "# x\ncommitted desc").unwrap();
        let store = SkillStore::new(cfg.path(), state.path());
        assert_eq!(store.index()[0].1, "committed desc");
        store.write_pending("x", "# x\nlearned desc").unwrap();
        store.approve("x").unwrap();
        assert_eq!(store.index()[0].1, "learned desc");
    }
}
