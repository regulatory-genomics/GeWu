use super::*;
use crate::plugins::{PluginHost, package::PluginEntry, stdio::Limits};

struct PluginTurnLlm {
    mode: &'static str,
    calls: AtomicUsize,
    catalogs: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl gewu_core::traits::LlmProvider for PluginTurnLlm {
    fn build(
        &self,
        _messages: &[Message],
        tools: &[Arc<dyn gewu_core::traits::Tool>],
    ) -> std::result::Result<Value, Box<dyn Error + Send + Sync>> {
        let names: Vec<_> = tools.iter().map(|tool| tool.name().to_owned()).collect();
        let prefix = format!("plg_demo_{}_", self.mode);
        let selected = names.iter().find(|name| name.starts_with(&prefix));
        let request = serde_json::json!({"tool": selected});
        self.catalogs.lock().unwrap().push(names);
        Ok(request)
    }

    async fn send<'a>(
        &'a self,
        request: Value,
    ) -> std::result::Result<
        futures::stream::BoxStream<
            'a,
            std::result::Result<gewu_core::types::ProviderEvent, Box<dyn Error + Send + Sync>>,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        use gewu_core::types::ProviderEvent;
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first && self.mode == "fail" {
            return Err("fixture provider failure".into());
        }
        let events = if first && matches!(self.mode, "echo" | "wait") {
            let name = request["tool"].as_str().ok_or("plugin catalog missing")?;
            vec![
                ProviderEvent::ToolCallStart {
                    id: "plugin-call".into(),
                    name: name.into(),
                },
                ProviderEvent::ToolCallDelta {
                    id: "plugin-call".into(),
                    delta: r#"{"value":7}"#.into(),
                },
                ProviderEvent::Finish {
                    reason: Some("tool_calls".into()),
                },
            ]
        } else {
            vec![
                ProviderEvent::ContentDelta {
                    delta: "Next turn works".into(),
                },
                ProviderEvent::Finish {
                    reason: Some("stop".into()),
                },
            ]
        };
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }

    async fn list_models(&self) -> std::result::Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

async fn plugin_turn(mode: &'static str) {
    let package = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    crate::plugins::package::copy_dir(&fixture, package.path()).unwrap();
    std::fs::copy(
        fixture.join("adapter.py"),
        package.path().join("fixture_adapter.py"),
    )
    .unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        package.path().join("gewu_plugin.py"),
    )
    .unwrap();
    std::fs::write(
        package.path().join("adapter.py"),
        r#"#!/usr/bin/env python3
import json
from pathlib import Path
import fixture_adapter
from gewu_plugin import serve

def call(tool, args, context, control):
    Path('call-context.json').write_text(json.dumps(context))
    output = fixture_adapter.call(tool, args, context, control)
    if tool == 'wait':
        Path('cancel-state').write_text(output['error']['code'])
    return output

serve(activate=fixture_adapter.activate, call=call)
"#,
    )
    .unwrap();
    let provider = Arc::new(PluginTurnLlm {
        mode,
        calls: AtomicUsize::new(0),
        catalogs: std::sync::Mutex::new(vec![]),
    });
    let mut state = server_state_with(provider.clone());
    let bootstrap = &mut Arc::get_mut(&mut state).unwrap().bootstrap;
    bootstrap.working_directory = workspace.path().to_str().unwrap().into();
    bootstrap.workspace_roots = Arc::new(BTreeMap::from([("cwd".into(), workspace.path().into())]));
    bootstrap.plugins = Arc::new(
        PluginHost::from_config(
            &BTreeMap::from([(
                "demo".into(),
                PluginEntry {
                    package: package.path().into(),
                    enabled: true,
                    settings: Default::default(),
                    workspace_settings: Default::default(),
                    service_origins: vec![],
                    sources: Default::default(),
                },
            )]),
            None,
            Limits::default(),
        )
        .unwrap()
        .with_store(bootstrap.pool.clone()),
    );
    let agent_name = if mode == "plan" { "plan" } else { "build" };
    let session = state
        .get_or_create_session(
            &mut None,
            Some(format!("plugin-{mode}")),
            Some(agent_name.into()),
        )
        .await
        .unwrap();
    let original: Vec<_> = session
        .agent
        .lock()
        .await
        .tools()
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    let (input, mut output, connection) = spawn_connection(state.clone()).await;
    input
        .send(
            serde_json::json!({
                "id": 0, "op": "plugin", "session_id": session.session_id,
                "plugin": {"action":"open", "pluginId":"demo"},
            })
            .to_string(),
        )
        .await
        .unwrap();
    let opened = recv_json(&mut output).await;
    assert_eq!(opened["type"], "plugin", "{opened}");
    input
        .send(
            ClientRequest::ask(1, "use the application")
                .with_session(&session.session_id)
                .to_line(),
        )
        .await
        .unwrap();
    assert_eq!(recv_json(&mut output).await["type"], "ready");
    let mut echoed = false;
    let mut interrupted = false;
    loop {
        let frame = recv_json(&mut output).await;
        if frame["event"]["type"] == "tool_finish" {
            echoed |= frame["event"]["metadata"]["gewu.plugin"]["data"]["value"] == 7;
        }
        if mode == "wait" && !interrupted && frame["event"]["type"] == "tool_progress" {
            input
                .send(
                    ClientRequest::interrupt(2)
                        .with_session(&session.session_id)
                        .to_line(),
                )
                .await
                .unwrap();
            interrupted = true;
        }
        match frame["type"].as_str() {
            Some("result") => {
                assert_ne!(mode, "fail");
                assert_ne!(mode, "wait");
                break;
            }
            Some("error") if mode == "fail" => {
                assert!(
                    frame["error"]
                        .as_str()
                        .unwrap()
                        .contains("fixture provider failure")
                );
                break;
            }
            Some("interrupted") if mode == "wait" => break,
            Some("error") if mode != "wait" || !interrupted => panic!("unexpected error: {frame}"),
            _ => {}
        }
    }
    let restored: Vec<_> = session
        .agent
        .lock()
        .await
        .tools()
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    assert_eq!(restored, original, "catalog leaked after {mode}");
    if mode == "echo" {
        assert!(echoed, "adapter result was not delivered");
    }
    if mode == "wait" {
        assert!(interrupted);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !workspace.path().join("cancel-state").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("adapter never settled cancellation");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("cancel-state")).unwrap(),
            "CANCELLED"
        );
    }
    if matches!(mode, "echo" | "wait") {
        let context: Value = serde_json::from_slice(
            &std::fs::read(workspace.path().join("call-context.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(context["sessionId"], session.session_id);
        assert_eq!(context["workspaceId"], "cwd");
        assert!(context.get("runId").is_none(), "no synthetic run identity");
        uuid::Uuid::parse_str(context["callId"].as_str().unwrap()).unwrap();
    }
    if mode == "plan" {
        assert!(
            provider
                .catalogs
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .all(|name| !name.starts_with("plugin_") && !name.starts_with("plg_"))
        );
    }
    input.send(serde_json::json!({
        "id": 200, "op": "plugin", "session_id": session.session_id,
        "plugin": {"action":"set_session", "pluginId":"demo", "enabled":false, "profiles":[]},
    }).to_string()).await.unwrap();
    let disabled = recv_json(&mut output).await;
    assert_eq!(disabled["type"], "plugin", "{disabled}");
    assert_eq!(disabled["id"], 200);
    input
        .send(
            ClientRequest::ask(3, "continue")
                .with_session(&session.session_id)
                .to_line(),
        )
        .await
        .unwrap();
    loop {
        let frame = recv_json(&mut output).await;
        match frame["type"].as_str() {
            Some("result") => break,
            Some("error") => panic!("next turn failed: {frame}"),
            _ => {}
        }
    }
    assert!(
        provider
            .catalogs
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .iter()
            .all(|name| !name.starts_with("plg_demo_"))
    );
    state.bootstrap.plugins.close_all().await.unwrap();
    drop(input);
    connection.await.unwrap().unwrap();
}

#[tokio::test]
async fn plugin_turn_success_restores_catalog() {
    plugin_turn("echo").await;
}

#[tokio::test]
async fn plugin_turn_provider_error_restores_catalog() {
    plugin_turn("fail").await;
}

#[tokio::test]
async fn plugin_turn_interrupt_cancels_adapter_and_restores_catalog() {
    plugin_turn("wait").await;
}

#[tokio::test]
async fn plugin_turn_plan_does_not_project_application_tools() {
    plugin_turn("plan").await;
}
