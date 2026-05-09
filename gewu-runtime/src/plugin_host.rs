use anyhow::{Result, anyhow};
use gewu_core::traits::{LlmProvider, Plugin, PluginPlatformContext};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A function that constructs a plugin from a platform context.
pub type PluginConstructor =
    Box<dyn Fn(&PluginPlatformContext) -> Result<Box<dyn Plugin>> + Send + Sync>;

/// Registry of known plugin constructors, keyed by plugin name.
pub struct PluginRegistry {
    constructors: HashMap<String, PluginConstructor>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            constructors: HashMap::new(),
        }
    }

    /// Register a plugin constructor under a given name.
    pub fn register<F>(&mut self, name: impl Into<String>, constructor: F)
    where
        F: Fn(&PluginPlatformContext) -> Result<Box<dyn Plugin>> + Send + Sync + 'static,
    {
        self.constructors.insert(name.into(), Box::new(constructor));
    }

    pub fn has(&self, name: &str) -> bool {
        self.constructors.contains_key(name)
    }

    pub fn constructor(&self, name: &str) -> Option<&PluginConstructor> {
        self.constructors.get(name)
    }

    pub fn registered_names(&self) -> Vec<&str> {
        self.constructors.keys().map(|k| k.as_str()).collect()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Hosts initialized plugin instances.
///
/// Reads plugin configuration, matches against a `PluginRegistry` of known
/// constructors, and produces live plugin instances ready for runtime injection.
pub struct PluginHost {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginHost {
    /// Build a PluginHost from config and a registry of constructors.
    ///
    /// For each entry in `plugin_configs`, if the registry has a matching
    /// constructor and the config has `"enabled": true` (or no `enabled` key),
    /// the plugin is constructed and added.
    pub fn from_config(
        plugin_configs: &HashMap<String, Value>,
        registry: &PluginRegistry,
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        working_directory: PathBuf,
    ) -> Result<Self> {
        let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();

        for (name, config) in plugin_configs {
            // Skip disabled plugins
            if let Some(enabled) = config.get("enabled")
                && enabled == &Value::Bool(false)
            {
                tracing::info!("Plugin '{}' is disabled, skipping", name);
                continue;
            }

            if let Some(constructor) = registry.constructor(name) {
                let ctx = PluginPlatformContext {
                    provider: provider.clone(),
                    pool: pool.clone(),
                    working_directory: working_directory.clone(),
                    plugin_config: config.clone(),
                };
                let plugin = constructor(&ctx)
                    .map_err(|e| anyhow!("Failed to initialize plugin '{name}': {e}"))?;
                tracing::info!("Loaded plugin: {} ({})", name, plugin.name());
                plugins.push(plugin);
            } else {
                tracing::warn!(
                    "Plugin '{}' is configured but no constructor is registered, skipping",
                    name
                );
            }
        }

        Ok(Self { plugins })
    }

    /// Create an empty PluginHost (no plugins loaded).
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    pub fn plugins(&self) -> &[Box<dyn Plugin>] {
        &self.plugins
    }

    pub fn plugin_names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }
}
