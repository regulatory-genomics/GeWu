//! Shared startup logic used by both `run` and `serve` subcommands.
//!
//! Resolves configuration, initializes the LLM provider, opens the SQLite
//! pool, discovers skills, and opens the optional LLM request log.

use anyhow::{Context, Result};
use gewu_core::traits::{LlmProvider, RequestLog};
use gewu_core::types::PathEnforcement;
use gewu_protocol::{PROVIDER_DEFAULT_EFFORT, REASONING_EFFORTS};
use gewu_providers::{
    AnthropicProvider, ChatCompletionsProvider, DeepSeekProvider, GlmProvider, MoonshotProvider,
    OpenAIProvider, ProviderProfile,
};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::BTreeMap;
use std::env;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::LoadedConfig;

/// CLI flags shared by the `run` and `serve` subcommands.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct CommonArgs {
    /// Custom OpenAI Base URL
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub base_url: Option<String>,

    /// OpenAI API Key
    #[arg(long, env = "OPENAI_API_KEY")]
    pub api_key: Option<String>,

    /// OpenAI Endpoint URL
    #[arg(long, env = "OPENAI_ENDPOINT")]
    pub endpoint: Option<String>,

    /// Custom Model Name
    #[arg(long, short, env = "OPENAI_MODEL")]
    pub model: Option<String>,

    /// Provider profile for direct CLI use (configured models own their provider)
    #[arg(long, env = "GEWU_PROVIDER")]
    pub provider: Option<String>,

    /// Initial Agent (default: build)
    #[arg(long, default_value = "build")]
    pub agent: String,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Path to SQLite database file
    #[arg(long)]
    pub db_path: Option<String>,

    /// Path to LLM configuration file
    #[arg(long)]
    pub llm_config: Option<String>,
}

/// One switchable model: a ready provider plus the settings agents need.
#[derive(Clone)]
pub struct ModelEntry {
    /// Provider for the model's configured default effort.
    pub provider: Arc<dyn LlmProvider>,
    pub provider_name: String,
    /// API model id (or catalog key) shown in UIs.
    pub label: String,
    /// Resolved context window used for compaction.
    pub context_limit: usize,
    /// Effort configured as the model default; `None` sends no effort field.
    pub default_effort: Option<String>,
    /// Providers for the selectable explicit efforts.
    pub effort_providers: BTreeMap<String, Arc<dyn LlmProvider>>,
    /// Whether the model accepts runtime reasoning-effort changes.
    pub supports_reasoning_effort: bool,
}

impl ModelEntry {
    /// The provider for an explicit effort, or the configured default when
    /// `effort` is `None`. [`PROVIDER_DEFAULT_EFFORT`] selects the provider
    /// built without an effort field. Returns `None` for an unsupported effort.
    pub fn provider_for(&self, effort: Option<&str>) -> Option<Arc<dyn LlmProvider>> {
        match effort {
            None => Some(self.provider.clone()),
            // The configured effort's provider is the default one.
            Some(effort) if self.default_effort.as_deref() == Some(effort) => {
                Some(self.provider.clone())
            }
            Some(effort) => self.effort_providers.get(effort).cloned(),
        }
    }

    /// Whether `effort` can be selected for this model at runtime.
    pub fn supports_effort(&self, effort: &str) -> bool {
        self.default_effort.as_deref() == Some(effort) || self.effort_providers.contains_key(effort)
    }
}

/// AgentBuilder's default context window, used when a model configures none.
const DEFAULT_CONTEXT_LIMIT: usize = 256_000;

/// Everything needed to build agents, resolved from CLI args + config + env.
pub struct Bootstrap {
    pub provider: Arc<dyn LlmProvider>,
    pub pool: SqlitePool,
    pub mcp_tools: Vec<Arc<dyn gewu_core::traits::Tool>>,
    pub plugins: Arc<crate::plugins::PluginHost>,
    pub skills: Vec<gewu_tools::SkillInfo>,
    pub log_writer: Option<RequestLog>,
    pub model: String,
    pub provider_name: String,
    /// Working directory all tools resolve paths against; captured once at
    /// startup so the UI can show it.
    pub working_directory: String,
    /// Canonical named roots available to sessions; always contains `cwd`.
    pub workspace_roots: Arc<BTreeMap<String, std::path::PathBuf>>,
    /// File-path confinement level for tools and file-tree operations.
    pub enforcement: PathEnforcement,
    /// Image limits applied by `read_file` before media reaches the model.
    pub image_limits: gewu_tools::ImageLimits,
    /// Model context window (tokens) forwarded to agents for compaction;
    /// `None` keeps the agent's built-in default.
    pub context_limit: Option<usize>,
    /// Every switchable model by `endpoint/model` selector. Always contains
    /// the default model, so sessions can always resolve their selector.
    pub models: BTreeMap<String, ModelEntry>,
    /// The default model's selector (the key it occupies in `models`).
    pub model_selector: String,
    /// User-defined agents from the config file, spawnable via the `task`
    /// tool.
    pub agents: Arc<BTreeMap<String, crate::config::AgentEntry>>,
    /// Server-owned directory where ask attachments are stored, one
    /// subdirectory per session, outside every workspace.
    pub attachment_store: std::path::PathBuf,
}

/// Initialize tracing from the `--log-level` flag.
///
/// Idempotent: safe to call from both a subcommand handler and the shared
/// `bootstrap()` path. When a global dispatcher is already installed (e.g.
/// `run` initialized it before calling `bootstrap()`), the call is a no-op
/// instead of failing with "a global default trace dispatcher has already
/// been set".
pub fn init_tracing(log_level: &str) -> Result<()> {
    let level = match log_level.to_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };
    let subscriber = tracing_subscriber::fmt().with_max_level(level).finish();
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        tracing::debug!("tracing already initialized; keeping the existing dispatcher");
    }
    Ok(())
}

/// Resolve the SQLite database path from CLI args or the default location.
pub fn resolve_db_path(db_path: Option<String>) -> Result<String> {
    if let Some(path) = db_path {
        return Ok(path);
    }
    let home_dir = dirs::home_dir()
        .context("Failed to determine home directory. Please provide --db-path explicitly.")?;
    let gewu_dir = home_dir.join(".gewu");

    if !gewu_dir.exists() {
        std::fs::create_dir_all(&gewu_dir).context("Failed to create .gewu directory")?;
    }

    Ok(gewu_dir.join("gewu.db").to_string_lossy().to_string())
}

/// Resolve the list of skill directories from config, making relative paths
/// absolute against the current working directory.
pub fn resolve_skill_paths(config: &crate::config::Config) -> Vec<std::path::PathBuf> {
    config
        .skills
        .iter()
        .flatten()
        .map(|p| {
            let path = std::path::PathBuf::from(p);
            if path.is_absolute() {
                path
            } else {
                env::current_dir()
                    .map(|dir| dir.join(path))
                    .unwrap_or_else(|_| std::path::PathBuf::from(p))
            }
        })
        .collect()
}

/// Resolve provider/model settings and build the LLM provider.
///
/// Priority: CLI args > environment variables > config file > defaults.
pub fn build_provider(
    args: &CommonArgs,
    file_config: &crate::config::Config,
) -> Result<(Arc<dyn LlmProvider>, String, String)> {
    file_config.validate()?;
    let selected = args
        .model
        .clone()
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .or_else(|| file_config.model.clone())
        .context("select a configured model with --model or the config's model field")?;
    let resolved = if file_config.endpoints.is_empty() {
        None
    } else {
        Some(file_config.resolve_model(&selected)?)
    };
    let entry = resolved.map(|(_, entry, _)| entry);
    let override_provider = args
        .provider
        .clone()
        .or_else(|| env::var("GEWU_PROVIDER").ok());
    let profile = if let Some(entry) = entry {
        if let Some(name) = &override_provider {
            anyhow::ensure!(
                name.parse::<ProviderProfile>()
                    .map_err(anyhow::Error::msg)?
                    == entry.provider,
                "provider override conflicts with provider assigned to model '{selected}'"
            );
        }
        entry.provider
    } else {
        override_provider
            .context("--provider is required for direct CLI model selection")?
            .parse::<ProviderProfile>()
            .map_err(anyhow::Error::msg)?
    };
    let settings = resolved.map(|(endpoint, _, _)| endpoint);
    let model = entry
        .and_then(|entry| entry.model.clone())
        .unwrap_or_else(|| {
            resolved
                .map(|(_, _, name)| name.to_string())
                .unwrap_or(selected)
        });
    let max_tokens = entry.and_then(|entry| entry.max_tokens);
    let api_key = args
        .api_key
        .clone()
        .or_else(|| env::var(profile.api_key_env()).ok())
        .or_else(|| settings.and_then(|settings| settings.api_key.clone()))
        .with_context(|| {
            format!(
                "API key required: use --api-key, {}, or endpoints.<name>.api_key",
                profile.api_key_env()
            )
        })?;

    let base_url = args
        .base_url
        .clone()
        .or_else(|| env::var("OPENAI_BASE_URL").ok())
        .or(args.endpoint.clone())
        .or_else(|| env::var("OPENAI_ENDPOINT").ok())
        .or_else(|| settings.map(|settings| settings.base_url.clone()))
        .or_else(|| profile.base_url().map(str::to_string))
        .with_context(|| format!("providers.{}.base_url is required", profile.id()))?;

    let provider_name = profile.id().to_string();

    let provider = construct_provider(
        profile,
        &api_key,
        &base_url,
        &model,
        max_tokens,
        entry.and_then(|entry| entry.reasoning_effort.as_deref()),
        entry.and_then(|entry| entry.reasoning_summary),
    )?;

    Ok((provider, provider_name, model))
}

/// Construct the provider for one resolved model entry.
fn construct_provider(
    profile: ProviderProfile,
    api_key: &str,
    base_url: &str,
    model: &str,
    max_tokens: Option<u32>,
    reasoning_effort: Option<&str>,
    reasoning_summary: Option<bool>,
) -> Result<Arc<dyn LlmProvider>> {
    let provider: Arc<dyn LlmProvider> = match profile {
        ProviderProfile::Anthropic => {
            let mut builder = AnthropicProvider::new_with_endpoint(api_key, base_url, model);
            if let Some(effort) = reasoning_effort {
                builder = builder.with_reasoning_effort(effort);
            }
            if let Some(max_tokens) = max_tokens {
                builder = builder.with_max_tokens(max_tokens);
            }
            info!(
                "Anthropic max_tokens: {} (override with `max_tokens` in the config file)",
                builder.max_tokens()
            );
            Arc::new(builder)
        }
        ProviderProfile::Deepseek => {
            let mut provider = DeepSeekProvider::new_with_endpoint(api_key, base_url, model)
                .with_max_tokens(max_tokens);
            if let Some(effort) = reasoning_effort {
                provider = provider.with_reasoning_effort(effort);
            }
            Arc::new(provider)
        }
        ProviderProfile::Glm => {
            let mut provider = GlmProvider::new_with_endpoint(api_key, base_url, model)
                .with_max_tokens(max_tokens);
            if let Some(effort) = reasoning_effort {
                provider = provider.with_reasoning_effort(effort);
            }
            Arc::new(provider)
        }
        ProviderProfile::Moonshot => {
            let mut provider = MoonshotProvider::new_with_endpoint(api_key, base_url, model)
                .with_max_tokens(max_tokens);
            if let Some(effort) = reasoning_effort {
                provider = provider.with_reasoning_effort(effort);
            }
            Arc::new(provider)
        }
        ProviderProfile::Openai => {
            let mut provider = OpenAIProvider::new_with_endpoint(api_key, base_url, model);
            if let Some(enabled) = reasoning_summary {
                provider = provider.with_reasoning_summary(enabled);
            }
            if let Some(limit) = max_tokens {
                provider = provider.with_max_output_tokens(limit)?;
            }
            if let Some(effort) = reasoning_effort {
                provider = provider.with_reasoning_effort(effort)?;
            }
            Arc::new(provider)
        }
        ProviderProfile::OpenaiCompatible => {
            let mut provider = ChatCompletionsProvider::new_with_endpoint(api_key, base_url, model)
                .with_max_tokens(max_tokens);
            if let Some(effort) = reasoning_effort {
                provider = provider.with_reasoning_effort(effort);
            }
            Arc::new(provider)
        }
    };
    Ok(provider)
}

/// Build a provider for every configured endpoint model so sessions can
/// switch models at runtime. Models whose API key cannot be resolved (or
/// whose options fail provider construction) are skipped with a warning;
/// the default model is guaranteed present via `bootstrap()`.
pub fn build_model_catalog(
    args: &CommonArgs,
    file_config: &crate::config::Config,
) -> BTreeMap<String, ModelEntry> {
    let mut catalog = BTreeMap::new();
    for (endpoint_name, settings) in &file_config.endpoints {
        for (key, model_config) in &settings.models {
            let selector = format!("{endpoint_name}/{key}");
            let profile = model_config.provider;
            let Some(api_key) = args
                .api_key
                .clone()
                .or_else(|| env::var(profile.api_key_env()).ok())
                .or_else(|| settings.api_key.clone())
            else {
                warn!(
                    "model '{selector}' is not switchable: no API key (set endpoints.{endpoint_name}.api_key or {})",
                    profile.api_key_env()
                );
                continue;
            };
            let label = model_config.model.clone().unwrap_or_else(|| key.clone());
            // A configured `provider-default` is the same as configuring no
            // effort; never send the sentinel to the provider.
            let configured_effort = model_config
                .reasoning_effort
                .as_deref()
                .filter(|effort| *effort != PROVIDER_DEFAULT_EFFORT);
            match construct_provider(
                profile,
                &api_key,
                &settings.base_url,
                &label,
                model_config.max_tokens,
                configured_effort,
                model_config.reasoning_summary,
            ) {
                Ok(provider) => {
                    let default_effort = configured_effort.map(str::to_string);
                    // OpenAI-family and Anthropic models expose the control
                    // by default; other services opt in through config.
                    let supports_reasoning_effort = default_effort.is_some()
                        || matches!(
                            profile,
                            ProviderProfile::Openai
                                | ProviderProfile::OpenaiCompatible
                                | ProviderProfile::Anthropic
                        );
                    let mut effort_providers = BTreeMap::new();
                    if supports_reasoning_effort {
                        if let Ok(provider) = construct_provider(
                            profile,
                            &api_key,
                            &settings.base_url,
                            &label,
                            model_config.max_tokens,
                            None,
                            model_config.reasoning_summary,
                        ) {
                            effort_providers.insert(PROVIDER_DEFAULT_EFFORT.to_string(), provider);
                        }
                        for effort in REASONING_EFFORTS {
                            match construct_provider(
                                profile,
                                &api_key,
                                &settings.base_url,
                                &label,
                                model_config.max_tokens,
                                Some(*effort),
                                model_config.reasoning_summary,
                            ) {
                                Ok(provider) => {
                                    effort_providers.insert((*effort).to_string(), provider);
                                }
                                Err(e) => warn!(
                                    "model '{selector}' effort '{effort}' is unavailable: {e}"
                                ),
                            }
                        }
                    }
                    catalog.insert(
                        selector.clone(),
                        ModelEntry {
                            provider,
                            provider_name: profile.id().to_string(),
                            label,
                            context_limit: model_config
                                .context_limit
                                .unwrap_or(DEFAULT_CONTEXT_LIMIT),
                            default_effort,
                            effort_providers,
                            supports_reasoning_effort,
                        },
                    );
                }
                Err(e) => warn!("model '{selector}' is not switchable: {e}"),
            }
        }
    }
    catalog
}

fn selected_context_limit(
    args: &CommonArgs,
    config: &crate::config::Config,
) -> Result<Option<usize>> {
    if config.endpoints.is_empty() {
        return Ok(None);
    }
    let selected = args
        .model
        .clone()
        .or_else(|| env::var("OPENAI_MODEL").ok())
        .or_else(|| config.model.clone())
        .context("select a configured model")?;
    Ok(config.resolve_model(&selected)?.1.context_limit)
}

/// Open the optional LLM request log file when configured.
pub fn open_request_log(file_config: &crate::config::Config) -> Result<Option<RequestLog>> {
    let Some(path) = &file_config.log else {
        return Ok(None);
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open LLM request log file: {}", path))?;
    info!("Logging LLM requests to: {}", path);
    Ok(Some(Arc::new(Mutex::new(Box::new(file)))))
}

/// Perform the full startup sequence shared by `run` and `serve`.
pub async fn bootstrap(args: &CommonArgs) -> Result<Bootstrap> {
    bootstrap_with_host_version(args, env!("CARGO_PKG_VERSION")).await
}

pub async fn bootstrap_with_host_version(args: &CommonArgs, version: &str) -> Result<Bootstrap> {
    init_tracing(&args.log_level)?;

    let loaded_config = LoadedConfig::load(args.llm_config.as_deref())?;
    if let Some(path) = &loaded_config.path {
        info!("Loaded config from: {}", path.display());
    } else {
        info!("No config file found");
    }
    let file_config = &loaded_config.config;
    let plugins = crate::plugins::PluginHost::from_config(
        &file_config.plugins,
        loaded_config.path.as_deref(),
        file_config.plugin_host.clone(),
    )?
    .with_host_version(version);
    let startup_directory = env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let workspace_roots = Arc::new(crate::config::resolve_workspace_roots(
        file_config,
        &startup_directory,
    )?);

    let (provider, provider_name, model) = build_provider(args, file_config)?;
    info!("Using model: {}", model);
    info!("Using provider: {}", provider_name);

    let context_limit = selected_context_limit(args, file_config)?;
    let mut models = build_model_catalog(args, file_config);
    // The default model's catalog selector. Direct CLI use (no configured
    // endpoints) has no selector, so the default joins the catalog under its
    // display name.
    let model_selector = if file_config.endpoints.is_empty() {
        let selector = model.clone();
        models.insert(
            selector.clone(),
            ModelEntry {
                provider: provider.clone(),
                provider_name: provider_name.clone(),
                label: model.clone(),
                context_limit: context_limit.unwrap_or(DEFAULT_CONTEXT_LIMIT),
                default_effort: None,
                effort_providers: BTreeMap::new(),
                supports_reasoning_effort: false,
            },
        );
        selector
    } else {
        // `build_provider` validated the selection, so the entry exists.
        let selected = args
            .model
            .clone()
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or_else(|| file_config.model.clone())
            .expect("build_provider validated the selected model");
        models
            .entry(selected.clone())
            .or_insert_with(|| ModelEntry {
                provider: provider.clone(),
                provider_name: provider_name.clone(),
                label: model.clone(),
                context_limit: context_limit.unwrap_or(DEFAULT_CONTEXT_LIMIT),
                default_effort: None,
                effort_providers: BTreeMap::new(),
                supports_reasoning_effort: false,
            });
        selected
    };
    info!(
        "Switchable models: {}",
        models.keys().cloned().collect::<Vec<_>>().join(", ")
    );

    let db_path = resolve_db_path(args.db_path.clone())?;
    info!("Using database: {}", db_path);
    let db_url = format!("sqlite://{}?mode=rwc", db_path);
    let pool = SqlitePoolOptions::new()
        .connect(&db_url)
        .await
        .context("Failed to connect to SQLite database")?;

    // Ask attachments live next to the database, in per-session
    // subdirectories. Prune sessions untouched for a week so the store does
    // not grow without bound.
    let attachment_store = std::path::Path::new(&db_path)
        .parent()
        .map(|dir| dir.join("attachments"))
        .unwrap_or_else(|| std::path::PathBuf::from(".gewu/attachments"));
    prune_attachment_store(&attachment_store);

    let mcp_tools =
        crate::mcp_loader::load_mcp_tools(loaded_config.config.mcp_servers.clone()).await?;
    let log_writer = open_request_log(file_config)?;

    let skill_paths = resolve_skill_paths(file_config);
    let skills = if skill_paths.is_empty() {
        Vec::new()
    } else {
        let found = gewu_tools::skill::discover(&skill_paths);
        info!("Discovered {} skills", found.len());
        found
    };

    Ok(Bootstrap {
        provider,
        pool: pool.clone(),
        mcp_tools,
        plugins: Arc::new(plugins.with_store(pool)),
        skills,
        log_writer,
        model,
        provider_name,
        working_directory: workspace_roots["cwd"].to_string_lossy().to_string(),
        workspace_roots,
        enforcement: loaded_config.config.sandbox,
        image_limits: (&loaded_config.config.attachment.image).into(),
        context_limit,
        models,
        model_selector,
        agents: Arc::new(file_config.agents.clone()),
        attachment_store,
    })
}

/// Attachment-store sessions untouched for longer than this are pruned at
/// startup. History keeps the attachment notes; only the stored bytes age
/// out (inline media lives inside the stored messages and survives).
const ATTACHMENT_STORE_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Best-effort removal of stale per-session attachment directories. Failures
/// are logged and ignored: pruning must never block startup.
pub(crate) fn prune_attachment_store(store: &std::path::Path) {
    prune_attachment_store_before(store, std::time::SystemTime::now());
}

/// Prune session directories whose newest content (the directory itself or
/// anything inside it) is older than the TTL, measured against `now`.
pub(crate) fn prune_attachment_store_before(store: &std::path::Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(store) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            // Stray files directly under the store root: prune by their own
            // mtime.
            prune_entry_if_stale(&entry.path(), &metadata, now);
            continue;
        }
        // A session is stale when no file inside it was touched within the
        // TTL. (The directory's own mtime only tracks entry create/delete —
        // rewriting an attachment's content would not refresh it.) An empty
        // session directory carries nothing and is dropped.
        let newest = newest_modified(&entry.path(), None);
        let stale = newest
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > ATTACHMENT_STORE_TTL);
        if stale && let Err(e) = std::fs::remove_dir_all(entry.path()) {
            tracing::debug!("could not prune {}: {e}", entry.path().display());
        }
    }
}

/// The newest modification time within `dir` (recursive), including `dir`
/// itself; `None` when nothing could be stat'd.
fn newest_modified(
    dir: &std::path::Path,
    seen: Option<std::time::SystemTime>,
) -> Option<std::time::SystemTime> {
    let mut newest = seen;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return newest;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().ok();
        if modified.is_some_and(|m| newest.is_none_or(|current| m > current)) {
            newest = modified;
        }
        if metadata.is_dir()
            && let Some(nested) = newest_modified(&entry.path(), newest)
        {
            newest = Some(nested);
        }
    }
    newest
}

fn prune_entry_if_stale(
    path: &std::path::Path,
    metadata: &std::fs::Metadata,
    now: std::time::SystemTime,
) {
    let stale = metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age > ATTACHMENT_STORE_TTL);
    if stale && let Err(e) = std::fs::remove_file(path) {
        tracing::debug!("could not prune {}: {e}", path.display());
    }
}

/// Default session ID generation shared by subcommands.
pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_models_share_endpoint_and_keep_their_own_settings() {
        let config = serde_json::from_value(serde_json::json!({
            "endpoints": {"shared": {"base_url":"https://example.com/v1", "api_key":"shared-key", "models": {
                "glm": {"provider":"glm", "max_tokens":1024, "context_limit":131072},
                "deepseek": {"provider":"deepseek", "max_tokens":2048},
                "gpt": {"provider":"openai", "model":"gpt-api-id", "max_tokens":4096, "reasoning_effort":"high"}
            }}}
        })).unwrap();
        for (key, profile, budget) in [
            ("glm", "glm", 1024),
            ("deepseek", "deepseek", 2048),
            ("gpt", "openai", 4096),
        ] {
            let args = CommonArgs {
                model: Some(format!("shared/{key}")),
                api_key: Some("shared-key".into()),
                base_url: Some("https://example.com/v1".into()),
                provider: Some(profile.into()),
                ..Default::default()
            };
            let (provider, actual, model) = build_provider(&args, &config).unwrap();
            assert_eq!(actual, profile);
            let payload = provider.build(&[], &[]).unwrap();
            assert_eq!(
                payload[if key == "gpt" {
                    "max_output_tokens"
                } else {
                    "max_tokens"
                }],
                budget
            );
            assert_eq!(model, if key == "gpt" { "gpt-api-id" } else { key });
            assert_eq!(
                selected_context_limit(&args, &config).unwrap(),
                if key == "glm" { Some(131072) } else { None }
            );
        }
    }

    #[test]
    fn configured_model_selects_provider_and_api_model_id() {
        let config = serde_json::from_str(r#"{"model":"west/coding","endpoints":{"west":{"base_url":"https://example.com/v1","api_key":"test","models":{"coding":{"provider":"deepseek","model":"deepseek-reasoner"}}}}}"#).unwrap();
        let args = CommonArgs {
            api_key: Some("test".into()),
            model: Some("west/coding".into()),
            provider: Some("deepseek".into()),
            ..Default::default()
        };
        let (_, provider, model) = build_provider(&args, &config).unwrap();
        assert_eq!(provider, "deepseek");
        assert_eq!(model, "deepseek-reasoner");
        let conflict = CommonArgs {
            provider: Some("openai".into()),
            ..args.clone()
        };
        assert!(build_provider(&conflict, &config).is_err());
        let unknown = CommonArgs {
            model: Some("missing".into()),
            ..args
        };
        assert!(build_provider(&unknown, &config).is_err());
    }

    #[test]
    fn startup_constructs_service_specific_providers() {
        for (name, field) in [
            ("deepseek", "reasoning_content"),
            ("moonshot", "reasoning_content"),
            ("glm", "reasoning"),
        ] {
            let config = serde_json::from_value(serde_json::json!({
                "endpoints": {"west": {"base_url": "https://example.com/v1", "models": {"coding": {"provider": name, "model": "arbitrary-model"}}}}
            })).unwrap();
            let args = CommonArgs {
                api_key: Some("key".into()),
                model: Some("west/coding".into()),
                provider: Some(name.into()),
                base_url: Some("https://example.com/v1".into()),
                ..Default::default()
            };
            let (provider, selected, model) = build_provider(&args, &config).unwrap();
            assert_eq!(selected, name);
            assert_eq!(model, "arbitrary-model");
            let payload = provider
                .build(
                    &[gewu_core::types::Message::Assistant {
                        content: Some("answer".into()),
                        reasoning: None,
                        replay: None,
                        tool_calls: None,
                    }],
                    &[],
                )
                .unwrap();
            assert!(payload["messages"][0].get(field).is_none());
        }
    }

    #[test]
    fn arbitrary_effort_is_forwarded_in_provider_native_shape() {
        for profile in [
            ProviderProfile::Openai,
            ProviderProfile::Glm,
            ProviderProfile::Deepseek,
            ProviderProfile::Moonshot,
            ProviderProfile::OpenaiCompatible,
            ProviderProfile::Anthropic,
        ] {
            for effort in ["max", "vendor-custom", ""] {
                let provider = construct_provider(
                    profile,
                    "key",
                    "https://example.com/v1",
                    "model",
                    None,
                    Some(effort),
                    None,
                )
                .unwrap();
                let body = provider.build(&[], &[]).unwrap();
                let forwarded = match profile {
                    ProviderProfile::Openai => &body["reasoning"]["effort"],
                    ProviderProfile::Anthropic => &body["output_config"]["effort"],
                    _ => &body["reasoning_effort"],
                };
                assert_eq!(forwarded, effort);
            }
        }
    }

    #[test]
    fn reasoning_summary_opt_out_reaches_default_and_catalog_providers() {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "model":"relay/gpt", "endpoints":{"relay":{"base_url":"https://example.com/v1", "api_key":"test-key",
                "models":{"gpt":{"provider":"openai", "reasoning_summary":false}}}}
        })).unwrap();
        config.validate().unwrap();
        let args = CommonArgs::default();
        let (provider, _, _) = build_provider(&args, &config).unwrap();
        assert!(provider.build(&[], &[]).unwrap().get("reasoning").is_none());
        let catalog = build_model_catalog(&args, &config);
        assert!(
            catalog["relay/gpt"]
                .provider
                .build(&[], &[])
                .unwrap()
                .get("reasoning")
                .is_none()
        );
        let invalid: crate::config::Config = serde_json::from_value(serde_json::json!({
            "endpoints":{"relay":{"base_url":"https://example.com/v1",
                "models":{"chat":{"provider":"openai-compatible", "reasoning_summary":false}}}}
        }))
        .unwrap();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn openai_selects_responses_and_compatible_selects_chat() {
        for (name, key) in [("openai", "input"), ("openai-compatible", "messages")] {
            let config = serde_json::from_value(serde_json::json!({
                "endpoints": {"west": {"base_url": "https://example.com/v1", "models": {"coding": {
                    "provider": name, "model": "test-model", "max_tokens": 2048,
                    "reasoning_effort": if name == "openai" { Some("high") } else { None }
                }}}}
            }))
            .unwrap();
            let args = CommonArgs {
                api_key: Some("key".into()),
                model: Some("west/coding".into()),
                provider: Some(name.into()),
                base_url: Some("https://example.com/v1".into()),
                ..Default::default()
            };
            let (provider, _, _) = build_provider(&args, &config).unwrap();
            let payload = provider.build(&[], &[]).unwrap();
            assert!(payload.get(key).is_some());
            if name == "openai" {
                assert_eq!(payload["store"], false);
                assert_eq!(payload["reasoning"]["effort"], "high");
                assert_eq!(payload["max_output_tokens"], 2048);
            }
        }
    }

    #[test]
    fn catalog_covers_every_configured_model_with_own_settings() {
        let config = serde_json::from_value(serde_json::json!({
            "model": "west/coding",
            "endpoints": {"west": {"base_url": "https://example.com/v1", "api_key": "shared", "models": {
                "coding": {"provider": "deepseek", "model": "deepseek-reasoner", "max_tokens": 2048, "context_limit": 131072},
                "fast": {"provider": "openai", "max_tokens": 4096}
            }}}
        })).unwrap();
        let args = CommonArgs {
            api_key: Some("shared".into()),
            model: Some("west/coding".into()),
            base_url: Some("https://example.com/v1".into()),
            provider: Some("deepseek".into()),
            ..Default::default()
        };
        let catalog = build_model_catalog(&args, &config);
        let selectors: Vec<_> = catalog.keys().cloned().collect();
        assert_eq!(selectors, vec!["west/coding", "west/fast"]);
        assert_eq!(catalog["west/coding"].label, "deepseek-reasoner");
        assert_eq!(catalog["west/coding"].provider_name, "deepseek");
        assert_eq!(catalog["west/coding"].context_limit, 131_072);
        assert_eq!(catalog["west/fast"].label, "fast");
        assert_eq!(catalog["west/fast"].provider_name, "openai");
        assert_eq!(catalog["west/fast"].context_limit, DEFAULT_CONTEXT_LIMIT);
        assert!(!catalog["west/coding"].supports_reasoning_effort);
        assert!(catalog["west/fast"].supports_reasoning_effort);
        assert_eq!(
            catalog["west/fast"].effort_providers.len(),
            REASONING_EFFORTS.len() + 1
        );
        assert!(
            catalog["west/fast"].supports_effort(PROVIDER_DEFAULT_EFFORT),
            "the provider default is always selectable when effort is supported"
        );
        assert!(
            catalog["west/fast"]
                .provider_for(Some(PROVIDER_DEFAULT_EFFORT))
                .unwrap()
                .build(&[], &[])
                .unwrap()["reasoning"]
                .get("effort")
                .is_none(),
            "the provider default sends no reasoning effort"
        );
        assert_eq!(
            catalog["west/fast"]
                .provider_for(Some("low"))
                .unwrap()
                .build(&[], &[])
                .unwrap()["reasoning"]["effort"],
            "low"
        );
        assert_eq!(
            catalog["west/coding"].provider.build(&[], &[]).unwrap()["max_tokens"],
            2048
        );
        assert_eq!(
            catalog["west/fast"].provider.build(&[], &[]).unwrap()["max_output_tokens"],
            4096
        );
    }

    #[test]
    fn init_tracing_is_idempotent() {
        // Regression: `run` called init_tracing and then bootstrap() called
        // it again, which failed with "a global default trace dispatcher has
        // already been set" and killed every `run` invocation.
        init_tracing("info").unwrap();
        init_tracing("debug").unwrap();
    }
}
