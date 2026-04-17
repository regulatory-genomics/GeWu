use anyhow::Result;
use gewu_core::traits::LlmProvider;
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::plugin_host::PluginHost;
use crate::runtime::GeWuRuntime;

/// Shared factory for building a `GeWuRuntime` with builtins and plugins.
///
/// Both CLI and Web should use this to ensure consistent runtime construction.
pub struct RuntimeFactory;

impl RuntimeFactory {
    /// Build a runtime with builtin capabilities/specialists and all plugins
    /// from the given PluginHost.
    pub fn build(
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        plugin_host: &PluginHost,
    ) -> Result<GeWuRuntime> {
        let mut builder = GeWuRuntime::builder()
            .provider(provider)
            .pool(pool)
            .register_builtin_capabilities()
            .register_builtin_specialists();

        for plugin in plugin_host.plugins() {
            // Extract factories and descriptors without consuming the plugin
            builder = builder.register_plugin_from_host(plugin.as_ref());
        }

        builder.build()
    }
}
