use gewu_protocol::plugin::{HostAction, ResourceRef};
use gewu_server::plugins::PluginHost;

#[tokio::test]
async fn editor_does_not_overwrite_a_changed_revision() {
    let host = PluginHost::default();
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("a.txt");
    std::fs::write(&file, "before").unwrap();
    let resource = ResourceRef {
        uri: "workspace:///a.txt".into(),
        resource_type: "text".into(),
        mime_type: None,
        revision: None,
    };
    let before = host
        .action(
            "w",
            root.path(),
            "s",
            HostAction::ReadText {
                resource: resource.clone(),
            },
        )
        .await
        .unwrap();
    let expected = before["revision"].as_str().unwrap().to_owned();
    std::fs::write(&file, "external").unwrap();
    let save = HostAction::SaveText {
        resource,
        content: "replacement".into(),
        expected,
    };
    assert!(
        host.action("w", root.path(), "s", save)
            .await
            .unwrap_err()
            .to_string()
            .contains("REVISION_CONFLICT")
    );
    assert_eq!(std::fs::read_to_string(file).unwrap(), "external");
}
