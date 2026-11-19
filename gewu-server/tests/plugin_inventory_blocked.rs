#[test]
fn invalid_installed_packages_are_reported_as_blocked_entries() {
    let home = tempfile::tempdir().unwrap();
    let package = home.path().join(".gewu/plugins/broken/1.0.0");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("plugin.json"), "not-json").unwrap();
    unsafe { std::env::set_var("HOME", home.path()) };
    let host = gewu_server::plugins::PluginHost::from_config(
        &Default::default(),
        None,
        gewu_server::plugins::stdio::Limits::default(),
    )
    .unwrap();
    let entry = host.catalog().into_iter().find(|entry| entry["blocked"] == true).unwrap();
    assert_eq!(entry["id"], "broken");
    assert!(!entry["error"].as_str().unwrap().is_empty());
}
