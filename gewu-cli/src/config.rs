use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use tracing::warn;

use gewu_tools::mcp::types::McpServerConfig;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub plugins: std::collections::HashMap<String, serde_json::Value>,
    #[serde(rename = "specialistDirs", default)]
    pub specialist_dirs: Vec<String>,
    #[serde(rename = "workflowDirs", default)]
    pub workflow_dirs: Vec<String>,
    #[serde(rename = "toolProviderDirs", default)]
    pub tool_provider_dirs: Vec<String>,
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub path: Option<PathBuf>,
}

impl LoadedConfig {
    pub fn load(config_path: Option<&str>) -> Result<Self> {
        let path = resolve_config_path(config_path)?;

        if !path.exists() {
            return Ok(LoadedConfig {
                config: Config::default(),
                path: None,
            });
        }

        let mut content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        json_strip_comments::strip(&mut content).with_context(|| {
            format!(
                "Failed to strip comments from config file: {}",
                path.display()
            )
        })?;

        let config: Config = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(LoadedConfig {
            config,
            path: Some(path),
        })
    }
}

fn resolve_config_path(config_path: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = config_path {
        return Ok(PathBuf::from(path));
    }

    let project_json = PathBuf::from("config.json");
    let project_jsonc = PathBuf::from("config.jsonc");

    if project_json.exists() && project_jsonc.exists() {
        warn!("Both config.json and config.jsonc exist, using config.json");
    }

    if project_json.exists() {
        return Ok(project_json);
    }

    if project_jsonc.exists() {
        return Ok(project_jsonc);
    }

    let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
    let home_json = home_dir.join(".gewu").join("config.json");
    let home_jsonc = home_dir.join(".gewu").join("config.jsonc");

    if home_json.exists() && home_jsonc.exists() {
        warn!("Both ~/.gewu/config.json and ~/.gewu/config.jsonc exist, using config.json");
    }

    if home_json.exists() {
        return Ok(home_json);
    }

    if home_jsonc.exists() {
        return Ok(home_jsonc);
    }

    Ok(home_json)
}
