#![cfg(unix)]
use gewu_core::types::ToolContext;
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

#[tokio::test]
async fn agent_catalog_discovers_large_tools_without_dumping_every_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    let pkg = tmp.path().join("plugin");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(pkg.join("web")).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    for f in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(f), pkg.join(f)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        pkg.join("gewu_plugin.py"),
    )
    .unwrap();
    let adapter = std::fs::read_to_string(pkg.join("adapter.py"))
        .unwrap()
        .replace(
            "\"inputSchema\": {\"type\": \"object\"",
            "\"inputSchema\": {\"description\": \"x\" * 6000, \"type\": \"object\"",
        );
    std::fs::write(pkg.join("adapter.py"), adapter).unwrap();
    let host = Arc::new(
        PluginHost::from_config(
            &BTreeMap::from([(
                "demo".into(),
                PluginEntry {
                    package: pkg,
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
    let instance = host
        .open_for_session("demo", "w", &root, None, Some("s"))
        .await
        .unwrap();
    let tools = host.tools("w", root.clone(), "s", &[]).await;
    let context = ToolContext::new("s", "message").with_working_directory(&root);
    let list = tools
        .iter()
        .find(|t| t.name() == "plugin_list")
        .unwrap()
        .execute(json!({}), &context)
        .await
        .expect("A large application must remain discoverable");
    let list: Value = serde_json::from_str(&list.output).unwrap();
    assert_eq!(list["instances"][0]["tools"].as_array().unwrap().len(), 5);
    assert!(
        list["instances"][0]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("echo"))
    );
    assert!(
        !list["instances"][0]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("workflow_echo"))
    );
    assert!(list["instances"][0]["tools"][0].is_string());
    let ui = host
        .action("w", &root, "s", HostAction::List)
        .await
        .unwrap();
    assert_eq!(
        ui["instances"][0]["tools"][0]["inputSchema"]["description"]
            .as_str()
            .unwrap()
            .len(),
        6000
    );
    let described = tools
        .iter()
        .find(|t| t.name() == "plugin_describe")
        .unwrap()
        .execute(
            json!({"instanceId":instance.activation.instance_id,"tool":"echo"}),
            &context,
        )
        .await
        .unwrap();
    assert!(described.output.contains(&"x".repeat(6000)));
    host.close_all().await.unwrap();
}
