#![cfg(unix)]
//! `gewu plugin install` puts packages in `~/.gewu/plugins`; the Plugins panel
//! must show them before any config entry exists. One test per binary so
//! setting HOME cannot race another test.
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, stdio::Limits};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter")
}

#[tokio::test]
async fn installed_but_unconfigured_packages_are_listed_and_refuse_enabling() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let installed = home.path().join(".gewu/plugins/demo/0.1.0");
    std::fs::create_dir_all(installed.join("web")).unwrap();
    for file in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture().join(file), installed.join(file)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        installed.join("gewu_plugin.py"),
    )
    .unwrap();
    // SAFETY: this binary has a single test, so no other thread reads the env.
    unsafe { std::env::set_var("HOME", home.path()) };

    let host = PluginHost::from_config(&BTreeMap::new(), None, Limits::default()).unwrap();
    let catalog = host.catalog();
    assert_eq!(catalog.len(), 1, "{catalog:?}");
    assert_eq!(catalog[0]["id"], "demo");
    assert_eq!(catalog[0]["version"], "0.1.0");
    assert_eq!(catalog[0]["configured"], false);
    assert_eq!(catalog[0]["enabled"], false);

    let error = host
        .action(
            "workspace",
            workspace.path(),
            "conversation",
            HostAction::SetEnabled {
                plugin_id: "demo".into(),
                enabled: true,
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not configured"), "{error}");

    // Enabling a configured plugin is remembered for the next start.
    unsafe { std::env::set_var("HOME", home.path()) };
    let entries = BTreeMap::from([(
        "demo".to_string(),
        serde_json::from_value(json!({
            "package": installed,
            "enabled": true,
            "settings": {},
            "sources": {"adapter": fixture(), "application": workspace.path()},
        }))
        .unwrap(),
    )]);
    let host = PluginHost::from_config(&entries, None, Limits::default()).unwrap();
    assert_eq!(host.catalog()[0]["enabled"], true);
    assert_eq!(host.catalog()[0]["package"], json!(installed));
    assert_eq!(host.catalog()[0]["sources"]["adapter"], json!(fixture()));
    assert_eq!(host.catalog()[0]["sources"]["application"], json!(workspace.path()));
    host.action(
        "workspace",
        workspace.path(),
        "conversation",
        HostAction::SetEnabled {
            plugin_id: "demo".into(),
            enabled: false,
        },
    )
    .await
    .unwrap();
    let state = std::fs::read(home.path().join(".gewu/plugin-state.json")).unwrap();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&state).unwrap()["demo"], false);
    let restarted =
        PluginHost::from_config(&entries, None, Limits::default()).unwrap();
    assert_eq!(restarted.catalog()[0]["enabled"], false);
}
