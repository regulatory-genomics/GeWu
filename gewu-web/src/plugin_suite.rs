use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSuiteConfig {
    pub id: String,
    pub enabled: bool,
    pub manifest: Option<String>,
    pub config: Value,
}

impl Default for PluginSuiteConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            manifest: None,
            config: Value::Object(Default::default()),
        }
    }
}

pub fn normalize_plugin_suites(
    raw_suites: Vec<Value>,
    legacy_tachyon_browser: Value,
    legacy_tachyon_suite: Value,
) -> Result<Vec<PluginSuiteConfig>> {
    let mut suites = raw_suites
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<PluginSuiteConfig>, _>>()?;

    let has_tachyon_suite = suites.iter().any(|suite| suite.id == "tachyon");
    if !has_tachyon_suite && legacy_tachyon_enabled(&legacy_tachyon_browser, &legacy_tachyon_suite)
    {
        tracing::warn!(
            "tachyonBrowser/tachyonPluginSuite are deprecated; use pluginSuites[{{ id: 'tachyon', ... }}]"
        );
        suites.push(legacy_tachyon_to_plugin_suite(
            legacy_tachyon_browser,
            legacy_tachyon_suite,
        ));
    }

    Ok(suites)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SidecarConfig {
    pub id: String,
    pub enabled: bool,
    pub url: Option<String>,
    #[serde(rename = "autoStart", alias = "auto_start")]
    pub auto_start: bool,
    pub command: Option<String>,
    pub args: Vec<String>,
    #[serde(rename = "healthPath", alias = "health_path")]
    pub health_path: String,
    #[serde(rename = "startupTimeoutMs", alias = "startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(rename = "workingDirectory", alias = "working_directory")]
    pub working_directory: Option<String>,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            id: "browser".to_string(),
            enabled: true,
            url: None,
            auto_start: false,
            command: None,
            args: Vec::new(),
            health_path: "/api/health".to_string(),
            startup_timeout_ms: 10_000,
            working_directory: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendAssetKind {
    Style,
    Script,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeFrontendAsset {
    pub kind: FrontendAssetKind,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSidecarInfo {
    pub id: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimePluginFrontend {
    pub plugin_id: String,
    pub sidecars: Vec<RuntimeSidecarInfo>,
    pub assets: Vec<RuntimeFrontendAsset>,
}

impl PluginSuiteConfig {
    pub fn browser_sidecar_config(&self) -> Result<Option<SidecarConfig>> {
        let Some(browser) = self.config.get("browser") else {
            return Ok(None);
        };
        let mut sidecar: SidecarConfig = serde_json::from_value(browser.clone())?;
        if sidecar.id.trim().is_empty() {
            sidecar.id = "browser".to_string();
        }
        Ok(Some(sidecar))
    }

    pub fn runtime_frontend(&self) -> Result<RuntimePluginFrontend> {
        let sidecars = match self.browser_sidecar_config()? {
            Some(sidecar) => sidecar
                .url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
                .map(|url| RuntimeSidecarInfo {
                    id: sidecar.id,
                    url: url.to_string(),
                })
                .into_iter()
                .collect(),
            None => Vec::new(),
        };

        let renderer = self.config.get("renderer").cloned().unwrap_or_else(|| json!({}));
        let mut assets = Vec::new();
        if let Some(style) = renderer.get("style").and_then(Value::as_str) {
            assets.push(RuntimeFrontendAsset {
                kind: FrontendAssetKind::Style,
                url: style.to_string(),
            });
        }
        if let Some(script) = renderer.get("script").and_then(Value::as_str) {
            assets.push(RuntimeFrontendAsset {
                kind: FrontendAssetKind::Script,
                url: script.to_string(),
            });
        }

        Ok(RuntimePluginFrontend {
            plugin_id: self.id.clone(),
            sidecars,
            assets,
        })
    }
}

fn legacy_tachyon_enabled(browser: &Value, suite: &Value) -> bool {
    browser
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || suite
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn legacy_tachyon_to_plugin_suite(browser: Value, suite: Value) -> PluginSuiteConfig {
    let renderer = suite.get("renderer").cloned().unwrap_or_else(|| json!({}));
    let browser_config = json!({
        "enabled": browser.get("enabled").cloned().unwrap_or(Value::Bool(true)),
        "url": browser.get("url").cloned().unwrap_or(Value::Null),
        "command": browser.get("command").cloned().unwrap_or(Value::Null),
        "args": browser.get("args").cloned().unwrap_or_else(|| json!([])),
        "autoStart": browser
            .get("autoStart")
            .or_else(|| browser.get("auto_start"))
            .cloned()
            .unwrap_or(Value::Bool(false)),
        "healthPath": browser
            .get("healthPath")
            .or_else(|| browser.get("health_path"))
            .cloned()
            .unwrap_or_else(|| json!("/api/health")),
        "startupTimeoutMs": browser
            .get("startupTimeoutMs")
            .or_else(|| browser.get("startup_timeout_ms"))
            .cloned()
            .unwrap_or_else(|| json!(10_000)),
        "workingDirectory": browser
            .get("workingDirectory")
            .or_else(|| browser.get("working_directory"))
            .cloned()
            .unwrap_or(Value::Null),
    });

    PluginSuiteConfig {
        id: "tachyon".to_string(),
        enabled: true,
        manifest: None,
        config: json!({
            "browser": browser_config,
            "renderer": renderer,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enabled_tachyon_plugin_suite() {
        let raw = serde_json::json!({
            "id": "tachyon",
            "enabled": true,
            "manifest": "/data/litian/project/tachyon-browser/plugin.yaml",
            "config": {
                "registry": "/tmp/atac-registry.json",
                "genes": "/data/litian/ucf_db/gene_info/result/gene_info.ucf",
                "browser": {
                    "url": "http://127.0.0.1:8400",
                    "autoStart": false
                }
            }
        });

        let suite: PluginSuiteConfig = serde_json::from_value(raw).unwrap();

        assert_eq!(suite.id, "tachyon");
        assert!(suite.enabled);
        assert_eq!(
            suite.manifest.as_deref(),
            Some("/data/litian/project/tachyon-browser/plugin.yaml")
        );
        assert_eq!(suite.config["browser"]["url"], "http://127.0.0.1:8400");
    }

    #[test]
    fn legacy_tachyon_config_normalizes_to_single_plugin_suite() {
        let legacy_browser = serde_json::json!({
            "enabled": true,
            "url": "http://127.0.0.1:8400",
            "autoStart": false
        });
        let legacy_suite = serde_json::json!({
            "enabled": true,
            "renderer": {
                    "script": "http://127.0.0.1:8400/assets/tachyon-canvas.iife.js",
                    "style": "http://127.0.0.1:8400/assets/tachyon-canvas.css"
            }
        });

        let suites = normalize_plugin_suites(Vec::new(), legacy_browser, legacy_suite).unwrap();

        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].id, "tachyon");
        assert!(suites[0].enabled);
        assert_eq!(suites[0].config["browser"]["url"], "http://127.0.0.1:8400");
    }

    #[test]
    fn suite_declares_browser_sidecar_and_renderer_assets() {
        let suite: PluginSuiteConfig = serde_json::from_value(serde_json::json!({
            "id": "tachyon",
            "enabled": true,
            "config": {
                "browser": {
                    "url": "http://127.0.0.1:8400",
                    "healthPath": "/api/health",
                    "autoStart": false
                },
                "renderer": {
                    "kind": "genome",
                    "script": "http://127.0.0.1:8400/assets/tachyon-canvas.iife.js",
                    "style": "http://127.0.0.1:8400/assets/tachyon-canvas.css"
                }
            }
        }))
        .unwrap();

        let runtime = suite.runtime_frontend().unwrap();

        assert_eq!(runtime.plugin_id, "tachyon");
        assert_eq!(runtime.sidecars[0].id, "browser");
        assert_eq!(runtime.sidecars[0].url, "http://127.0.0.1:8400");
        assert_eq!(
            runtime.assets[0].url,
            "http://127.0.0.1:8400/assets/tachyon-canvas.css"
        );
        assert_eq!(
            runtime.assets[1].url,
            "http://127.0.0.1:8400/assets/tachyon-canvas.iife.js"
        );
    }
}
