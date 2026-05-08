use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TachyonPluginSuiteConfig {
    pub enabled: bool,
    pub id: String,
    pub renderer: TachyonRendererConfig,
    pub sidecar: TachyonBrowserConfig,
}

impl Default for TachyonPluginSuiteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            id: "tachyon".to_string(),
            renderer: TachyonRendererConfig::default(),
            sidecar: TachyonBrowserConfig::default(),
        }
    }
}

impl TachyonPluginSuiteConfig {
    pub fn effective_sidecar_config(&self, legacy: &TachyonBrowserConfig) -> TachyonBrowserConfig {
        if !self.enabled {
            return legacy.clone();
        }

        let mut sidecar = self.sidecar.clone();
        sidecar.enabled = true;
        sidecar
    }

    pub fn runtime_assets(
        &self,
        browser_url: Option<&str>,
    ) -> crate::fallback::RuntimeFrontendAssets {
        if !self.enabled {
            return crate::fallback::RuntimeFrontendAssets::default();
        }

        let sidecars = browser_url
            .map(ToOwned::to_owned)
            .or_else(|| self.sidecar.url.clone())
            .map(|url| crate::fallback::RuntimeFrontendSidecar {
                id: "browser".to_string(),
                url,
            })
            .into_iter()
            .collect();

        crate::fallback::RuntimeFrontendAssets {
            plugins: vec![crate::fallback::RuntimeFrontendPlugin {
                id: self.id.clone(),
                sidecars,
                styles: self.renderer.style.clone().into_iter().collect(),
                scripts: self.renderer.script.clone().into_iter().collect(),
            }],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TachyonRendererConfig {
    pub kind: String,
    pub script: Option<String>,
    pub style: Option<String>,
}

impl Default for TachyonRendererConfig {
    fn default() -> Self {
        Self {
            kind: "genome".to_string(),
            script: None,
            style: None,
        }
    }
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

    #[test]
    fn parses_tachyon_plugin_suite_config() {
        let cfg: TachyonPluginSuiteConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "id": "tachyon",
            "renderer": {
                "kind": "genome",
                "script": "/plugins/tachyon/tachyon-canvas.iife.js",
                "style": "/plugins/tachyon/tachyon-canvas.css"
            },
            "sidecar": {
                "enabled": true,
                "autoStart": true,
                "url": "http://127.0.0.1:8400",
                "command": "tachyon-browser"
            }
        }))
        .unwrap();

        assert!(cfg.enabled);
        assert_eq!(cfg.id, "tachyon");
        assert_eq!(cfg.renderer.kind, "genome");
        assert_eq!(
            cfg.renderer.script.as_deref(),
            Some("/plugins/tachyon/tachyon-canvas.iife.js")
        );
        assert_eq!(
            cfg.renderer.style.as_deref(),
            Some("/plugins/tachyon/tachyon-canvas.css")
        );
        assert!(cfg.sidecar.enabled);
        assert!(cfg.sidecar.auto_start);
    }

    #[test]
    fn enabled_plugin_suite_forces_sidecar_enabled() {
        let suite: TachyonPluginSuiteConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "sidecar": {
                "url": "http://127.0.0.1:8400"
            }
        }))
        .unwrap();
        let legacy = TachyonBrowserConfig::default();

        let effective = suite.effective_sidecar_config(&legacy);

        assert!(effective.enabled);
        assert_eq!(effective.url.as_deref(), Some("http://127.0.0.1:8400"));
    }

    #[test]
    fn disabled_plugin_suite_keeps_legacy_sidecar_config() {
        let suite = TachyonPluginSuiteConfig::default();
        let legacy: TachyonBrowserConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "url": "http://127.0.0.1:8500"
        }))
        .unwrap();

        let effective = suite.effective_sidecar_config(&legacy);

        assert!(effective.enabled);
        assert_eq!(effective.url.as_deref(), Some("http://127.0.0.1:8500"));
    }

    #[test]
    fn runtime_assets_follow_enabled_plugin_suite() {
        let suite: TachyonPluginSuiteConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "renderer": {
                "script": "/plugins/tachyon/tachyon-canvas.iife.js",
                "style": "/plugins/tachyon/tachyon-canvas.css"
            },
            "sidecar": {
                "url": "http://127.0.0.1:8400"
            }
        }))
        .unwrap();

        let assets = suite.runtime_assets(Some("http://127.0.0.1:8400"));

        assert_eq!(assets.plugins[0].id, "tachyon");
        assert_eq!(assets.plugins[0].sidecars[0].url, "http://127.0.0.1:8400");
        assert_eq!(
            assets.plugins[0].scripts[0],
            "/plugins/tachyon/tachyon-canvas.iife.js"
        );
        assert_eq!(
            assets.plugins[0].styles[0],
            "/plugins/tachyon/tachyon-canvas.css"
        );
    }

    #[test]
    fn runtime_assets_prefer_resolved_browser_url() {
        let suite: TachyonPluginSuiteConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "sidecar": {
                "url": "http://127.0.0.1:8400"
            }
        }))
        .unwrap();

        let assets = suite.runtime_assets(Some("http://127.0.0.1:8500"));

        assert_eq!(assets.plugins[0].sidecars[0].url, "http://127.0.0.1:8500");
    }
}
