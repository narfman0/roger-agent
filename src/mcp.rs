//! MCP (Model Context Protocol) client support. roger connects to configured MCP
//! servers as a client (stdio child processes), discovers their tools, and exposes
//! them to the HTTP models alongside its native tools. Tool names are namespaced
//! `mcp__<server>__<tool>`; calls are routed back to the owning server.
//!
//! Servers are connected once at startup and kept alive for the process lifetime
//! (restart to change them — not hot-reloaded). Subprocess backends (claude-code /
//! opencode) get MCP through their own config, not this manager.

use crate::config::McpServerConfig;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::transport::TokioChildProcess;
use rmcp::{service::RunningService, RoleClient, ServiceExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::process::Command;
use tracing::{info, warn};

struct McpServer {
    client: RunningService<RoleClient, ()>,
    tools: Vec<Tool>,
}

#[derive(Default)]
pub struct McpManager {
    servers: HashMap<String, McpServer>,
}

impl McpManager {
    /// Connect to every enabled server. Failures are logged and skipped (a bad
    /// server never blocks startup).
    pub async fn connect(configs: &HashMap<String, McpServerConfig>) -> Self {
        let mut servers = HashMap::new();
        for (name, cfg) in configs {
            if !cfg.enabled {
                continue;
            }
            if name.contains("__") {
                warn!("mcp server name '{}' contains '__'; skipping (reserved separator)", name);
                continue;
            }
            match connect_one(cfg).await {
                Ok(server) => {
                    info!(server = %name, tools = server.tools.len(), "mcp server connected");
                    servers.insert(name.clone(), server);
                }
                Err(e) => warn!(server = %name, "mcp server failed to connect: {}", e),
            }
        }
        McpManager { servers }
    }

    /// OpenAI-style function definitions for every MCP tool, namespaced by server.
    pub fn tool_definitions(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (server, s) in &self.servers {
            for tool in &s.tools {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": format!("mcp__{}__{}", server, tool.name),
                        "description": tool.description.as_deref().unwrap_or(""),
                        "parameters": Value::Object((*tool.input_schema).clone()),
                    }
                }));
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Count of connected servers and their total tools (for `/status`).
    pub fn summary(&self) -> (usize, usize) {
        (self.servers.len(), self.servers.values().map(|s| s.tools.len()).sum())
    }

    /// True if `name` is an MCP tool call (`mcp__server__tool`).
    pub fn handles(name: &str) -> bool {
        name.starts_with("mcp__")
    }

    /// Route an `mcp__server__tool` call to its server. `args` is the parsed tool
    /// arguments (should be a JSON object). Returns the tool's text output or an
    /// error string.
    pub async fn call(&self, namespaced: &str, args: Value) -> String {
        let rest = match namespaced.strip_prefix("mcp__") {
            Some(r) => r,
            None => return format!("error: not an mcp tool: {}", namespaced),
        };
        let (server, tool) = match rest.split_once("__") {
            Some(p) => p,
            None => return format!("error: malformed mcp tool name: {}", namespaced),
        };
        let Some(s) = self.servers.get(server) else {
            return format!("error: unknown mcp server '{}'", server);
        };

        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }

        match s.client.call_tool(params).await {
            Err(e) => format!("error: mcp call failed: {}", e),
            Ok(result) => {
                if result.is_error.unwrap_or(false) {
                    return format!("error: {}", render_content(&result.content));
                }
                render_content(&result.content)
            }
        }
    }
}

async fn connect_one(cfg: &McpServerConfig) -> anyhow::Result<McpServer> {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let client = ().serve(TokioChildProcess::new(cmd)?).await?;
    let tools = client.list_all_tools().await?;
    Ok(McpServer { client, tools })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_recognizes_mcp_prefix() {
        assert!(McpManager::handles("mcp__fs__read"));
        assert!(!McpManager::handles("web_search"));
        assert!(!McpManager::handles("read_file"));
    }

    #[test]
    fn empty_manager_advertises_nothing() {
        let m = McpManager::default();
        assert!(m.is_empty());
        assert!(m.tool_definitions().is_empty());
        assert_eq!(m.summary(), (0, 0));
    }
}

/// Flatten an MCP result's content blocks into text.
fn render_content(content: &[rmcp::model::ContentBlock]) -> String {
    let parts: Vec<String> = content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    if parts.is_empty() {
        "(no text content)".to_string()
    } else {
        parts.join("\n")
    }
}
