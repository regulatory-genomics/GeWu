use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent, SpecialistExecutionRequest};
use gewu_runtime::GeWuRuntime;
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

struct FinishLlm;

#[async_trait]
impl LlmProvider for FinishLlm {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        Ok(Box::pin(stream::iter(vec![Ok(ProviderEvent::Finish {
            reason: Some("profile checked".to_string()),
        })])))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

fn spawn_memory_profile_hook() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        let body = serde_json::json!({
            "rendered": "## User Profile\n- My name is Gilbert Han.\n- I prefer Rust.\n- My micromamba env: py311.",
            "tools_hint": "Use memory_search when prior context is needed."
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    format!("http://{}", addr)
}

#[tokio::test]
async fn runtime_plugin_memory_hook_injects_user_profile() {
    let base_url = spawn_memory_profile_hook();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("gewu-memory.yaml"),
        format!(
            r#"
id: gewu-memory
kind: http
enabled: true
base_url: {base_url}
timeout_ms: 1500
on_error: fail
hooks:
  - name: prepare_memory_context
    event: specialist.system_prompt.transform
    endpoint: /v1/context/prepare
    method: POST
    timeout_ms: 1500
    on_error: fail
"#
        ),
    )
    .unwrap();

    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(FinishLlm);
    let runtime = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .with_runtime_plugin_dir(dir.path())
        .build()
        .unwrap();

    let envelope = runtime
        .execute_request(
            &SpecialistExecutionRequest {
                task_id: "memory-profile-check".to_string(),
                specialist_id: "general".to_string(),
                session_id: "memory-profile-session".to_string(),
                prompt: "What do you remember about me?".to_string(),
                metadata_json: serde_json::json!({
                    "project_id": "GeWu",
                    "user_id": "default"
                }),
            },
            vec![],
        )
        .await
        .unwrap();

    let system_prompt = &envelope.llm_io[0].system_prompt;
    assert!(system_prompt.contains("Gilbert Han"), "{system_prompt}");
    assert!(system_prompt.contains("Rust"), "{system_prompt}");
    assert!(system_prompt.contains("py311"), "{system_prompt}");
}
