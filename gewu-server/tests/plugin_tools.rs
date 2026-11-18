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
                },
            )]),
            None,
            Limits::default(),
        )
        .unwrap(),
    );
    let now = host.open("demo", "workspace", root, None).await.unwrap();
    let selected = host.tools("workspace", root.to_owned(), &[]).await;
    let echo = selected
        .iter()
        .find(|tool| tool.name().starts_with("plg_demo_echo_"))
        .unwrap();
    let ctx = ToolContext::new("conversation", "message").with_working_directory(root);
    let out = echo.execute(json!({"value":7}), &ctx).await.unwrap();
    assert_eq!(out.metadata["gewu.plugin"]["data"]["value"], 7);
    assert!(echo.execute(json!({"value":"bad"}), &ctx).await.is_err());

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
    assert_eq!(actual, out.metadata["gewu.plugin"]);
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
    host.close("workspace", root, &instance_id).await.unwrap();
    assert!(echo.execute(json!({"value":7}), &ctx).await.is_err());
}
