use gewu_core::types::ToolContext;
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc};

#[tokio::test]
async fn all_agent_gateways_enforce_profiles_with_or_without_context_provider() {
    for provider in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(package.join("web")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
        for file in ["plugin.json", "adapter.py", "web/index.html"] {
            std::fs::copy(fixture.join(file), package.join(file)).unwrap();
        }
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"), package.join("gewu_plugin.py")).unwrap();
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(package.join("plugin.json")).unwrap()).unwrap();
        if provider {
            manifest["contextProvider"] = json!({"tool":"context.get"});
            manifest["settingsSchema"]["properties"]["contextFixture"] = json!({"type":"boolean"});
            std::fs::write(package.join("plugin.json"), manifest.to_string()).unwrap();
        }
        let host = Arc::new(PluginHost::from_config(&BTreeMap::from([("demo".into(), PluginEntry {
            package, enabled:true, settings: if provider {json!({"contextFixture":true}).as_object().unwrap().clone()} else {Default::default()},
            workspace_settings:Default::default(), service_origins:vec![], sources:Default::default(),
        })]), None, Limits::default()).unwrap());
        let instance = host.open("demo", "w", &root, None).await.unwrap();
        let ctx = ToolContext::new("s", "m").with_working_directory(&root);
        let tools = host.tools("w", root.clone(), "s", &[]).await;
        for gateway in ["plugin_describe", "plugin_call"] {
            let tool = tools.iter().find(|tool| tool.name() == gateway).unwrap();
            for asynchronous in [false, true] {
                let result = tool.execute(json!({"instanceId":instance.activation.instance_id,"tool":"workflow_echo","args":{"value":42},"async":asynchronous}), &ctx).await;
                assert!(result.is_err(), "{gateway} bypassed disabled workflow group (provider={provider}, async={asynchronous})");
            }
        }
        let set_profiles = |profiles| HostAction::SetSession { plugin_id:"demo".into(), enabled:Some(true), profiles };
        host.action("w", &root, "s", set_profiles(vec!["workspace-basic".into(), "workflow-authoring".into()])).await.unwrap();
        let enabled = host.tools("w", root.clone(), "s", &[]).await;
        let workflow = enabled.iter().find(|tool| tool.name().contains("workflow_echo")).unwrap();
        assert_eq!(workflow.execute(json!({"value":42}), &ctx).await.unwrap().metadata["gewu.plugin"]["data"]["value"], 42);
        host.action("w", &root, "s", set_profiles(vec!["workspace-basic".into()])).await.unwrap();
        assert!(workflow.execute(json!({"value":42}), &ctx).await.is_err(), "an already-advertised typed tool ignored a profile change");
        host.close_all().await.unwrap();
    }
}
