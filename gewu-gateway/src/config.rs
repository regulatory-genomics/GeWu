use anyhow::{Context, Result};
use gewu_cli::config::LoadedConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
    pub db_path: PathBuf,
    pub plugins: HashMap<String, Value>,
    pub specialist_dirs: Vec<PathBuf>,
    pub workflow_dirs: Vec<PathBuf>,
    pub runtime_plugin_dirs: Vec<PathBuf>,
    pub skill_dirs: Vec<PathBuf>,
}

impl GatewayConfig {
    pub fn load(
        config_path: Option<&str>,
        api_key_override: Option<String>,
        endpoint_override: Option<String>,
        model_override: Option<String>,
        db_path_override: Option<String>,
    ) -> Result<Self> {
        let loaded = LoadedConfig::load(config_path)?;
        let api_key = api_key_override
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .or(loaded.config.api_key.clone())
            .context("OPENAI_API_KEY not provided")?;
        let endpoint = endpoint_override
            .or_else(|| std::env::var("OPENAI_ENDPOINT").ok())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .or(loaded.config.endpoint.clone())
            .or(loaded.config.base_url.clone())
            .context("Endpoint not provided")?;
        let model = model_override
            .or_else(|| std::env::var("OPENAI_MODEL").ok())
            .or(loaded.config.model.clone())
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        let db_path = db_path_override
            .map(expand_config_path)
            .unwrap_or(default_gateway_db_path()?);

        Ok(Self {
            api_key,
            endpoint,
            model,
            db_path,
            plugins: loaded.config.plugins,
            specialist_dirs: loaded
                .config
                .specialist_dirs
                .iter()
                .map(expand_config_path)
                .collect(),
            workflow_dirs: loaded
                .config
                .workflow_dirs
                .iter()
                .map(expand_config_path)
                .collect(),
            runtime_plugin_dirs: loaded
                .config
                .runtime_plugin_dirs
                .iter()
                .map(expand_config_path)
                .collect(),
            skill_dirs: loaded
                .config
                .skill_dirs
                .iter()
                .map(expand_config_path)
                .collect(),
        })
    }
}

pub fn expand_config_path(s: impl AsRef<str>) -> PathBuf {
    let s = s.as_ref();
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

fn default_gateway_db_path() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
    let gewu_dir = home_dir.join(".gewu");
    std::fs::create_dir_all(&gewu_dir).context("Failed to create ~/.gewu")?;
    Ok(gewu_dir.join("gewu-gateway.sqlite3"))
}
