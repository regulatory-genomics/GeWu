use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::time::sleep;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TachyonBrowserConfig {
    pub enabled: bool,
    #[serde(rename = "autoStart", alias = "auto_start")]
    pub auto_start: bool,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    #[serde(rename = "healthPath", alias = "health_path")]
    pub health_path: String,
    #[serde(rename = "startupTimeoutMs", alias = "startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(rename = "workingDirectory", alias = "working_directory")]
    pub working_directory: Option<String>,
}

impl Default for TachyonBrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start: false,
            url: None,
            command: None,
            args: Vec::new(),
            health_path: "/api/health".to_string(),
            startup_timeout_ms: 10_000,
            working_directory: None,
        }
    }
}

pub struct ManagedSidecar {
    url: String,
    child: Option<Child>,
}

impl ManagedSidecar {
    fn external(url: String) -> Self {
        Self { url, child: None }
    }

    fn owned(url: String, child: Child) -> Self {
        Self {
            url,
            child: Some(child),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for ManagedSidecar {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

pub async fn ensure_tachyon_browser(
    cfg: &TachyonBrowserConfig,
    env_url: Option<&str>,
) -> Result<Option<ManagedSidecar>> {
    if !cfg.enabled {
        if let Some(url) = env_url.filter(|u| !u.trim().is_empty()) {
            if health_ok(url, &cfg.health_path).await {
                return Ok(Some(ManagedSidecar::external(url.to_string())));
            }
            tracing::warn!("TACHYON_BROWSER_URL is set but health check failed: {url}");
        }
        return Ok(None);
    }

    let url = cfg
        .url
        .as_deref()
        .or(env_url)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| anyhow!("tachyonBrowser.enabled=true requires url or TACHYON_BROWSER_URL"))?
        .trim_end_matches('/')
        .to_string();

    if health_ok(&url, &cfg.health_path).await {
        tracing::info!("using existing tachyon-browser at {url}");
        return Ok(Some(ManagedSidecar::external(url)));
    }

    if !cfg.auto_start {
        anyhow::bail!("tachyon-browser at {url} is not healthy and tachyonBrowser.autoStart=false");
    }

    let command = cfg
        .command
        .as_deref()
        .ok_or_else(|| anyhow!("tachyonBrowser.autoStart=true requires command"))?;

    let mut child = spawn_sidecar(command, cfg)
        .with_context(|| format!("spawning tachyon-browser sidecar via {command}"))?;

    if let Err(err) = wait_until_healthy(&url, &cfg.health_path, cfg.startup_timeout_ms).await {
        let _ = child.start_kill();
        return Err(err);
    }

    tracing::info!("started tachyon-browser sidecar at {url}");
    Ok(Some(ManagedSidecar::owned(url, child)))
}

fn spawn_sidecar(command: &str, cfg: &TachyonBrowserConfig) -> Result<Child> {
    let mut cmd = Command::new(command);
    cmd.args(&cfg.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(dir) = &cfg.working_directory {
        cmd.current_dir(expand_tilde(dir));
    }
    Ok(cmd.spawn()?)
}

async fn wait_until_healthy(url: &str, health_path: &str, timeout_ms: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let mut last_error = String::new();
    while Instant::now() < deadline {
        if health_ok(url, health_path).await {
            return Ok(());
        }
        last_error = format!(
            "{}{} not healthy yet",
            url.trim_end_matches('/'),
            health_path
        );
        sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "timed out waiting for tachyon-browser health at {}{} ({last_error})",
        url.trim_end_matches('/'),
        health_path
    )
}

async fn health_ok(url: &str, health_path: &str) -> bool {
    let full = format!("{}{}", url.trim_end_matches('/'), health_path);
    let Ok(resp) = reqwest::Client::new().get(full).send().await else {
        return false;
    };
    resp.status().is_success()
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_to_disabled() {
        let cfg = TachyonBrowserConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.health_path, "/api/health");
    }

    #[test]
    fn parses_camel_case_config() {
        let cfg: TachyonBrowserConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "autoStart": true,
            "url": "http://127.0.0.1:8400",
            "command": "tachyon-browser",
            "startupTimeoutMs": 1234,
            "workingDirectory": "/tmp"
        }))
        .unwrap();
        assert!(cfg.enabled);
        assert!(cfg.auto_start);
        assert_eq!(cfg.startup_timeout_ms, 1234);
        assert_eq!(cfg.working_directory.as_deref(), Some("/tmp"));
    }
}
