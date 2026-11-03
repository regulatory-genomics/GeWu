use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use tracing::warn;

use gewu_tools::mcp::types::McpServerConfig;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Optional path to a log file recording every wire LLM request as JSONL.
    pub log: Option<String>,
    /// Optional directories to scan for skills (files named `SKILL.md`).
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Destination policy for the `webfetch` tool.
    #[serde(rename = "webFetch", default)]
    pub web_fetch: Option<WebFetchConfig>,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
}

/// `webFetch` section of the config file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchConfig {
    /// Refuse destinations that resolve to a private address and do not follow
    /// redirects. Off by default.
    #[serde(default)]
    pub block_private_networks: bool,
    /// Host suffixes that may be fetched. Empty means "no host allowlist".
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

impl WebFetchConfig {
    pub fn policy(&self) -> gewu_tools::web_fetch::WebFetchPolicy {
        gewu_tools::web_fetch::WebFetchPolicy {
            block_private_networks: self.block_private_networks,
            allowed_hosts: self.allowed_hosts.clone(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_option() {
        let config: Config = serde_json::from_str(r#"{"log": "gewu-requests.jsonl"}"#).unwrap();
        assert_eq!(config.log.as_deref(), Some("gewu-requests.jsonl"));
    }

    #[test]
    fn log_is_optional() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.log.is_none());
    }

    #[test]
    fn parses_skills_option() {
        let config: Config =
            serde_json::from_str(r#"{"skills": ["skills", "/abs/skills"]}"#).unwrap();
        assert_eq!(
            config.skills,
            Some(vec!["skills".to_string(), "/abs/skills".to_string()])
        );
    }

    #[test]
    fn skills_is_optional() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.skills.is_none());
    }

    #[test]
    fn parses_the_web_fetch_policy() {
        let config: Config = serde_json::from_str(
            r#"{"webFetch": {"blockPrivateNetworks": true, "allowedHosts": ["pubmed.ncbi.nlm.nih.gov"]}}"#,
        )
        .unwrap();

        let policy = config.web_fetch.expect("webFetch").policy();
        assert!(policy.block_private_networks);
        assert_eq!(policy.allowed_hosts, vec!["pubmed.ncbi.nlm.nih.gov"]);
    }

    #[test]
    fn web_fetch_is_permissive_when_omitted() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.web_fetch.is_none());
    }
}
