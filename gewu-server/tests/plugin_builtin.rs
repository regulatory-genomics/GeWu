#[test]
fn bundled_marketplace_is_materialized_from_the_binary_and_protected() {
    let home = tempfile::tempdir().unwrap();
    let invalid = home.path().join(".gewu/plugins/broken/1.0.0");
    std::fs::create_dir_all(&invalid).unwrap();
    std::fs::write(invalid.join("plugin.json"), "not-json").unwrap();
    // SAFETY: this binary has one test, so no other thread reads the environment.
    unsafe { std::env::set_var("HOME", home.path()) };
    let host = gewu_server::plugins::PluginHost::from_config(
        &Default::default(),
        None,
        gewu_server::plugins::stdio::Limits::default(),
    )
    .unwrap();
    let entry = host
        .catalog()
        .into_iter()
        .find(|entry| entry["id"] == "gewu-marketplace")
        .unwrap();
    assert_eq!(entry["builtin"], true);
    assert_eq!(entry["enabled"], true);
    let materialized = home
        .path()
        .join(".gewu/builtin-plugins/gewu-marketplace/0.1.0");
    assert!(
        materialized.join("plugin.json").is_file(),
        "{materialized:?}"
    );
    let blocked = host
        .catalog()
        .into_iter()
        .find(|entry| entry["id"] == "broken")
        .unwrap();
    assert_eq!(blocked["blocked"], true);
    assert!(!blocked["error"].as_str().unwrap().is_empty());
}
