#[test]
fn invalid_installed_packages_are_reported_as_blocked_entries() {
    let home = tempfile::tempdir().unwrap();
    let package = home.path().join(".gewu/plugins/broken/1.0.0");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("plugin.json"), "not-json").unwrap();
    let (packages, issues) =
        gewu_server::plugins::package::installed_report(&home.path().join(".gewu/plugins"))
            .unwrap();
    assert!(packages.is_empty());
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].root, package);
    assert!(!issues[0].error.is_empty());
}
