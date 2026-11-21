use gewu_server::plugins::diagnostics::{self, Attempt};
use serde_json::json;

#[test]
fn reads_only_own_attempt_and_recognizes_interruption_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let report = Attempt::start(
        root,
        None,
        json!({"plugin":"demo"}),
        Some(("work", "session")),
        root,
    )
    .unwrap();
    let id = report.id().to_string();
    assert_eq!(
        diagnostics::read(root, &id, Some(("work", "session"))).unwrap()["report"]["outcome"],
        "running"
    );
    assert!(diagnostics::read(root, &id, Some(("work", "other"))).is_err());
    assert!(diagnostics::read(root, "../state.json", None).is_err());
    assert!(Attempt::start(root, Some(&id), json!({"plugin":"demo"}), None, root).is_err());
    drop(report);
    assert_eq!(
        diagnostics::read(root, &id, None).unwrap()["report"]["outcome"],
        "interrupted"
    );
    let persisted: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("install-attempts").join(id).join("report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["outcome"], "running");
}
