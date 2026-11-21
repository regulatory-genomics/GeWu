use anyhow::{Context, Result};
use gewu_providers::ProviderProfile;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: ProviderProfile,
    /// API model ID; defaults to the catalog key.
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Request visible reasoning summaries (OpenAI Responses only; default true).
    pub reasoning_summary: Option<bool>,
    /// Output budget, including reasoning tokens.
    pub max_tokens: Option<u32>,
    /// Context window used by history compaction.
    pub context_limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub models: BTreeMap<String, ModelConfig>,
}

use gewu_core::types::PathEnforcement;
use gewu_tools::mcp::types::McpServerConfig;
use gewu_tools::{DEFAULT_MAX_BASE64_BYTES, DEFAULT_MAX_DIMENSION, ImageLimits};

/// Built-in agent profiles; config-defined agents derive from one of these
/// and cannot shadow their names.
pub const BUILTIN_AGENT_NAMES: &[&str] = &["build", "plan", "explore", "general"];

/// A user-defined agent from the config file: a base profile plus optional
/// overrides. Spawnable through the `task` tool; primary use is explicit opt-in.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEntry {
    /// Allow this profile to be selected as a primary session agent.
    #[serde(default)]
    pub primary: bool,
    /// Built-in profile providing the tool set.
    pub base: String,
    /// Shown to the model in the task tool's agent list.
    pub description: String,
    /// Replaces the base profile's system prompt when present.
    pub instructions: Option<String>,
    /// Narrows the base profile's tool set to these names.
    pub tools: Option<Vec<String>>,
}

/// Image attachment limits for `read_file`, mirroring opencode's
/// `attachment.image` block.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    /// Downscale oversized images instead of refusing them.
    #[serde(default = "default_auto_resize")]
    pub auto_resize: bool,
    /// Longest allowed edge, in pixels.
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    /// Longest allowed edge, in pixels.
    #[serde(default = "default_max_height")]
    pub max_height: u32,
    /// Cap on the base64 payload handed to the model.
    #[serde(default = "default_max_base64_bytes")]
    pub max_base64_bytes: usize,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            auto_resize: default_auto_resize(),
            max_width: default_max_width(),
            max_height: default_max_height(),
            max_base64_bytes: default_max_base64_bytes(),
        }
    }
}

impl From<&ImageConfig> for ImageLimits {
    fn from(config: &ImageConfig) -> Self {
        Self {
            auto_resize: config.auto_resize,
            max_width: config.max_width,
            max_height: config.max_height,
            max_base64_bytes: config.max_base64_bytes,
        }
    }
}

fn default_auto_resize() -> bool {
    true
}

fn default_max_width() -> u32 {
    DEFAULT_MAX_DIMENSION
}

fn default_max_height() -> u32 {
    DEFAULT_MAX_DIMENSION
}

fn default_max_base64_bytes() -> usize {
    DEFAULT_MAX_BASE64_BYTES
}

/// Attachment handling settings (opencode's `attachment` block).
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AttachmentConfig {
    #[serde(default)]
    pub image: ImageConfig,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub endpoints: BTreeMap<String, EndpointConfig>,
    pub model: Option<String>,
    /// Optional path to a log file recording every wire LLM request as JSONL.
    pub log: Option<String>,
    /// Optional directories to scan for skills (files named `SKILL.md`).
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Named filesystem roots available to sessions in addition to `cwd`.
    /// Relative paths are resolved against the server's startup directory.
    #[serde(default)]
    pub workspaces: BTreeMap<String, String>,
    /// File-path confinement for tools and file-tree operations:
    /// `"none"` (default) applies no checks, `"contain"` rejects paths that
    /// escape the working directory, `"strict"` also canonicalizes and
    /// rejects symlinks.
    #[serde(default)]
    pub sandbox: PathEnforcement,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub plugins: BTreeMap<String, crate::plugins::package::PluginEntry>,
    #[serde(rename = "pluginHost", default)]
    pub plugin_host: crate::plugins::stdio::Limits,
    /// User-defined agents keyed by name; spawnable via the `task` tool.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentEntry>,
    /// Image limits for `read_file` attachments.
    #[serde(default)]
    pub attachment: AttachmentConfig,
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
        config.validate()?;

        Ok(LoadedConfig {
            config,
            path: Some(path),
        })
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let image = &self.attachment.image;
        anyhow::ensure!(
            image.max_width > 0 && image.max_height > 0,
            "attachment.image max_width and max_height must be positive"
        );
        anyhow::ensure!(
            image.max_base64_bytes > 0,
            "attachment.image max_base64_bytes must be positive"
        );
        for (name, path) in &self.workspaces {
            anyhow::ensure!(
                name != "cwd"
                    && !name.trim().is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "workspace name '{name}' must use letters, digits, '-' or '_' and cannot be 'cwd'"
            );
            anyhow::ensure!(
                !path.trim().is_empty(),
                "workspace '{name}' path cannot be empty"
            );
        }
        if let Some(model) = &self.model {
            self.resolve_model(model)?;
        }
        for (endpoint, settings) in &self.endpoints {
            anyhow::ensure!(
                !endpoint.trim().is_empty() && !endpoint.contains('/'),
                "endpoint names must be nonempty and cannot contain '/'"
            );
            anyhow::ensure!(
                settings.base_url.starts_with("https://")
                    || settings.base_url.starts_with("http://"),
                "endpoint '{endpoint}' needs an HTTP(S) base_url"
            );
            for (name, model) in &settings.models {
                anyhow::ensure!(
                    !name.trim().is_empty(),
                    "model catalog keys cannot be empty"
                );
                anyhow::ensure!(
                    model.model.as_ref().is_none_or(|id| !id.trim().is_empty()),
                    "API model ID for '{name}' cannot be empty"
                );
                anyhow::ensure!(
                    model.max_tokens != Some(0) && model.context_limit != Some(0),
                    "model '{endpoint}/{name}' limits must be positive"
                );
                if model.reasoning_summary.is_some() {
                    anyhow::ensure!(
                        model.provider == ProviderProfile::Openai,
                        "reasoning_summary is only supported by openai models ({endpoint}/{name})"
                    );
                }
            }
        }
        for (name, agent) in &self.agents {
            anyhow::ensure!(
                !name.trim().is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "agent name '{name}' must be nonempty and use only letters, digits, '-' or '_'"
            );
            anyhow::ensure!(
                !BUILTIN_AGENT_NAMES.contains(&name.as_str()),
                "agent name '{name}' collides with a built-in profile"
            );
            anyhow::ensure!(
                BUILTIN_AGENT_NAMES.contains(&agent.base.as_str()),
                "agent '{name}': unknown base profile '{}' (expected one of {})",
                agent.base,
                BUILTIN_AGENT_NAMES.join(", ")
            );
            anyhow::ensure!(
                !agent.description.trim().is_empty(),
                "agent '{name}' needs a nonempty description"
            );
            if let Some(instructions) = &agent.instructions {
                anyhow::ensure!(
                    !instructions.trim().is_empty(),
                    "agent '{name}': instructions cannot be empty"
                );
            }
            if let Some(tools) = &agent.tools {
                anyhow::ensure!(
                    agent.primary || !tools.is_empty(),
                    "agent '{name}': tools cannot be an empty list (omit it to keep the base profile's tools)"
                );
                for tool in tools {
                    anyhow::ensure!(
                        !tool.trim().is_empty(),
                        "agent '{name}': tool names cannot be empty"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn resolve_model(&self, selected: &str) -> Result<(&EndpointConfig, &ModelConfig, &str)> {
        let (endpoint, name) = selected
            .split_once('/')
            .context("model selector must be endpoint/model")?;
        let settings = self
            .endpoints
            .get(endpoint)
            .with_context(|| format!("unknown endpoint '{endpoint}'"))?;
        let (key, model) = settings
            .models
            .get_key_value(name)
            .with_context(|| format!("unknown model '{selected}'"))?;
        Ok((settings, model, key.as_str()))
    }
}

/// Resolve the implicit startup root and every configured workspace root.
/// Paths are canonicalized once so containment checks use stable roots.
pub fn resolve_workspace_roots(
    config: &Config,
    startup_directory: impl AsRef<std::path::Path>,
) -> Result<BTreeMap<String, PathBuf>> {
    config.validate()?;
    let startup_directory =
        std::fs::canonicalize(startup_directory.as_ref()).with_context(|| {
            format!(
                "cannot resolve startup working directory: {}",
                startup_directory.as_ref().display()
            )
        })?;
    anyhow::ensure!(
        startup_directory.is_dir(),
        "startup working directory is not a directory: {}",
        startup_directory.display()
    );

    let mut roots = BTreeMap::from([(String::from("cwd"), startup_directory.clone())]);
    for (name, configured_path) in &config.workspaces {
        let path = PathBuf::from(configured_path);
        let path = if path.is_absolute() {
            path
        } else {
            startup_directory.join(path)
        };
        let path = std::fs::canonicalize(&path).with_context(|| {
            format!(
                "cannot resolve workspace '{name}' directory: {}",
                path.display()
            )
        })?;
        anyhow::ensure!(
            path.is_dir(),
            "workspace '{name}' is not a directory: {}",
            path.display()
        );
        roots.insert(name.clone(), path);
    }
    Ok(roots)
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
    fn image_attachment_limits_default_and_override() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(
            ImageLimits::from(&config.attachment.image),
            ImageLimits::default()
        );

        let config: Config = serde_json::from_value(serde_json::json!({
            "attachment": {"image": {
                "auto_resize": false,
                "max_width": 800,
                "max_height": 600,
                "max_base64_bytes": 1_048_576
            }}
        }))
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            ImageLimits::from(&config.attachment.image),
            ImageLimits {
                auto_resize: false,
                max_width: 800,
                max_height: 600,
                max_base64_bytes: 1_048_576,
            }
        );

        let partial: Config = serde_json::from_value(serde_json::json!({
            "attachment": {"image": {"max_width": 640}}
        }))
        .unwrap();
        assert_eq!(partial.attachment.image.max_width, 640);
        assert_eq!(
            partial.attachment.image.max_height,
            ImageLimits::default().max_height
        );
        serde_json::from_value::<Config>(serde_json::json!({
            "attachment": {"image": {"max_size": 1}}
        }))
        .expect_err("unknown attachment.image keys must be rejected");
    }

    #[test]
    fn image_attachment_limits_must_be_positive() {
        for config in [
            serde_json::json!({"attachment": {"image": {"max_width": 0}}}),
            serde_json::json!({"attachment": {"image": {"max_height": 0}}}),
            serde_json::json!({"attachment": {"image": {"max_base64_bytes": 0}}}),
        ] {
            let config: Config = serde_json::from_value(config).unwrap();
            assert!(config.validate().is_err(), "{config:?} should not validate");
        }
    }

    #[test]
    fn validates_model_options_and_selectors() {
        let mut config: Config = serde_json::from_value(serde_json::json!({"endpoints":{"shared":{
            "base_url":"https://example.com/v1","models":{"vendor/model":{"provider":"openai", "max_tokens":1}}
        }}})).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.resolve_model("shared/vendor/model").unwrap().2,
            "vendor/model"
        );
        for selected in ["shared", "missing/model", "shared/missing"] {
            assert!(config.resolve_model(selected).is_err());
        }
        config
            .endpoints
            .get_mut("shared")
            .unwrap()
            .models
            .get_mut("vendor/model")
            .unwrap()
            .max_tokens = Some(0);
        assert!(config.validate().is_err());
        let config: Config = serde_json::from_value(serde_json::json!({"endpoints":{"shared":{
            "base_url":"https://example.com/v1","models":{"glm":{"provider":"glm", "reasoning_effort":"high"}}
        }}})).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn models_require_known_providers() {
        assert!(serde_json::from_str::<ModelConfig>(r#"{}"#).is_err());
        assert!(serde_json::from_str::<ModelConfig>(r#"{"provider":"typo"}"#).is_err());
        let config: Config = serde_json::from_str(r#"{"model":"west/m","endpoints":{"west":{"base_url":"https://example.com/v1","models":{"m":{"provider":"deepseek"}}}}}"#).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.endpoints["west"].models["m"].provider,
            ProviderProfile::Deepseek
        );
        assert!(config.endpoints["west"].models["m"].model.is_none());
    }

    #[test]
    fn rejects_invalid_defaults_and_removed_settings() {
        let config: Config = serde_json::from_str(r#"{"model":"missing"}"#).unwrap();
        assert!(config.validate().is_err());
        for field in [
            "reasoning_field",
            "require_reasoning",
            "provider",
            "api_key",
            "base_url",
        ] {
            assert!(serde_json::from_str::<Config>(&format!(r#"{{"{field}":null}}"#)).is_err());
        }
    }

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
    fn parses_max_tokens_option() {
        let config: ModelConfig =
            serde_json::from_str(r#"{"provider":"openai", "max_tokens": 65536}"#).unwrap();
        assert_eq!(config.max_tokens, Some(65536));
    }

    #[test]
    fn parses_context_limit_option() {
        let config: ModelConfig =
            serde_json::from_str(r#"{"provider":"glm", "context_limit": 131072}"#).unwrap();
        assert_eq!(config.context_limit, Some(131_072));
        let config: ModelConfig = serde_json::from_str(r#"{"provider":"glm"}"#).unwrap();
        assert_eq!(config.context_limit, None);
    }

    #[test]
    fn max_tokens_is_optional() {
        let config: ModelConfig = serde_json::from_str(r#"{"provider":"glm"}"#).unwrap();
        assert!(config.max_tokens.is_none());
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
    fn sandbox_defaults_to_no_enforcement() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.sandbox, PathEnforcement::None);
    }

    #[test]
    fn parses_sandbox_levels() {
        for (text, expected) in [
            (r#""none""#, PathEnforcement::None),
            (r#""contain""#, PathEnforcement::Contain),
            (r#""strict""#, PathEnforcement::Strict),
        ] {
            let config: Config = serde_json::from_str(&format!("{{\"sandbox\": {text}}}")).unwrap();
            assert_eq!(config.sandbox, expected, "parsing {text}");
        }
        assert!(serde_json::from_str::<Config>("{\"sandbox\": \"yolo\"}").is_err());
    }

    #[test]
    fn parses_agents_option() {
        let config: Config = serde_json::from_str(
            r#"{"agents":{"reviewer":{"base":"general","description":"Reviews diffs","instructions":"Be meticulous","tools":["read_file","grep"]}}}"#,
        )
        .unwrap();
        let agent = config.agents.get("reviewer").unwrap();
        assert_eq!(agent.base, "general");
        assert_eq!(agent.description, "Reviews diffs");
        assert_eq!(agent.instructions.as_deref(), Some("Be meticulous"));
        assert_eq!(
            agent.tools.as_ref().unwrap(),
            &["read_file".to_string(), "grep".to_string()]
        );
    }

    #[test]
    fn workspace_names_are_validated() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "workspaces": {
                "frontend": "../frontend",
                "bad/name": "../bad"
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("workspace name"), "{error}");
    }

    #[test]
    fn workspace_paths_resolve_against_startup_directory() {
        let root = tempfile::tempdir().unwrap();
        let frontend = root.path().join("frontend");
        std::fs::create_dir(&frontend).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "workspaces": {"frontend": "frontend"}
        }))
        .unwrap();

        let roots = super::resolve_workspace_roots(&config, root.path()).unwrap();
        assert_eq!(roots.get("cwd"), Some(&root.path().canonicalize().unwrap()));
        assert_eq!(
            roots.get("frontend"),
            Some(&frontend.canonicalize().unwrap())
        );
    }

    #[test]
    fn workspace_paths_must_be_existing_directories() {
        let root = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "workspaces": {"missing": "does-not-exist"}
        }))
        .unwrap();
        let error = super::resolve_workspace_roots(&config, root.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("workspace 'missing'"), "{error}");
    }

    #[test]
    fn no_workspaces_config_exposes_only_cwd() {
        let root = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_str("{}").unwrap();
        let roots = super::resolve_workspace_roots(&config, root.path()).unwrap();
        assert_eq!(roots.len(), 1);
        assert!(roots.contains_key("cwd"));
    }

    #[test]
    fn agents_is_optional() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.agents.is_empty());
    }

    #[test]
    fn agent_entries_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<Config>(
                r#"{"agents":{"r":{"base":"general","description":"d","model":"x"}}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn validates_agent_entries() {
        serde_json::from_str::<Config>(
            r#"{"agents":{"reviewer":{"base":"explore","description":"d"}}}"#,
        )
        .unwrap()
        .validate()
        .unwrap();

        for (text, needle) in [
            (
                r#"{"agents":{"":{"base":"general","description":"d"}}}"#,
                "nonempty",
            ),
            (
                r#"{"agents":{"bad name":{"base":"general","description":"d"}}}"#,
                "letters, digits",
            ),
            (
                r#"{"agents":{"build":{"base":"general","description":"d"}}}"#,
                "collides",
            ),
            (
                r#"{"agents":{"reviewer":{"base":"nope","description":"d"}}}"#,
                "unknown base profile",
            ),
            (
                r#"{"agents":{"reviewer":{"base":"general","description":"  "}}}"#,
                "nonempty description",
            ),
            (
                r#"{"agents":{"reviewer":{"base":"general","description":"d","instructions":"  "}}}"#,
                "instructions cannot be empty",
            ),
            (
                r#"{"agents":{"reviewer":{"base":"general","description":"d","tools":[]}}}"#,
                "tools cannot be an empty list",
            ),
            (
                r#"{"agents":{"reviewer":{"base":"general","description":"d","tools":["read_file","  "]}}}"#,
                "tool names cannot be empty",
            ),
        ] {
            let err = serde_json::from_str::<Config>(text)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "unexpected error for {text}: {err}");
        }
    }
}
