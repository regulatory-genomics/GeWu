use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent, SpecialistExecutionRequest};
use gewu_runtime::GeWuRuntime;
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

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

fn spawn_capture_hook(expected_requests: usize) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captures_for_thread = captures.clone();
    std::thread::spawn(move || {
        for _ in 0..expected_requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let size = stream.read(&mut request).unwrap();
            let request_text = String::from_utf8_lossy(&request[..size]);
            if let Some((_, body)) = request_text.split_once("\r\n\r\n")
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim())
            {
                captures_for_thread.lock().unwrap().push(value);
            }
            let body = serde_json::json!({
                "event_id": "evt_test",
                "queued_for_indexing": true,
                "duplicate": false
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{}", addr), captures)
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
    gewu_storage::run_migrations(&pool).await.unwrap();
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
                workdir: None,
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

#[tokio::test]
async fn runtime_plugin_emits_raw_user_and_assistant_events() {
    let (base_url, captures) = spawn_capture_hook(2);
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
  - name: remember_agent_event
    event: agent.event
    endpoint: /v1/events/remember
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
    gewu_storage::run_migrations(&pool).await.unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(FinishLlm);
    let runtime = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_builtin_capabilities()
        .register_builtin_specialists()
        .with_runtime_plugin_dir(dir.path())
        .build()
        .unwrap();

    runtime
        .execute_request(
            &SpecialistExecutionRequest {
                task_id: "memory-raw-capture".to_string(),
                specialist_id: "general".to_string(),
                session_id: "memory-raw-session".to_string(),
                prompt: "Persist this raw GeWu prompt.".to_string(),
                workdir: None,
                metadata_json: serde_json::json!({
                    "project_id": "GeWu",
                    "user_id": "default"
                }),
            },
            vec![],
        )
        .await
        .unwrap();

    let captured = captures.lock().unwrap().clone();
    assert_eq!(captured.len(), 2, "{captured:#?}");
    assert!(captured.iter().any(|event| {
        event["event_kind"] == "user_message"
            && event["role"] == "user"
            && event["layer"] == "raw"
            && event["content"] == "Persist this raw GeWu prompt."
            && event["workspace_root"].is_string()
    }));
    assert!(captured.iter().any(|event| {
        event["event_kind"] == "assistant_response"
            && event["role"] == "assistant"
            && event["layer"] == "raw"
            && event["content_json"].is_object()
    }));
}
