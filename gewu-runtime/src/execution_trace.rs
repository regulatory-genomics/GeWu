use anyhow::Result;
use async_trait::async_trait;
use gewu_core::types::{ExecutionEvent, ExecutionRun, ExecutionRunKind};
use gewu_storage::{ExecutionTraceStore, StartExecutionRun};
use serde_json::Value;
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct TraceContext {
    pub root_run_id: Option<String>,
    pub current_run_id: Option<String>,
    pub session_id: String,
}

impl TraceContext {
    pub fn root(session_id: impl Into<String>) -> Self {
        Self {
            root_run_id: None,
            current_run_id: None,
            session_id: session_id.into(),
        }
    }

    pub fn child(&self, run_id: impl Into<String>) -> Self {
        let run_id = run_id.into();
        Self {
            root_run_id: self.root_run_id.clone().or_else(|| Some(run_id.clone())),
            current_run_id: Some(run_id),
            session_id: self.session_id.clone(),
        }
    }

    pub fn to_metadata_map(&self) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        if let Some(root_run_id) = &self.root_run_id {
            map.insert(
                "trace_root_run_id".to_string(),
                Value::String(root_run_id.clone()),
            );
        }
        if let Some(current_run_id) = &self.current_run_id {
            map.insert(
                "trace_current_run_id".to_string(),
                Value::String(current_run_id.clone()),
            );
        }
        map
    }
}

#[derive(Clone, Debug)]
pub struct StartTraceRun {
    pub parent_id: Option<String>,
    pub session_id: Option<String>,
    pub kind: ExecutionRunKind,
    pub name: String,
    pub input_json: Option<Value>,
}

#[async_trait]
pub trait TraceSink: Send + Sync {
    async fn start_run(&self, input: StartTraceRun) -> Result<ExecutionRun>;
    async fn finish_run(&self, run_id: &str, output_json: Option<&Value>) -> Result<ExecutionRun>;
    async fn fail_run(&self, run_id: &str, error: &str) -> Result<ExecutionRun>;
    async fn event(
        &self,
        run_id: &str,
        event_type: &str,
        payload_json: &Value,
    ) -> Result<ExecutionEvent>;
}

#[derive(Clone, Debug, Default)]
pub struct NoopTraceSink;

#[async_trait]
impl TraceSink for NoopTraceSink {
    async fn start_run(&self, input: StartTraceRun) -> Result<ExecutionRun> {
        Ok(ExecutionRun {
            id: format!("noop_run_{}", uuid::Uuid::new_v4()),
            parent_id: input.parent_id,
            session_id: input.session_id,
            kind: input.kind,
            name: input.name,
            status: gewu_core::types::ExecutionRunStatus::Running,
            input_json: input.input_json,
            output_json: None,
            error: None,
            started_at: now_unix(),
            finished_at: None,
        })
    }

    async fn finish_run(&self, run_id: &str, output_json: Option<&Value>) -> Result<ExecutionRun> {
        Ok(ExecutionRun {
            id: run_id.to_string(),
            parent_id: None,
            session_id: None,
            kind: ExecutionRunKind::Tool,
            name: "noop".to_string(),
            status: gewu_core::types::ExecutionRunStatus::Succeeded,
            input_json: None,
            output_json: output_json.cloned(),
            error: None,
            started_at: now_unix(),
            finished_at: Some(now_unix()),
        })
    }

    async fn fail_run(&self, run_id: &str, error: &str) -> Result<ExecutionRun> {
        Ok(ExecutionRun {
            id: run_id.to_string(),
            parent_id: None,
            session_id: None,
            kind: ExecutionRunKind::Tool,
            name: "noop".to_string(),
            status: gewu_core::types::ExecutionRunStatus::Failed,
            input_json: None,
            output_json: None,
            error: Some(error.to_string()),
            started_at: now_unix(),
            finished_at: Some(now_unix()),
        })
    }

    async fn event(
        &self,
        run_id: &str,
        event_type: &str,
        payload_json: &Value,
    ) -> Result<ExecutionEvent> {
        Ok(ExecutionEvent {
            id: format!("noop_evt_{}", uuid::Uuid::new_v4()),
            run_id: run_id.to_string(),
            event_type: event_type.to_string(),
            payload_json: payload_json.clone(),
            created_at: now_unix(),
        })
    }
}

#[derive(Clone)]
pub struct SqliteTraceSink {
    store: ExecutionTraceStore,
}

impl SqliteTraceSink {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        Ok(Self {
            store: ExecutionTraceStore::new(pool).await?,
        })
    }

    pub fn store(&self) -> &ExecutionTraceStore {
        &self.store
    }
}

#[async_trait]
impl TraceSink for SqliteTraceSink {
    async fn start_run(&self, input: StartTraceRun) -> Result<ExecutionRun> {
        self.store
            .start_run(StartExecutionRun {
                parent_id: input.parent_id,
                session_id: input.session_id,
                kind: input.kind,
                name: input.name,
                input_json: input.input_json,
            })
            .await
    }

    async fn finish_run(&self, run_id: &str, output_json: Option<&Value>) -> Result<ExecutionRun> {
        self.store.finish_run(run_id, output_json).await
    }

    async fn fail_run(&self, run_id: &str, error: &str) -> Result<ExecutionRun> {
        self.store.fail_run(run_id, error).await
    }

    async fn event(
        &self,
        run_id: &str,
        event_type: &str,
        payload_json: &Value,
    ) -> Result<ExecutionEvent> {
        self.store
            .record_event(run_id, event_type, payload_json)
            .await
    }
}

pub fn trace_sink_from_pool(pool: SqlitePool) -> Arc<dyn TraceSink> {
    // Defer fallible initialization to call sites that need the concrete store.
    Arc::new(LazySqliteTraceSink { pool })
}

struct LazySqliteTraceSink {
    pool: SqlitePool,
}

impl LazySqliteTraceSink {
    async fn store(&self) -> Result<ExecutionTraceStore> {
        ExecutionTraceStore::new(self.pool.clone()).await
    }
}

#[async_trait]
impl TraceSink for LazySqliteTraceSink {
    async fn start_run(&self, input: StartTraceRun) -> Result<ExecutionRun> {
        self.store()
            .await?
            .start_run(StartExecutionRun {
                parent_id: input.parent_id,
                session_id: input.session_id,
                kind: input.kind,
                name: input.name,
                input_json: input.input_json,
            })
            .await
    }

    async fn finish_run(&self, run_id: &str, output_json: Option<&Value>) -> Result<ExecutionRun> {
        self.store().await?.finish_run(run_id, output_json).await
    }

    async fn fail_run(&self, run_id: &str, error: &str) -> Result<ExecutionRun> {
        self.store().await?.fail_run(run_id, error).await
    }

    async fn event(
        &self,
        run_id: &str,
        event_type: &str,
        payload_json: &Value,
    ) -> Result<ExecutionEvent> {
        self.store()
            .await?
            .record_event(run_id, event_type, payload_json)
            .await
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
