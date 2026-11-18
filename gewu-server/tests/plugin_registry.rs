#[cfg(unix)]
#[tokio::test]
async fn opens_a_validated_package_once_per_workspace_and_rejects_escape() {
    use gewu_server::plugins::stdio::Limits;
    use gewu_server::plugins::{
        PluginHost,
        package::{Package, PluginEntry},
    };
    use serde_json::json;
    use std::{collections::BTreeMap, path::Path};
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    for file in ["plugin.json", "adapter.py"] {
        std::fs::copy(fixture.join(file), tmp.path().join(file)).unwrap();
    }
    std::fs::create_dir(tmp.path().join("web")).unwrap();
    std::fs::copy(
        fixture.join("web/index.html"),
        tmp.path().join("web/index.html"),
    )
    .unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        tmp.path().join("gewu_plugin.py"),
    )
    .unwrap();
    let entry = PluginEntry {
        package: tmp.path().to_path_buf(),
        enabled: true,
        settings: Default::default(),
        workspace_settings: Default::default(),
        service_origins: vec![],
        sources: Default::default(),
    };
    let host = PluginHost::from_config(
        &BTreeMap::from([("demo".into(), entry)]),
        None,
        Limits::default(),
    )
    .unwrap();
    let a = host.open("demo", "a", work.path(), None).await.unwrap();
    let again = host.open("demo", "a", work.path(), None).await.unwrap();
    let b = host.open("demo", "b", second.path(), None).await.unwrap();
    assert_eq!(a.activation.instance_id, again.activation.instance_id);
    assert_ne!(a.activation.instance_id, b.activation.instance_id);
    use gewu_protocol::plugin::HostAction;
    host.action(
        "a",
        work.path(),
        "s",
        HostAction::SetEnabled {
            plugin_id: "demo".into(),
            enabled: false,
        },
    )
    .await
    .unwrap();
    assert!(host.open("demo", "a", work.path(), None).await.is_err());
    assert!(host.active("b", second.path()).await.is_empty());
    host.action(
        "a",
        work.path(),
        "s",
        HostAction::SetEnabled {
            plugin_id: "demo".into(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    let reopened = host.open("demo", "a", work.path(), None).await.unwrap();
    assert!(reopened.activation.generation > a.activation.generation);
    host.close_all().await.unwrap();
    let bad = json!({"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"0.1.0","scope":"workspace","backend":{"connector":"gewu-stdio","config":{"entry":"../outside"}}});
    std::fs::write(tmp.path().join("plugin.json"), bad.to_string()).unwrap();
    assert!(Package::read(tmp.path()).is_err());
}
