use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use chrono::Local;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommsMode {
    Sync,
    Async,
    Auto,
}

impl Default for CommsMode {
    fn default() -> Self {
        CommsMode::Auto
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatencyClass {
    Fast,
    Slow,
}

impl Default for LatencyClass {
    fn default() -> Self {
        LatencyClass::Fast
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    OpenAi,
    ClaudeCode,
    OpenCode,
}

impl Default for BackendKind {
    fn default() -> Self {
        BackendKind::OpenAi
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileConfig {
    pub backend: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub comms: CommsMode,
    #[serde(default)]
    pub latency_class: LatencyClass,
    #[serde(default)]
    pub idle_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackendConfig {
    #[serde(default)]
    pub kind: BackendKind,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key_env: String,
}

impl BackendConfig {
    pub fn api_key(&self) -> Option<String> {
        if self.api_key_env.is_empty() {
            None
        } else {
            env::var(&self.api_key_env).ok()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CommsConfig {
    #[serde(default = "default_sync_budget")]
    pub sync_budget_ms: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_ms: u64,
    #[serde(default = "default_absolute_ceiling")]
    pub absolute_ceiling_ms: u64,
    #[serde(default = "default_soft_worker_cap")]
    pub soft_worker_cap: usize,
    #[serde(default = "default_max_concurrent_children")]
    pub max_concurrent_children: usize,
    #[serde(default = "default_edit_debounce")]
    pub edit_debounce_ms: u64,
}

fn default_sync_budget() -> u64 { 7000 }
fn default_idle_timeout() -> u64 { 60000 }
fn default_absolute_ceiling() -> u64 { 1800000 }
fn default_soft_worker_cap() -> usize { 4 }
fn default_max_concurrent_children() -> usize { 3 }
fn default_edit_debounce() -> u64 { 600 }

impl Default for CommsConfig {
    fn default() -> Self {
        CommsConfig {
            sync_budget_ms: default_sync_budget(),
            idle_timeout_ms: default_idle_timeout(),
            absolute_ceiling_ms: default_absolute_ceiling(),
            soft_worker_cap: default_soft_worker_cap(),
            max_concurrent_children: default_max_concurrent_children(),
            edit_debounce_ms: default_edit_debounce(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub task_profiles: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomConfig {
    #[serde(default)]
    pub name: String,
    /// If true (default), bot only responds when @-mentioned.
    /// Set to false for rooms where the bot should respond to every message.
    #[serde(default = "default_require_mention")]
    pub require_mention: bool,
}

fn default_require_mention() -> bool { true }

impl Default for RoomConfig {
    fn default() -> Self {
        RoomConfig { name: String::new(), require_mention: true }
    }
}

#[derive(Debug, Deserialize)]
struct ProfilesFile {
    profiles: HashMap<String, ProfileConfig>,
    #[serde(default)]
    routing: Option<RoutingConfig>,
    #[serde(default)]
    comms: Option<CommsConfig>,
    #[serde(default)]
    rooms: HashMap<String, RoomConfig>,
}

#[derive(Debug, Deserialize)]
struct BackendsFile {
    backends: HashMap<String, BackendConfig>,
}

#[derive(Debug)]
pub struct Config {
    pub profiles: HashMap<String, ProfileConfig>,
    pub backends: HashMap<String, BackendConfig>,
    pub routing: RoutingConfig,
    pub comms: CommsConfig,
    pub rooms: HashMap<String, RoomConfig>,
    pub matrix_homeserver: String,
    pub matrix_user: String,
    pub matrix_password: String,
    pub room_allowlist: Vec<String>,
    /// URL for the Whisper-compatible transcription service (e.g. speaches)
    pub speaches_url: Option<String>,
    /// System prompt injected as the first message in every LLM call
    pub system_prompt: String,
}

impl Config {
    pub fn load(config_dir: &Path) -> anyhow::Result<Self> {
        // Load env
        let _ = dotenvy::dotenv();

        // Load profiles
        let profiles_path = config_dir.join("profiles.toml");
        let profiles_str = fs::read_to_string(&profiles_path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", profiles_path.display(), e))?;
        let profiles_file: ProfilesFile = toml::from_str(&profiles_str)?;

        // Load per-host backends
        let host_role = env::var("HOST_ROLE").unwrap_or_else(|_| "local".to_string());
        let backends_path = config_dir.join(format!("backends.{}.toml", host_role));
        let backends_str = fs::read_to_string(&backends_path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}\nCopy backends.example.toml to backends.{}.toml", backends_path.display(), e, host_role))?;
        let backends_file: BackendsFile = toml::from_str(&backends_str)?;

        let matrix_homeserver = env::var("MATRIX_HOMESERVER")
            .map_err(|_| anyhow::anyhow!("MATRIX_HOMESERVER not set"))?;
        let matrix_user = env::var("MATRIX_USER")
            .map_err(|_| anyhow::anyhow!("MATRIX_USER not set"))?;
        let matrix_password = env::var("MATRIX_PASSWORD")
            .map_err(|_| anyhow::anyhow!("MATRIX_PASSWORD not set"))?;
        let room_allowlist = env::var("ROOM_ALLOWLIST")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let speaches_url = env::var("SPEACHES_URL").ok();

        // Load system prompt from file, inject current date
        let prompt_path = config_dir.join("system_prompt.txt");
        let raw_prompt = fs::read_to_string(&prompt_path).unwrap_or_else(|_| {
            "You are Roger, a helpful Matrix-native AI assistant.".to_string()
        });
        let system_prompt = raw_prompt.replace("{date}", &Local::now().format("%Y-%m-%d").to_string());

        Ok(Config {
            profiles: profiles_file.profiles,
            backends: backends_file.backends,
            routing: profiles_file.routing.unwrap_or_else(|| RoutingConfig {
                task_profiles: HashMap::new(),
            }),
            comms: profiles_file.comms.unwrap_or_default(),
            rooms: profiles_file.rooms,
            matrix_homeserver,
            matrix_user,
            matrix_password,
            room_allowlist,
            speaches_url,
            system_prompt,
        })
    }

    pub fn backend_for_profile(&self, profile: &str) -> anyhow::Result<&BackendConfig> {
        let p = self.profiles.get(profile)
            .ok_or_else(|| anyhow::anyhow!("unknown profile: {}", profile))?;
        self.backends.get(&p.backend)
            .ok_or_else(|| anyhow::anyhow!("unknown backend '{}' for profile '{}'", p.backend, profile))
    }
}
