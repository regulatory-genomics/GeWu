#![cfg(unix)]
use gewu_core::types::ToolContext;
use gewu_protocol::plugin::{HostAction, ToolOutput};
use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

#[tokio::test]
async fn agent_and_ui_validate_against_the_same_catalog_and_reject_stale_generation() {
    let package = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    std::fs::create_dir(package.path().join("web")).unwrap();
    for file in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(file), package.path().join(file)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        package.path().join("gewu_plugin.py"),
    )
    .unwrap();
    let host = Arc::new(
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
        .unwrap(),
    );
    let now = host.open("demo", "workspace", root, None).await.unwrap();
    let selected = host
        .tools("workspace", root.to_owned(), "conversation", &[])
        .await;
    let names: Vec<&str> = selected.iter().map(|tool| tool.name()).collect();
    for expected in [
        "plugin_list",
        "plugin_open",
        "plugin_close",
        "plugin_describe",
        "plugin_call",
        "plugin_set_enabled",
        "plugin_set_session",
        "plugin_calls",
        "plugin_call_status",
        "plugin_cancel",
        "plugin_focus",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    let echo = selected
        .iter()
        .find(|tool| tool.name().starts_with("plg_demo_echo_"))
        .unwrap();
    let view_echo = selected
        .iter()
        .find(|tool| tool.name().starts_with("plg_demo_view_echo_"))
        .unwrap();
    assert!(
        !selected
            .iter()
            .any(|tool| tool.name().contains("workflow_echo"))
    );
    let ctx = ToolContext::new("conversation", "message").with_working_directory(root);
    let out = echo.execute(json!({"value":7}), &ctx).await.unwrap();
    assert_eq!(out.metadata["gewu.plugin"]["data"]["value"], 7);
    assert!(echo.execute(json!({"value":"bad"}), &ctx).await.is_err());

    let attached = host
        .action(
            "workspace",
            root,
            "conversation",
            HostAction::Attach {
                instance_id: now.activation.instance_id.clone(),
                view_type: "demo-view".into(),
                resource: None,
            },
        )
        .await
        .unwrap();
    let view_id = attached["viewId"].as_str().unwrap().to_owned();
    host.action(
        "workspace",
        root,
        "conversation",
        HostAction::ViewReady {
            view_id: view_id.clone(),
        },
    )
    .await
    .unwrap();
    host.action(
        "workspace",
        root,
        "conversation",
        HostAction::FocusInstance {
            instance_id: now.activation.instance_id.clone(),
        },
    )
    .await
    .unwrap();
    host.action(
        "workspace",
        root,
        "conversation",
        HostAction::PublishContext {
            view_id: view_id.clone(),
            summary: "slide 3 selected".into(),
        },
    )
    .await
    .unwrap();
    let selected_context =
        serde_json::to_value(host.selected_context("workspace", "conversation")).unwrap();
    assert_eq!(
        selected_context[0]["instanceId"],
        now.activation.instance_id
    );
    let listing = host
        .action("workspace", root, "conversation", HostAction::List)
        .await
        .unwrap();
    assert_eq!(
        listing["appContext"][0]["instanceId"],
        now.activation.instance_id
    );
    assert_eq!(listing["appContext"][0]["viewId"], view_id);
    let gateway_listing = selected
        .iter()
        .find(|tool| tool.name() == "plugin_list")
        .unwrap()
        .execute(json!({}), &ctx)
        .await
        .unwrap();
    let gateway_listing = serde_json::from_str::<Value>(&gateway_listing.output).unwrap();
    assert_eq!(
        gateway_listing["appContext"][0]["instanceId"],
        now.activation.instance_id
    );
    let view_call = view_echo.execute(json!({"value":11}), &ctx).await.unwrap();
    assert_eq!(view_call.metadata["gewu.plugin"]["data"]["value"], 11);
    assert_eq!(
        view_call.metadata["gewu.plugin"]["context"]["viewId"],
        view_id
    );

    let async_view_call = selected
        .iter()
        .find(|tool| tool.name() == "plugin_call")
        .unwrap()
        .execute(
            json!({
                "instanceId": now.activation.instance_id,
                "tool": "view_echo",
                "args": {"value": 12},
                "async": true
            }),
            &ctx,
        )
        .await
        .unwrap();
    let async_view_id = serde_json::from_str::<Value>(&async_view_call.output).unwrap()["callId"]
        .as_str()
        .unwrap()
        .to_owned();
    let async_view_status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let reply = selected
                .iter()
                .find(|tool| tool.name() == "plugin_call_status")
                .unwrap()
                .execute(json!({"callId": async_view_id}), &ctx)
                .await
                .unwrap();
            let value = serde_json::from_str::<Value>(&reply.output).unwrap();
            if value["result"] != Value::Null {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(async_view_status["result"]["data"]["value"], 12);
    assert_eq!(async_view_status["context"]["viewId"], view_id);

    let second_view = host
        .action(
            "workspace",
            root,
            "conversation",
            HostAction::Attach {
                instance_id: now.activation.instance_id.clone(),
                view_type: "demo-view".into(),
                resource: None,
            },
        )
        .await
        .unwrap();
    let second_view_id = second_view["viewId"].as_str().unwrap().to_owned();
    host.action(
        "workspace",
        root,
        "conversation",
        HostAction::ViewReady {
            view_id: second_view_id.clone(),
        },
    )
    .await
    .unwrap();
    host.action(
        "workspace",
        root,
        "conversation",
        HostAction::Detach { view_id },
    )
    .await
    .unwrap();
    let after_detach = host
        .action("workspace", root, "conversation", HostAction::List)
        .await
        .unwrap();
    assert_eq!(after_detach["appContext"][0]["viewId"], second_view_id);

    host.action(
        "workspace",
        root,
        "conversation",
        HostAction::SetSession {
            plugin_id: "demo".into(),
            enabled: Some(true),
            profiles: vec![
                "workspace-basic".into(),
                "analysis-read".into(),
                "workflow-authoring".into(),
            ],
        },
    )
    .await
    .unwrap();
    let profiled = host
        .tools("workspace", root.to_owned(), "conversation", &[])
        .await;
    assert!(
        profiled
            .iter()
            .any(|tool| tool.name().contains("workflow_echo"))
    );

    let instance_id = now.activation.instance_id.clone();
    let joined = host
        .action(
            "workspace",
            root,
            "conversation",
            HostAction::Call {
                instance_id: instance_id.clone(),
                tool: "echo".into(),
                args: json!({"value":7}).as_object().unwrap().clone(),
                view_id: None,
                context_ref: None,
            },
        )
        .await
        .unwrap();
    let call_id = joined["callId"].as_str().unwrap().to_owned();
    let actual = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let reply = host
                .action(
                    "workspace",
                    root,
                    "conversation",
                    HostAction::CallStatus {
                        call_id: call_id.clone(),
                    },
                )
                .await
                .unwrap();
            if reply["result"] != Value::Null {
                break reply["result"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        actual["data"]["value"],
        out.metadata["gewu.plugin"]["data"]["value"]
    );
    assert!(
        host.action(
            "workspace",
            root,
            "another-session",
            HostAction::CallStatus { call_id }
        )
        .await
        .is_err()
    );

    // The agent-facing gateway can start a call asynchronously and poll it,
    // instead of blocking a turn on a long-running application tool.
    let gateway = |name: &'static str| {
        selected
            .iter()
            .find(|tool| tool.name() == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    let started = gateway("plugin_call")
        .execute(
            json!({"instanceId": instance_id, "tool": "echo", "args": {"value": 3}, "async": true}),
            &ctx,
        )
        .await
        .unwrap();
    let async_id = serde_json::from_str::<Value>(&started.output).unwrap()["callId"]
        .as_str()
        .unwrap()
        .to_owned();
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let reply = gateway("plugin_call_status")
                .execute(json!({"callId": async_id}), &ctx)
                .await
                .unwrap();
            let value = serde_json::from_str::<Value>(&reply.output).unwrap();
            if value["result"] != Value::Null {
                break value["result"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status["data"]["value"], 3);
    let calls = gateway("plugin_calls")
        .execute(json!({}), &ctx)
        .await
        .unwrap();
    assert!(calls.output.contains(&async_id));

    let result: anyhow::Result<ToolOutput> = now
        .invoke(
            "bad_output",
            Default::default(),
            now.context("bad".into(), None),
            None,
            false,
            false,
        )
        .await;
    assert!(result.is_err());
    // The Agent closes an application through the same scoped gateway the GUI uses.
    let close = selected
        .iter()
        .find(|tool| tool.name() == "plugin_close")
        .unwrap();
    let closed = close
        .execute(json!({"instanceId": instance_id.clone()}), &ctx)
        .await
        .unwrap();
    assert_eq!(closed.output.trim(), r#"{"closed":true}"#);
    assert!(echo.execute(json!({"value": 7}), &ctx).await.is_err());
    assert!(
        now.invoke(
            "echo",
            Default::default(),
            now.context("after-close".into(), None),
            None,
            false,
            false,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn resource_instances_are_shared_but_visible_only_to_attached_sessions() {
    use gewu_protocol::plugin::ResourceRef;

    let package = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    std::fs::create_dir(package.path().join("web")).unwrap();
    for file in ["adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(file), package.path().join(file)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        package.path().join("gewu_plugin.py"),
    )
    .unwrap();
    let manifest = std::fs::read_to_string(fixture.join("plugin.json"))
        .unwrap()
        .replace("\"scope\": \"workspace\"", "\"scope\": \"resource\"");
    std::fs::write(package.path().join("plugin.json"), manifest).unwrap();
    std::fs::create_dir(root.join("doc")).unwrap();

    let host = PluginHost::from_config(
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
    .unwrap();
    let resource = || ResourceRef {
        uri: "workspace:///doc".into(),
        resource_type: "demo.view".into(),
        mime_type: None,
        revision: None,
    };

    let first = host
        .open_for_session(
            "demo",
            "workspace",
            root,
            Some(resource()),
            Some("session-a"),
        )
        .await
        .unwrap();
    assert_eq!(
        host.active_for_session("workspace", root, Some("session-a"))
            .await
            .len(),
        1
    );
    assert!(
        host.active_for_session("workspace", root, Some("session-b"))
            .await
            .is_empty(),
        "another session must not see this instance or its preview"
    );

    let second = host
        .open_for_session(
            "demo",
            "workspace",
            root,
            Some(resource()),
            Some("session-b"),
        )
        .await
        .unwrap();
    assert_eq!(first.activation.instance_id, second.activation.instance_id);
    assert_eq!(
        host.active_for_session("workspace", root, Some("session-b"))
            .await
            .len(),
        1
    );
    host.close_for_session(
        "workspace",
        root,
        &second.activation.instance_id,
        Some("session-b"),
    )
    .await
    .unwrap();
    assert_eq!(
        host.active_for_session("workspace", root, Some("session-a"))
            .await
            .len(),
        1,
        "closing one session must not stop the shared resource instance"
    );

    let attached = host
        .action(
            "workspace",
            root,
            "session-a",
            HostAction::Attach {
                instance_id: first.activation.instance_id.clone(),
                view_type: "demo-view".into(),
                resource: Some(ResourceRef {
                    uri: "workspace:///doc".into(),
                    resource_type: "demo.view".into(),
                    mime_type: None,
                    revision: Some("current".into()),
                }),
            },
        )
        .await
        .unwrap();
    let view_id = attached["viewId"].as_str().unwrap().to_owned();
    host.action(
        "workspace",
        root,
        "session-a",
        HostAction::ViewReady {
            view_id: view_id.clone(),
        },
    )
    .await
    .unwrap();
    host.action(
        "workspace",
        root,
        "session-a",
        HostAction::FocusInstance {
            instance_id: first.activation.instance_id.clone(),
        },
    )
    .await
    .unwrap();
    let started = host
        .action(
            "workspace",
            root,
            "session-a",
            HostAction::Call {
                instance_id: first.activation.instance_id.clone(),
                tool: "view_echo".into(),
                args: json!({"value":13}).as_object().unwrap().clone(),
                view_id: None,
                context_ref: None,
            },
        )
        .await
        .unwrap();
    let call_id = started["callId"].as_str().unwrap().to_owned();
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = host
                .action(
                    "workspace",
                    root,
                    "session-a",
                    HostAction::CallStatus {
                        call_id: call_id.clone(),
                    },
                )
                .await
                .unwrap();
            if status["result"] != Value::Null {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status["status"], "failed");
    assert_eq!(status["result"]["error"]["code"], "CALL_FAILED");
    let changes = host
        .action("workspace", root, "session-a", HostAction::Changes)
        .await
        .unwrap();
    assert_eq!(changes, json!([]));
    host.close_all().await.unwrap();
}
