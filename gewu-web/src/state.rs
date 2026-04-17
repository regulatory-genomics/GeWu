use anyhow::{Context, Result};
use gewu_core::traits::Tool;
use gewu_providers::OpenAIProvider;
use gewu_runtime::{RuntimeFactory, PluginHost};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::info;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub agent_type: String,
    pub created_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub db_path: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            endpoint: None,
            model: Some("gpt-5.3-codex".to_string()),
            db_path: None,
        }
    }
}

pub struct AgentHandle {
    pub agent: Arc<Mutex<gewu_core::Agent>>,
    pub session_id: String,
    pub agent_type: String,
}

#[derive(Clone)]
pub struct AppState {
    pub auth_token: Arc<RwLock<crate::auth::AuthState>>,
    pub sessions: Arc<RwLock<HashMap<String, SessionInfo>>>,
    pub agents: Arc<RwLock<HashMap<String, Arc<AgentHandle>>>>,
    pub config: Arc<RwLock<ServerConfig>>,
    pub db_pool: Arc<Mutex<Option<SqlitePool>>>,
    pub mcp_tools: Arc<Vec<Arc<dyn Tool>>>,
}

impl AppState {
    pub async fn new(
        config: ServerConfig,
        auth_token: crate::auth::AuthState,
        mcp_tools: Vec<Arc<dyn Tool>>,
    ) -> Result<Self> {
        let db_pool = if let Some(ref path) = config.db_path {
            Some(create_db_pool(path).await?)
        } else {
            let db_path = get_default_db_path()?;
            Some(create_db_pool(&db_path).await?)
        };

        Ok(Self {
            auth_token: Arc::new(RwLock::new(auth_token)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            agents: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(RwLock::new(config)),
            db_pool: Arc::new(Mutex::new(db_pool)),
            mcp_tools: Arc::new(mcp_tools),
        })
    }

    pub async fn create_session(&self, agent_type: String) -> Result<SessionInfo> {
        info!("Creating session with agent_type: {}", agent_type);
        let session_id = Uuid::new_v4().to_string();

        let config = self.config.read().await;
        let api_key = config.api_key.clone().context("API key not configured")?;
        let endpoint = config.endpoint.clone().context("Endpoint not configured")?;
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        drop(config);

        let pool_guard = self.db_pool.lock().await;
        let pool = pool_guard.as_ref().context("Database not initialized")?;

        info!("Creating provider and agent...");
        let provider = Arc::new(OpenAIProvider::new_with_endpoint(
            &api_key, &endpoint, &model,
        ));

        let mcp_tools: Vec<Arc<dyn Tool>> = self.mcp_tools.iter().cloned().collect();
        let plugin_host = PluginHost::empty();
        let runtime = RuntimeFactory::build(provider.clone(), pool.clone(), &plugin_host)?;
        let (agent, _registry) = runtime
            .build_specialist(&agent_type, session_id.clone(), mcp_tools)
            .await?;
        info!("Agent created successfully");

        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let session_info = SessionInfo {
            id: session_id.clone(),
            agent_type: agent_type.clone(),
            created_at,
        };

        let handle = Arc::new(AgentHandle {
            agent: Arc::new(Mutex::new(agent)),
            session_id: session_id.clone(),
            agent_type: agent_type.clone(),
        });

        self.sessions
            .write()
            .await
            .insert(session_id.clone(), session_info.clone());
        self.agents.write().await.insert(session_id.clone(), handle);
        info!("Session {} stored in state", session_id);

        Ok(session_info)
    }

    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.read().await.get(session_id).cloned()
    }

    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.read().await.values().cloned().collect()
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.sessions.write().await.remove(session_id);
        self.agents.write().await.remove(session_id);
        Ok(())
    }

    pub async fn get_agent(&self, session_id: &str) -> Option<Arc<AgentHandle>> {
        self.agents.read().await.get(session_id).cloned()
    }

    pub async fn get_session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<gewu_core::types::Message>> {
        let pool_guard = self.db_pool.lock().await;
        let pool = pool_guard.as_ref().context("Database not initialized")?;

        let rows =
            sqlx::query("SELECT content FROM memory_messages WHERE session_id = ? ORDER BY id ASC")
                .bind(session_id)
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

        let old_handle = self.agents.write().await.remove(session_id);
        if old_handle.is_none() {
            return Err(anyhow::anyhow!("Session not found"));
        }

        let config = self.config.read().await;
        let api_key = config.api_key.clone().context("API key not configured")?;
        let endpoint = config.endpoint.clone().context("Endpoint not configured")?;
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        drop(config);

        let pool_guard = self.db_pool.lock().await;
        let pool = pool_guard.as_ref().context("Database not initialized")?;

        let provider = Arc::new(OpenAIProvider::new_with_endpoint(
            &api_key, &endpoint, &model,
        ));

        let mcp_tools: Vec<Arc<dyn Tool>> = self.mcp_tools.iter().cloned().collect();
        let plugin_host = PluginHost::empty();
        let runtime = RuntimeFactory::build(provider.clone(), pool.clone(), &plugin_host)?;
        let (agent, _registry) = runtime
            .build_specialist(&new_agent_type, session_id.to_string(), mcp_tools)
            .await?;

        let handle = Arc::new(AgentHandle {
            agent: Arc::new(Mutex::new(agent)),
            session_id: session_id.to_string(),
            agent_type: new_agent_type.clone(),
        });

        let created_at = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id)
                .map(|s| s.created_at)
                .unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64
                })
        };

        let session_info = SessionInfo {
            id: session_id.to_string(),
            agent_type: new_agent_type,
            created_at,
        };

        self.agents
            .write()
            .await
            .insert(session_id.to_string(), handle);
        *self.sessions.write().await.get_mut(session_id).unwrap() = session_info.clone();

        info!("Agent switched successfully for session {}", session_id);
        Ok(session_info)
    }

    pub async fn update_config(&self, new_config: ServerConfig) -> Result<()> {
        *self.config.write().await = new_config;
        Ok(())
    }

    pub async fn get_config(&self) -> ServerConfig {
        self.config.read().await.clone()
    }
}

fn get_default_db_path() -> Result<String> {
    let home_dir = dirs::home_dir().context("Failed to determine home directory")?;
    let gewu_dir = home_dir.join(".gewu");
    if !gewu_dir.exists() {
        std::fs::create_dir_all(&gewu_dir).context("Failed to create .gewu directory")?;
    }
    Ok(gewu_dir.join("gewu.db").to_string_lossy().to_string())
}

async fn create_db_pool(db_path: &str) -> Result<SqlitePool> {
    let db_url = format!("sqlite://{}?mode=rwc", db_path);
    let pool = SqlitePoolOptions::new()
        .connect(&db_url)
        .await
        .context("Failed to connect to SQLite database")?;
    Ok(pool)
}
