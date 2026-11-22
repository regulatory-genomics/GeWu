#![cfg(unix)]

use gewu_core::types::ToolContext;
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

#[tokio::test]
async fn headless_context_is_session_local_and_both_gateways_keep_the_original_ref() {
    let package = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = here.join("tests/fixtures/plugin-adapter");
    std::fs::create_dir(package.path().join("web")).unwrap();
    for file in ["adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(file), package.path().join(file)).unwrap();
    }
    std::fs::copy(
        here.join("../sdk/python/gewu_plugin.py"),
        package.path().join("gewu_plugin.py"),
    )
    .unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("plugin.json")).unwrap()).unwrap();
    manifest["scope"] = json!("session");
    manifest["contextProvider"] = json!({"tool":"context.get"});
    manifest["settingsSchema"]["properties"]["contextFixture"] = json!({"type":"boolean"});
    std::fs::write(package.path().join("plugin.json"), manifest.to_string()).unwrap();
    std::fs::create_dir_all(package.path().join("skills/guide")).unwrap();
    std::fs::write(package.path().join("skills/guide/SKILL.md"),
        "---\nname: guide\ndescription: Use the demo application\n---\nRead context.get before writing; opening Preview alone creates no view.\n").unwrap();
    let host = Arc::new(
        PluginHost::from_config(
            &BTreeMap::from([(
                "demo".into(),
                PluginEntry {
                    package: package.path().into(),
                    enabled: true,
                    settings: json!({"contextFixture":true}).as_object().unwrap().clone(),
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

    let cold = host.tools("workspace", root.into(), "a", &[]).await;
    assert!(
        host.active_for_session("workspace", root, Some("a"))
            .await
            .is_empty(),
        "building tools for ordinary chat must not activate a provider"
    );
    let source = host.context_source("workspace", root.into(), "a");
    assert!(source.inference_context().await.unwrap().text.is_empty());
    let skill = cold
        .iter()
        .find(|tool| tool.name() == "plugin_skill")
        .expect("instructions are available before activation");
    let guide = skill
        .execute(
            json!({"name":"demo:guide"}),
            &ToolContext::new("a", "guide"),
        )
        .await
        .unwrap();
    assert!(guide.output.contains("Read context.get"));
    assert!(
        host.active_for_session("workspace", root, Some("a"))
            .await
            .is_empty()
    );
    let activate = cold
        .iter()
        .find(|tool| tool.name() == "plugin_activate")
        .unwrap();
    let activation = activate
        .execute(
            json!({"pluginId":"demo"}),
            &ToolContext::new("a", "activate"),
        )
        .await
        .unwrap();
    let activation: Value = serde_json::from_str(&activation.output).unwrap();
    assert!(
        !activation["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("workflow_echo")),
        "default profiles cannot call workflow tools"
    );
    assert!(
        activation["instructions"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Read context.get")
    );
    let listed = cold
        .iter()
        .find(|tool| tool.name() == "plugin_list")
        .unwrap()
        .execute(json!({}), &ToolContext::new("a", "list"))
        .await
        .unwrap();
    let listed: Value = serde_json::from_str(&listed.output).unwrap();
    assert!(
        !listed["instances"][0]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("workflow_echo"))
    );
    let tools = host.tools("workspace", root.into(), "a", &[]).await;
    let active = host.active_for_session("workspace", root, Some("a")).await;
    assert_eq!(
        active.len(),
        1,
        "tools must be ready before any Preview opens"
    );
    let id = &active[0].activation.instance_id;
    let old = source.inference_context().await.unwrap();
    assert!(old.text.contains("demo://session/a"));
    std::fs::write(root.join("fixture-revision"), "r1").unwrap();
    assert!(
        source
            .inference_context()
            .await
            .unwrap()
            .text
            .contains("r1")
    );
    assert!(
        host.active_for_session("workspace", root, Some("b"))
            .await
            .is_empty()
    );
    host.action(
        "workspace",
        root,
        "b",
        HostAction::Activate {
            plugin_id: "demo".into(),
        },
    )
    .await
    .unwrap();
    let other = host
        .context_source("workspace", root.into(), "b")
        .inference_context()
        .await
        .unwrap();
    assert!(other.text.contains("demo://session/b"));
    assert!(!other.text.contains("demo://session/a"));

    let typed = tools
        .iter()
        .find(|tool| tool.name().starts_with("plg_demo_context_write_"))
        .unwrap();
    let gateway = tools
        .iter()
        .find(|tool| tool.name() == "plugin_call")
        .unwrap();
    let mut ctx = ToolContext::new("a", "message").with_working_directory(root);
    assert!(
        typed
            .execute(json!({}), &ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("CONTEXT_UNAVAILABLE")
    );
    ctx.metadata.extend(other.metadata);
    assert!(
        typed
            .execute(json!({}), &ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("CONTEXT_UNAVAILABLE")
    );
    ctx.metadata.extend(old.metadata);
    let pinned = typed.execute(json!({}), &ctx).await.unwrap();
    assert_eq!(
        pinned.metadata["gewu.plugin"]["data"]["ref"]["revision"],
        "r0"
    );

    for deferred in [false, true] {
        let reply = gateway
            .execute(
                json!({"instanceId":id,"tool":"context.write","args":{},"async":deferred}),
                &ctx,
            )
            .await
            .unwrap();
        if deferred {
            let call = serde_json::from_str::<Value>(&reply.output).unwrap();
            let call_id = call["callId"].as_str().unwrap().to_owned();
            let status = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let status = host
                        .action(
                            "workspace",
                            root,
                            "a",
                            HostAction::CallStatus {
                                call_id: call_id.clone(),
                            },
                        )
                        .await
                        .unwrap();
                    if !status["result"].is_null() {
                        break status;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(status["result"]["data"]["ref"]["revision"], "r0");
        } else {
            assert_eq!(
                reply.metadata["gewu.plugin"]["data"]["ref"]["revision"],
                "r0"
            );
        }
    }
    host.action(
        "workspace",
        root,
        "a",
        HostAction::SetSession {
            plugin_id: "demo".into(),
            enabled: Some(true),
            profiles: vec!["analysis-read".into()],
        },
    )
    .await
    .unwrap();
    for deferred in [false, true] {
        let error = gateway
            .execute(
                json!({"instanceId":id,"tool":"context.write","args":{},"async":deferred}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("PLUGIN_PROFILE_DISABLED"),
            "{error}"
        );
    }
    host.close_all().await.unwrap();
}
