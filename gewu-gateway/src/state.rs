use anyhow::Result;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{PluginHost, PluginRegistry, RuntimeFactory, WorkflowRegistry};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::GatewayConfig;
use crate::scheduler::store::JobStore;

pub struct AppState {
    pub config: GatewayConfig,
    pub runtime: Arc<gewu_runtime::GeWuRuntime>,
    pub pool: SqlitePool,
    pub jobs: JobStore,
}

impl AppState {
    pub async fn new(config: GatewayConfig) -> Result<Self> {
        let pool = create_db_pool(&config.db_path).await?;
        let provider = Arc::new(OpenAIProvider::new_with_endpoint(
            &config.api_key,
            &config.endpoint,
            &config.model,
        ));

        let plugin_registry = PluginRegistry::new();
        let working_directory = std::env::current_dir()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(PathBuf::from(".")))?;
        let plugin_host = PluginHost::from_config(
            &config.plugins,
            &plugin_registry,
            provider.clone(),
            pool.clone(),
            working_directory,
        )?;

        let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs(
            provider,
            pool.clone(),
            &plugin_host,
            &config.specialist_dirs,
            &config.runtime_plugin_dirs,
            &config.skill_dirs,
            &config.workflow_dirs,
        )?;

        let jobs = JobStore::new(pool.clone()).await?;
        Ok(Self {
            config,
            runtime: Arc::new(runtime),
            pool,
            jobs,
        })
    }

    pub fn workflow_registry(&self) -> Result<WorkflowRegistry> {
        let mut dirs = self.config.workflow_dirs.clone();
        let home_workflows = dirs::home_dir()
            .map(|h| h.join(".gewu").join("workflows"))
            .unwrap_or_else(|| PathBuf::from(".gewu/workflows"));
        if home_workflows.is_dir() && !dirs.iter().any(|dir| dir == &home_workflows) {
            dirs.push(home_workflows);
        }
        let project_workflows = PathBuf::from(".gewu/workflows");
        if project_workflows.is_dir() && !dirs.iter().any(|dir| dir == &project_workflows) {
            dirs.push(project_workflows);
        }
        WorkflowRegistry::from_dirs(&dirs)
    }
}

async fn create_db_pool(db_path: &Path) -> Result<SqlitePool> {
    // Phase 1 (C2 + C5): use shared PRAGMA-tuned helper and run migrations.
    let pool = gewu_storage::create_pool(db_path).await?;
    gewu_storage::run_migrations(&pool).await?;
    Ok(pool)
}
