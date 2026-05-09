use anyhow::Result;
use gewu_core::traits::LlmProvider;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::plugin_host::PluginHost;
use crate::runtime::GeWuRuntime;

pub struct RuntimeFactory;

impl RuntimeFactory {
    /// Build a runtime with builtin capabilities/specialists and all plugins
    /// from the given PluginHost.
    pub fn build(
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        plugin_host: &PluginHost,
    ) -> Result<GeWuRuntime> {
        Self::build_with_manifest_dirs(provider, pool, plugin_host, &[])
    }

    /// Build a runtime with manifest directories for YAML-defined specialists.
    pub fn build_with_manifest_dirs(
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        plugin_host: &PluginHost,
        manifest_dirs: &[PathBuf],
    ) -> Result<GeWuRuntime> {
        Self::build_with_manifest_and_runtime_plugin_dirs(
            provider,
            pool,
            plugin_host,
            manifest_dirs,
            &[],
            &[],
            &[],
        )
    }

    /// Build a runtime with specialist manifest directories and YAML runtime plugin directories.
    pub fn build_with_manifest_and_runtime_plugin_dirs(
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        plugin_host: &PluginHost,
        manifest_dirs: &[PathBuf],
        runtime_plugin_dirs: &[PathBuf],
        skill_dirs: &[PathBuf],
        workflow_dirs: &[PathBuf],
    ) -> Result<GeWuRuntime> {
        let working_directory = std::env::current_dir()?;
        Self::build_with_manifest_and_runtime_plugin_dirs_in_directory(
            provider,
            pool,
            plugin_host,
            manifest_dirs,
            runtime_plugin_dirs,
            skill_dirs,
            workflow_dirs,
            &working_directory,
        )
    }

    /// Build a runtime using an explicit project/session working directory for project-local config.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_manifest_and_runtime_plugin_dirs_in_directory(
        provider: Arc<dyn LlmProvider>,
        pool: SqlitePool,
        plugin_host: &PluginHost,
        manifest_dirs: &[PathBuf],
        runtime_plugin_dirs: &[PathBuf],
        skill_dirs: &[PathBuf],
        workflow_dirs: &[PathBuf],
        working_directory: &Path,
    ) -> Result<GeWuRuntime> {
        let mut builder = GeWuRuntime::builder()
            .provider(provider)
            .pool(pool)
            .register_builtin_capabilities()
            .register_builtin_specialists();

        for plugin in plugin_host.plugins() {
            builder = builder.register_plugin_from_host(plugin.as_ref());
            if let Some(dir) = plugin.specialist_manifest_dir() {
                builder = builder.with_manifest_dir(dir);
            }
        }

        for dir in manifest_dirs {
            builder = builder.with_manifest_dir(dir);
        }

        for dir in runtime_plugin_dirs {
            builder = builder.with_runtime_plugin_dir(dir);
        }

        for dir in skill_dirs {
            builder = builder.with_skill_dir(dir);
        }

        for dir in workflow_dirs {
            builder = builder.with_workflow_dir(dir);
        }

        builder = builder.with_resource_dir(working_directory.join(".gewu/resources"));

        let home_specialists = dirs::home_dir()
            .map(|h| h.join(".gewu").join("specialists"))
            .unwrap_or_else(|| PathBuf::from(".gewu/specialists"));
        if home_specialists.is_dir() {
            builder = builder.with_manifest_dir(&home_specialists);
        }

        let project_specialists = PathBuf::from(".gewu/specialists");
        if project_specialists.is_dir() && project_specialists != home_specialists {
            builder = builder.with_manifest_dir(project_specialists);
        }

        builder.build()
    }
}
