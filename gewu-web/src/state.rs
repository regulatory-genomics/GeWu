use anyhow::{Context, Result, bail};
use gewu_canvas::{CanvasManager, canvas_tools};
use gewu_core::permission::PermissionGrant;
use gewu_core::traits::Tool;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{BackgroundTaskCoordinator, PluginHost, PluginRegistry, RuntimeFactory};
use gewu_storage::session_store::{SessionRecord, SessionStore};
use gewu_storage::{ProjectRecord, ProjectStore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::task::AbortHandle;
use tracing::info;
use uuid::Uuid;

use crate::sidecar::TachyonBrowserConfig;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub project_id: Option<String>,
    pub workdir: Option<String>,
    pub agent_type: String,
    pub created_at: i64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

struct RuntimeListingContext {
    api_key: String,
    endpoint: String,
    model: String,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    plugins: HashMap<String, Value>,
    runtime_plugin_dirs: Vec<PathBuf>,
    skill_dirs: Vec<PathBuf>,
}

impl From<SessionRecord> for SessionInfo {
    fn from(r: SessionRecord) -> Self {
        Self {
            id: r.session_id,
            project_id: r.project_id,
            workdir: r.workdir,
            agent_type: r.agent_type,
            created_at: r.created_at,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ProjectInfo {
    pub project_id: String,
    pub name: String,
    pub workdir: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub session_count: usize,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DirectoryEntry {
    pub path: String,
    pub name: String,
    pub parent: Option<String>,
    pub is_root: bool,
}

impl ProjectInfo {
    fn from_record(record: ProjectRecord, session_count: usize) -> Self {
        Self {
            project_id: record.project_id,
            name: record.name,
            workdir: record.workdir,
            created_at: record.created_at,
            updated_at: record.updated_at,
            session_count,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    #[serde(
        alias = "maxTokens",
        alias = "max_output_tokens",
        alias = "maxOutputTokens"
    )]
    pub max_output_tokens: Option<u32>,
    #[serde(default, alias = "contextLimit")]
    pub context_limit: Option<usize>,
    #[serde(default)]
    pub temperature: Option<f32>,
    pub db_path: Option<String>,
    #[serde(default)]
    pub plugins: HashMap<String, Value>,
    #[serde(default)]
    pub plugin_sidecars: HashMap<String, HashMap<String, String>>,
    #[serde(rename = "runtimePluginDirs", default)]
    pub runtime_plugin_dirs: Vec<String>,
    #[serde(rename = "skillDirs", default)]
    pub skill_dirs: Vec<String>,
    /// Optional base URL of a tachyon-browser instance (e.g. http://127.0.0.1:8400).
    /// When set, the genome.* tools (inspect_region, compare_samples, local_qc,
    /// scan_candidates, explain_region) are registered for every session. When
    /// absent, only canvas.* is available — the genome canvas works as a
    /// viewer but the agent cannot run genome analyses.
    #[serde(default, alias = "tachyonBrowserUrl")]
    pub tachyon_browser_url: Option<String>,
    #[serde(rename = "tachyonBrowser", default)]
    pub tachyon_browser: TachyonBrowserConfig,
}

pub const DEFAULT_MODEL: &str = "gpt-5.3-codex";

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            endpoint: None,
            model: Some(DEFAULT_MODEL.to_string()),
            max_output_tokens: None,
            context_limit: None,
            temperature: None,
            db_path: None,
            plugins: HashMap::new(),
            plugin_sidecars: HashMap::new(),
            runtime_plugin_dirs: Vec::new(),
            skill_dirs: Vec::new(),
            tachyon_browser_url: None,
            tachyon_browser: TachyonBrowserConfig::default(),
        }
    }
}

fn expand_config_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

pub struct AgentHandle {
    pub agent: Arc<Mutex<gewu_core::Agent>>,
    pub session_id: String,
    pub agent_type: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ActiveTurnInfo {
    pub session_id: String,
    pub started_at: i64,
    pub last_event_at: i64,
    pub event_count: usize,
    pub reasoning: String,
    pub content: String,
}

pub type PluginRegistrar = Arc<dyn Fn(&mut PluginRegistry) + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub auth_mode: Arc<RwLock<crate::auth::AuthMode>>,
    pub cookie_store: crate::auth::CookieStore,
    pub agents: Arc<RwLock<HashMap<String, Arc<AgentHandle>>>>,
    pub active_turns: Arc<RwLock<HashMap<String, ActiveTurnInfo>>>,
    pub active_aborts: Arc<RwLock<HashMap<String, Vec<AbortHandle>>>>,
    pub config: Arc<RwLock<ServerConfig>>,
    pub db_pool: SqlitePool,
    pub project_store: Arc<ProjectStore>,
    pub session_store: Arc<SessionStore>,
    pub mcp_tools: Arc<Vec<Arc<dyn Tool>>>,
    pub plugin_registrar: PluginRegistrar,
    pub canvases: CanvasManager,
    pub canvas_tools: Arc<Vec<Arc<dyn Tool>>>,
    pub permission_grants: Arc<RwLock<HashMap<String, Vec<PermissionGrant>>>>,
    rebuild_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl AppState {
    fn build_plugin_host(
        &self,
        provider: Arc<dyn gewu_core::traits::LlmProvider>,
        pool: SqlitePool,
        plugins: HashMap<String, Value>,
    ) -> Result<PluginHost> {
        let working_directory = std::env::current_dir()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(PathBuf::from(".")))?;
        self.build_plugin_host_for_workdir(provider, pool, plugins, working_directory)
    }

    fn build_plugin_host_for_workdir(
        &self,
        provider: Arc<dyn gewu_core::traits::LlmProvider>,
        pool: SqlitePool,
        plugins: HashMap<String, Value>,
        working_directory: PathBuf,
    ) -> Result<PluginHost> {
        let mut plugin_registry = PluginRegistry::new();
        (self.plugin_registrar)(&mut plugin_registry);

        PluginHost::from_config(
            &plugins,
            &plugin_registry,
            provider,
            pool,
            working_directory,
        )
    }

    async fn runtime_listing_context(&self) -> Result<RuntimeListingContext> {
        let config = self.config.read().await;
        Ok(RuntimeListingContext {
            api_key: config.api_key.clone().context("API key not configured")?,
            endpoint: config.endpoint.clone().context("Endpoint not configured")?,
            model: config
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            max_output_tokens: config.max_output_tokens,
            temperature: config.temperature,
            plugins: config.plugins.clone(),
            runtime_plugin_dirs: config
                .runtime_plugin_dirs
                .iter()
                .map(|s| expand_config_path(s))
                .collect(),
            skill_dirs: config
                .skill_dirs
                .iter()
                .map(|s| expand_config_path(s))
                .collect(),
        })
    }

    pub async fn list_available_agents(&self) -> Result<Vec<(String, String)>> {
        let ctx = self.runtime_listing_context().await?;

        let pool = &self.db_pool;

        let provider = Arc::new(
            OpenAIProvider::new_with_endpoint(&ctx.api_key, &ctx.endpoint, &ctx.model)
                .with_max_output_tokens(ctx.max_output_tokens)
                .with_temperature(ctx.temperature),
        );

        let plugin_host = self.build_plugin_host(provider.clone(), pool.clone(), ctx.plugins)?;
        let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs(
            provider,
            pool.clone(),
            &plugin_host,
            &[],
            &ctx.runtime_plugin_dirs,
            &ctx.skill_dirs,
            &[],
        )?;
        Ok(runtime
            .list_specialists()
            .into_iter()
            .map(|agent| (agent.name, agent.description))
            .collect())
    }

    pub async fn list_available_skills(&self) -> Result<Vec<SkillInfo>> {
        let ctx = self.runtime_listing_context().await?;

        let pool = &self.db_pool;

        let provider = Arc::new(
            OpenAIProvider::new_with_endpoint(&ctx.api_key, &ctx.endpoint, &ctx.model)
                .with_max_output_tokens(ctx.max_output_tokens)
                .with_temperature(ctx.temperature),
        );

        let plugin_host = self.build_plugin_host(provider.clone(), pool.clone(), ctx.plugins)?;
        let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs(
            provider,
            pool.clone(),
            &plugin_host,
            &[],
            &ctx.runtime_plugin_dirs,
            &ctx.skill_dirs,
            &[],
        )?;
        Ok(runtime
            .skill_summaries()
            .into_iter()
            .map(|skill| SkillInfo {
                name: skill.name,
                description: skill.description,
            })
            .collect())
    }

    pub async fn new(
        config: ServerConfig,
        auth_mode: crate::auth::AuthMode,
        mcp_tools: Vec<Arc<dyn Tool>>,
        plugin_registrar: PluginRegistrar,
    ) -> Result<Self> {
        let db_pool = if let Some(ref path) = config.db_path {
            create_db_pool(path).await?
        } else {
            let db_path = ensure_default_db_path()?;
            create_db_pool(&db_path).await?
        };

        let (project_store, session_store) = {
            let project_store = Arc::new(ProjectStore::new(db_pool.clone()).await?);
            let store = SessionStore::new(db_pool.clone()).await?;
            let session_store = Arc::new(store);
            let default_project = ensure_default_project(&project_store).await?;
            session_store.backfill_from_history().await?;
            session_store
                .backfill_workdir(default_project.workdir.as_str())
                .await?;
            (project_store, session_store)
        };

        let canvases = CanvasManager::new();
        let canvas_tools = Arc::new(canvas_tools(Arc::new(canvases.clone())));

        {
            let coordinator = BackgroundTaskCoordinator::new(db_pool.clone());
            match coordinator.reconcile_uninjected_groups().await {
                Ok(count) if count > 0 => {
                    info!("reconciled {count} async task completion group(s)");
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!("failed to reconcile async task groups: {err}");
                }
            }

            let stale_rows = sqlx::query("SELECT session_id, content FROM active_turn_snapshots")
                .fetch_all(&db_pool)
                .await
                .unwrap_or_default();
            for row in &stale_rows {
                let sid: String = row.get("session_id");
                let content: String = row.get("content");
                let summary = if content.len() > 200 {
                    format!("{}…", &content[..200])
                } else {
                    content
                };
                let msg =
                    format!("[session.interrupted_by_restart] Last partial response: {summary}");
                let _ = sqlx::query(
                    "INSERT INTO memory_messages (session_id, role, content, created_at) VALUES (?, 'system', ?, strftime('%s','now'))",
                )
                .bind(&sid)
                .bind(&msg)
                .execute(&db_pool)
                .await;
                info!("marked session {sid} as interrupted by restart");
            }
            if !stale_rows.is_empty() {
                let _ = sqlx::query("DELETE FROM active_turn_snapshots")
                    .execute(&db_pool)
                    .await;
            }
        }

        Ok(Self {
            auth_mode: Arc::new(RwLock::new(auth_mode)),
            cookie_store: crate::auth::CookieStore::new(),
            agents: Arc::new(RwLock::new(HashMap::new())),
            active_turns: Arc::new(RwLock::new(HashMap::new())),
            active_aborts: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(RwLock::new(config)),
            db_pool: db_pool.clone(),
            project_store,
            session_store,
            mcp_tools: Arc::new(mcp_tools),
            plugin_registrar,
            canvases,
            canvas_tools,
            permission_grants: Arc::new(RwLock::new(HashMap::new())),
            rebuild_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn add_permission_grant(&self, grant: PermissionGrant) -> Result<()> {
        let session_id = self.resolve_session_id(&grant.session_id).await?;
        let mut normalized = grant;
        normalized.session_id = session_id.clone();
        self.permission_grants
            .write()
            .await
            .entry(session_id.clone())
            .or_default()
            .push(normalized);
        self.agents.write().await.remove(&session_id);
        Ok(())
    }

    pub async fn permission_grants_for_session(&self, session_id: &str) -> Result<Vec<PermissionGrant>> {
        let session_id = self.resolve_session_id(session_id).await?;
        Ok(self
            .permission_grants
            .read()
            .await
            .get(&session_id)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn create_session(
        &self,
        agent_type: String,
        project_id: Option<String>,
    ) -> Result<SessionInfo> {
        info!("Creating session with agent_type: {}", agent_type);
        let session_id = Uuid::new_v4().to_string();
        let project = match project_id {
            Some(project_id) => Some(
                self.project_store
                    .get(&project_id)
                    .await?
                    .with_context(|| format!("Project not found: {project_id}"))?,
            ),
            None => Some(self.ensure_default_project().await?),
        };

        let config = self.config.read().await;
        let api_key = config.api_key.clone().context("API key not configured")?;
        let endpoint = config.endpoint.clone().context("Endpoint not configured")?;
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let max_output_tokens = config.max_output_tokens;
        let temperature = config.temperature;
        let plugins = config.plugins.clone();
        let runtime_plugin_dirs: Vec<PathBuf> = config
            .runtime_plugin_dirs
            .iter()
            .map(|s| expand_config_path(s))
            .collect();
        let skill_dirs: Vec<PathBuf> = config
            .skill_dirs
            .iter()
            .map(|s| expand_config_path(s))
            .collect();
        drop(config);

        let pool = &self.db_pool;
        let working_directory = project_workdir(project.as_ref())?;
        let working_directory_string = working_directory.to_string_lossy().to_string();

        info!("Creating provider and agent...");
        let provider = Arc::new(
            OpenAIProvider::new_with_endpoint(&api_key, &endpoint, &model)
                .with_max_output_tokens(max_output_tokens)
                .with_temperature(temperature),
        );

        let mut mcp_tools: Vec<Arc<dyn Tool>> = self.mcp_tools.iter().cloned().collect();
        mcp_tools.extend(self.canvas_tools.iter().cloned());
        let plugin_host = self.build_plugin_host_for_workdir(
            provider.clone(),
            pool.clone(),
            plugins,
            working_directory.clone(),
        )?;
        let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs_in_directory(
            provider.clone(),
            pool.clone(),
            &plugin_host,
            &[],
            &runtime_plugin_dirs,
            &skill_dirs,
            &[],
            &working_directory,
        )?;
        let (agent, _registry) = runtime
            .build_specialist_with_metadata_in_directory(
                &agent_type,
                session_id.clone(),
                mcp_tools,
                self.session_metadata_with_permissions(&session_id, project.as_ref(), &working_directory).await?,
                working_directory,
            )
            .await?;
        info!("Agent created successfully");

        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let session_info = SessionInfo {
            id: session_id.clone(),
            project_id: project.as_ref().map(|p| p.project_id.clone()),
            workdir: Some(working_directory_string.clone()),
            agent_type: agent_type.clone(),
            created_at,
        };

        let handle = Arc::new(AgentHandle {
            agent: Arc::new(Mutex::new(agent)),
            session_id: session_id.clone(),
            agent_type: agent_type.clone(),
        });

        self.session_store
            .insert(
                &session_id,
                project.as_ref().map(|p| p.project_id.as_str()),
                Some(working_directory_string.as_str()),
                &agent_type,
                created_at,
            )
            .await?;
        self.agents.write().await.insert(session_id.clone(), handle);
        info!("Session {} stored in state", session_id);

        Ok(session_info)
    }

    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.session_store
            .get(session_id)
            .await
            .ok()
            .flatten()
            .map(SessionInfo::from)
    }

    pub async fn resolve_session_id(&self, id: &str) -> Result<String> {
        if self.session_store.get(id).await?.is_some() {
            return Ok(id.to_string());
        }
        if let Some(full_id) = self.session_store.resolve_prefix(id).await? {
            return Ok(full_id);
        }
        Ok(id.to_string())
    }

    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        self.session_store
            .list_all()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(SessionInfo::from)
            .collect()
    }

    pub async fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let projects = self.project_store.list_all().await?;
        let mut infos = Vec::with_capacity(projects.len());
        for project in projects {
            let session_count = self
                .session_store
                .list_by_project(&project.project_id)
                .await?
                .len();
            infos.push(ProjectInfo::from_record(project, session_count));
        }
        Ok(infos)
    }

    pub async fn browse_project_directories(
        &self,
        requested: Option<String>,
    ) -> Result<Vec<DirectoryEntry>> {
        let roots = self.directory_picker_roots().await?;
        if requested
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            return Ok(roots
                .into_iter()
                .map(|path| directory_entry(path, true))
                .collect());
        }

        let raw = PathBuf::from(requested.unwrap());
        let canonical = raw.canonicalize().with_context(|| {
            format!(
                "Directory does not exist or cannot be read: {}",
                raw.display()
            )
        })?;
        if !canonical.is_dir() {
            bail!("Path is not a directory: {}", canonical.display());
        }
        if !roots.iter().any(|root| canonical.starts_with(root)) {
            bail!(
                "Directory is outside allowed project roots: {}",
                canonical.display()
            );
        }

        let mut entries = Vec::new();
        if let Some(parent) = canonical.parent() {
            if roots.iter().any(|root| parent.starts_with(root)) {
                entries.push(DirectoryEntry {
                    path: parent.to_string_lossy().to_string(),
                    name: "..".to_string(),
                    parent: parent.parent().map(|p| p.to_string_lossy().to_string()),
                    is_root: false,
                });
            }
        }

        for entry in std::fs::read_dir(&canonical)
            .with_context(|| format!("Failed to read directory: {}", canonical.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }
            let path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
            entries.push(directory_entry(path, false));
        }
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        Ok(entries)
    }

    pub async fn create_project(&self, name: String, workdir: String) -> Result<ProjectInfo> {
        let workdir = validate_project_workdir(&workdir)?;
        let created_at = unix_timestamp();
        let project = self
            .project_store
            .create_or_get_by_workdir(&name, &workdir, created_at)
            .await?;
        let session_count = self
            .session_store
            .list_by_project(&project.project_id)
            .await?
            .len();
        Ok(ProjectInfo::from_record(project, session_count))
    }

    pub async fn infer_legacy_session_projects(&self) -> Result<usize> {
        let sessions = self.session_store.list_all().await?;
        let pool = &self.db_pool;
        let mut updated = 0usize;

        for session in sessions
            .into_iter()
            .filter(|session| session.project_id.is_none())
        {
            let Some(workdir) = self.infer_session_workdir(pool, &session).await? else {
                continue;
            };
            let canonical = validate_project_workdir(&workdir)?;
            let project_name = project_name_from_workdir(&canonical);
            let project = self
                .project_store
                .create_or_get_by_workdir(&project_name, &canonical, unix_timestamp())
                .await?;
            self.session_store
                .update_project_and_workdir(
                    &session.session_id,
                    Some(project.project_id.as_str()),
                    Some(canonical.as_str()),
                )
                .await?;
            updated += 1;
        }
        Ok(updated)
    }

    pub async fn ensure_default_project(&self) -> Result<ProjectRecord> {
        ensure_default_project(&self.project_store).await
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.session_store.delete(session_id).await?;
        self.agents.write().await.remove(session_id);
        self.active_turns.write().await.remove(session_id);
        self.active_aborts.write().await.remove(session_id);
        self.canvases.drop_session(session_id).await;
        Ok(())
    }

    pub async fn register_abort_handle(&self, session_id: &str, handle: AbortHandle) {
        self.active_aborts
            .write()
            .await
            .entry(session_id.to_string())
            .or_default()
            .push(handle);
    }

    pub async fn remove_abort_handles(&self, session_id: &str) {
        self.active_aborts.write().await.remove(session_id);
    }

    pub async fn cancel_session(&self, session_id: &str) -> bool {
        let handles = self.active_aborts.write().await.remove(session_id);
        if let Some(handles) = handles {
            for h in &handles {
                h.abort();
            }
            self.finish_active_turn(session_id).await;
            !handles.is_empty()
        } else {
            false
        }
    }

    pub async fn start_active_turn(&self, session_id: &str) {
        let now = unix_timestamp();
        self.active_turns.write().await.insert(
            session_id.to_string(),
            ActiveTurnInfo {
                session_id: session_id.to_string(),
                started_at: now,
                last_event_at: now,
                event_count: 0,
                reasoning: String::new(),
                content: String::new(),
            },
        );
        self.upsert_turn_snapshot(session_id, now, now, 0, "", "")
            .await;
    }

    pub async fn touch_active_turn(&self, session_id: &str, event_count: usize) {
        let mut turns = self.active_turns.write().await;
        if let Some(turn) = turns.get_mut(session_id) {
            turn.last_event_at = unix_timestamp();
            turn.event_count = event_count;
            if event_count % 10 == 0 {
                let t = turn.clone();
                drop(turns);
                self.upsert_turn_snapshot(
                    session_id,
                    t.started_at,
                    t.last_event_at,
                    t.event_count,
                    &t.reasoning,
                    &t.content,
                )
                .await;
            }
        }
    }

    pub async fn append_active_turn_reasoning(&self, session_id: &str, delta: &str) {
        if let Some(turn) = self.active_turns.write().await.get_mut(session_id) {
            turn.last_event_at = unix_timestamp();
            turn.reasoning.push_str(delta);
        }
    }

    pub async fn append_active_turn_content(&self, session_id: &str, delta: &str) {
        if let Some(turn) = self.active_turns.write().await.get_mut(session_id) {
            turn.last_event_at = unix_timestamp();
            turn.content.push_str(delta);
        }
    }

    pub async fn finish_active_turn(&self, session_id: &str) {
        self.active_turns.write().await.remove(session_id);
        self.delete_turn_snapshot(session_id).await;
    }

    async fn upsert_turn_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
        last_event_at: i64,
        event_count: usize,
        reasoning: &str,
        content: &str,
    ) {
        let pool = &self.db_pool;
        let _ = sqlx::query(
            "INSERT INTO active_turn_snapshots (session_id, started_at, last_event_at, event_count, reasoning, content) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(session_id) DO UPDATE SET \
             last_event_at = excluded.last_event_at, \
             event_count = excluded.event_count, \
             reasoning = excluded.reasoning, \
             content = excluded.content",
        )
        .bind(session_id)
        .bind(started_at)
        .bind(last_event_at)
        .bind(event_count as i64)
        .bind(reasoning)
        .bind(content)
        .execute(pool)
        .await;
    }

    async fn delete_turn_snapshot(&self, session_id: &str) {
        let pool = &self.db_pool;
        let _ = sqlx::query("DELETE FROM active_turn_snapshots WHERE session_id = ?")
            .bind(session_id)
            .execute(pool)
            .await;
    }

    pub async fn get_active_turn(&self, session_id: &str) -> Option<ActiveTurnInfo> {
        let resolved = self
            .resolve_session_id(session_id)
            .await
            .unwrap_or_else(|_| session_id.to_string());
        self.active_turns.read().await.get(&resolved).cloned()
    }

    pub async fn get_agent(&self, session_id: &str) -> Option<Arc<AgentHandle>> {
        self.agents.read().await.get(session_id).cloned()
    }

    pub async fn ensure_agent(
        &self,
        session_id: &str,
        fallback_agent_type: &str,
    ) -> Result<(String, Arc<AgentHandle>)> {
        let session_id = self.resolve_session_id(session_id).await?;

        if let Some(handle) = self.get_agent(&session_id).await {
            return Ok((session_id, handle));
        }

        let lock = {
            let mut locks = self.rebuild_locks.lock().await;
            locks
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        if let Some(handle) = self.get_agent(&session_id).await {
            return Ok((session_id, handle));
        }

        let record = self
            .session_store
            .get(&session_id)
            .await?
            .context("Session not found")?;

        info!("rehydrating agent for persisted session {}", session_id);

        let agent_type = if record.agent_type.is_empty() {
            fallback_agent_type.to_string()
        } else {
            record.agent_type.clone()
        };

        let handle = self.build_agent(&session_id, &agent_type).await?;
        self.agents
            .write()
            .await
            .insert(session_id.clone(), handle.clone());

        Ok((session_id, handle))
    }

    async fn build_agent(&self, session_id: &str, agent_type: &str) -> Result<Arc<AgentHandle>> {
        let record = self.session_store.get(session_id).await?;
        let project = self.project_for_session(record.as_ref()).await?;
        let working_directory = session_workdir(record.as_ref(), project.as_ref())?;

        let config = self.config.read().await;
        let api_key = config.api_key.clone().context("API key not configured")?;
        let endpoint = config.endpoint.clone().context("Endpoint not configured")?;
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let max_output_tokens = config.max_output_tokens;
        let temperature = config.temperature;
        let plugins = config.plugins.clone();
        let runtime_plugin_dirs: Vec<PathBuf> = config
            .runtime_plugin_dirs
            .iter()
            .map(|s| expand_config_path(s))
            .collect();
        let skill_dirs: Vec<PathBuf> = config
            .skill_dirs
            .iter()
            .map(|s| expand_config_path(s))
            .collect();
        drop(config);

        let pool = &self.db_pool;

        let provider = Arc::new(
            OpenAIProvider::new_with_endpoint(&api_key, &endpoint, &model)
                .with_max_output_tokens(max_output_tokens)
                .with_temperature(temperature),
        );

        let mut mcp_tools: Vec<Arc<dyn Tool>> = self.mcp_tools.iter().cloned().collect();
        mcp_tools.extend(self.canvas_tools.iter().cloned());
        let plugin_host = self.build_plugin_host_for_workdir(
            provider.clone(),
            pool.clone(),
            plugins,
            working_directory.clone(),
        )?;
        let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs_in_directory(
            provider.clone(),
            pool.clone(),
            &plugin_host,
            &[],
            &runtime_plugin_dirs,
            &skill_dirs,
            &[],
            &working_directory,
        )?;
        let (agent, _registry) = runtime
            .build_specialist_with_metadata_in_directory(
                agent_type,
                session_id.to_string(),
                mcp_tools,
                self.session_metadata_with_permissions(session_id, project.as_ref(), &working_directory).await?,
                working_directory,
            )
            .await?;

        Ok(Arc::new(AgentHandle {
            agent: Arc::new(Mutex::new(agent)),
            session_id: session_id.to_string(),
            agent_type: agent_type.to_string(),
        }))
    }

    async fn infer_session_workdir(
        &self,
        pool: &SqlitePool,
        record: &SessionRecord,
    ) -> Result<Option<String>> {
        if let Some(workdir) = record
            .workdir
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(Some(workdir.to_string()));
        }

        let rows = sqlx::query(
            "SELECT input_json FROM execution_runs WHERE session_id = ? AND input_json IS NOT NULL ORDER BY started_at ASC",
        )
        .bind(&record.session_id)
        .fetch_all(pool)
        .await
        .context("Failed to read execution trace metadata for session")?;

        for row in rows {
            let raw: Option<String> = row.try_get("input_json").ok();
            let Some(raw) = raw else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if let Some(workdir) = trace_workdir(&value) {
                return Ok(Some(workdir.to_string()));
            }
        }
        Ok(None)
    }

    async fn project_for_session(
        &self,
        record: Option<&SessionRecord>,
    ) -> Result<Option<ProjectRecord>> {
        let Some(project_id) = record.and_then(|record| record.project_id.as_deref()) else {
            return Ok(None);
        };
        self.project_store
            .get(project_id)
            .await?
            .with_context(|| format!("Project not found for session: {project_id}"))
            .map(Some)
    }

    pub async fn get_session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<gewu_core::types::Message>> {
        let session_id = self.resolve_session_id(session_id).await?;
        let pool = &self.db_pool;

        let rows = sqlx::query(
            "SELECT content FROM memory_messages WHERE session_id = ? ORDER BY created_at, id",
        )
        .bind(&session_id)
        .fetch_all(pool)
        .await
        .context("Failed to load messages from DB")?;

        let mut messages = Vec::new();
        for row in rows {
            let content: String = row.try_get("content")?;
            let message: gewu_core::types::Message =
                serde_json::from_str(&content).context("Failed to deserialize message content")?;
            messages.push(message);
        }

        info!(
            "Loaded {} messages for session {}",
            messages.len(),
            session_id
        );
        Ok(messages)
    }

    pub async fn switch_agent(
        &self,
        session_id: &str,
        new_agent_type: String,
    ) -> Result<SessionInfo> {
        info!(
            "Switching agent for session {} to type: {}",
            session_id, new_agent_type
        );

        self.agents.write().await.remove(session_id);

        let record = self
            .session_store
            .get(session_id)
            .await?
            .context("Session not found")?;

        let handle = self.build_agent(session_id, &new_agent_type).await?;

        self.session_store
            .update_agent_type(session_id, &new_agent_type)
            .await?;

        self.agents
            .write()
            .await
            .insert(session_id.to_string(), handle);

        info!("Agent switched successfully for session {}", session_id);
        Ok(SessionInfo {
            id: session_id.to_string(),
            project_id: record.project_id,
            workdir: record.workdir,
            agent_type: new_agent_type,
            created_at: record.created_at,
        })
    }

    pub async fn update_config(&self, new_config: ServerConfig) -> Result<()> {
        *self.config.write().await = new_config;
        Ok(())
    }

    pub async fn get_config(&self) -> ServerConfig {
        self.config.read().await.clone()
    }

    async fn session_metadata_with_permissions(
        &self,
        session_id: &str,
        project: Option<&ProjectRecord>,
        working_directory: &Path,
    ) -> Result<Value> {
        let mut metadata = session_metadata(project, working_directory);
        let grants = self.permission_grants_for_session(session_id).await?;
        if !grants.is_empty() {
            let grants_value = serde_json::to_value(grants)?;
            if let Value::Object(map) = &mut metadata {
                map.insert("permission_grants".to_string(), grants_value);
            }
        }
        Ok(metadata)
    }
}

impl AppState {
    async fn directory_picker_roots(&self) -> Result<Vec<PathBuf>> {
        let mut roots = Vec::new();
        if let Some(home) = dirs::home_dir().and_then(|path| path.canonicalize().ok()) {
            roots.push(home);
        }
        if let Ok(cwd) = std::env::current_dir().and_then(|path| path.canonicalize()) {
            roots.push(cwd);
        }
        for project in self.project_store.list_all().await? {
            if let Ok(path) = PathBuf::from(project.workdir).canonicalize() {
                roots.push(path);
            }
        }
        roots.sort();
        roots.dedup();
        Ok(roots)
    }
}

async fn ensure_default_project(project_store: &ProjectStore) -> Result<ProjectRecord> {
    let cwd =
        std::env::current_dir().or_else(|_| Ok::<PathBuf, std::io::Error>(PathBuf::from(".")))?;
    let canonical = cwd.canonicalize().unwrap_or(cwd);
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Current Directory")
        .to_string();
    let workdir = canonical.to_string_lossy().to_string();
    project_store
        .create_or_get_by_workdir(&name, &workdir, unix_timestamp())
        .await
}

fn validate_project_workdir(workdir: &str) -> Result<String> {
    let path = PathBuf::from(workdir);
    if !path.is_absolute() {
        bail!("Project workdir must be an absolute path");
    }
    if !path.exists() {
        bail!("Project workdir does not exist: {workdir}");
    }
    if !path.is_dir() {
        bail!("Project workdir is not a directory: {workdir}");
    }
    Ok(path
        .canonicalize()
        .unwrap_or(path)
        .to_string_lossy()
        .to_string())
}

fn directory_entry(path: PathBuf, is_root: bool) -> DirectoryEntry {
    DirectoryEntry {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| path.to_str().unwrap_or("/"))
            .to_string(),
        parent: path
            .parent()
            .map(|parent| parent.to_string_lossy().to_string()),
        path: path.to_string_lossy().to_string(),
        is_root,
    }
}

fn project_name_from_workdir(workdir: &str) -> String {
    PathBuf::from(workdir)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Project")
        .to_string()
}

fn trace_workdir(value: &Value) -> Option<&str> {
    value
        .pointer("/metadata/workdir")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/metadata/workspace_root")
                .and_then(Value::as_str)
        })
        .or_else(|| value.pointer("/metadata/directory").and_then(Value::as_str))
}

fn project_workdir(project: Option<&ProjectRecord>) -> Result<PathBuf> {
    match project {
        Some(project) => Ok(PathBuf::from(&project.workdir)),
        None => std::env::current_dir()
            .or_else(|_| Ok::<PathBuf, std::io::Error>(PathBuf::from(".")))
            .map_err(Into::into),
    }
}

fn session_workdir(
    record: Option<&SessionRecord>,
    project: Option<&ProjectRecord>,
) -> Result<PathBuf> {
    if let Some(workdir) = record.and_then(|record| record.workdir.as_deref()) {
        if !workdir.trim().is_empty() {
            return Ok(PathBuf::from(workdir));
        }
    }
    project_workdir(project)
}

fn session_metadata(project: Option<&ProjectRecord>, working_directory: &Path) -> Value {
    let workdir = working_directory.to_string_lossy().to_string();
    match project {
        Some(project) => serde_json::json!({
            "project_id": project.project_id,
            "workdir": workdir,
            "workspace_root": project.workdir,
        }),
        None => serde_json::json!({
            "workdir": workdir,
            "workspace_root": workdir,
        }),
    }
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn ensure_default_db_path() -> Result<String> {
    let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
    let gewu_dir = home_dir.join(".gewu");
    if !gewu_dir.exists() {
        std::fs::create_dir_all(&gewu_dir).context("Failed to create .gewu directory")?;
    }
    Ok(gewu_dir.join("gewu.db").to_string_lossy().to_string())
}

async fn create_db_pool(db_path: &str) -> Result<SqlitePool> {
    // Phase 1 (C2 + C5): use the shared, PRAGMA-tuned helper and run
    // versioned SQL migrations from gewu-storage/migrations/.
    let pool = gewu_storage::create_pool(Path::new(db_path)).await?;
    gewu_storage::run_migrations(&pool).await?;
    Ok(pool)
}
