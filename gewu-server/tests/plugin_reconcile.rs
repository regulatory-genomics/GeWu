#![cfg(unix)]
//! A generation installed by `gewu plugin install` while the host runs must show
//! up on the next catalog/list request instead of needing a restart. One test
//! per binary so setting HOME cannot race another test.
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, stdio::Limits};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, time::Duration};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter")
}

#[tokio::test]
async fn a_cli_install_is_reconciled_by_the_next_list() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // SAFETY: single test in this binary; no other thread reads the environment.
    unsafe { std::env::set_var("HOME", home.path()) };
    let config = home.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let host = PluginHost::from_config(&BTreeMap::new(), Some(&config), Limits::default()).unwrap();
    assert!(!host.catalog().iter().any(|entry| entry["id"] == "demo"));

    // What `gewu plugin install` writes from another process.
    let root = home.path().join(".gewu");
    let package = root.join("plugins/demo/generations/demo-0.1.0/package");
    std::fs::create_dir_all(package.join("web")).unwrap();
    for file in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture().join(file), package.join(file)).unwrap();
    }
    std::fs::create_dir_all(package.join("lib")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        package.join("lib/gewu_plugin.py"),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(20)); // a new mtime on coarse filesystems
    std::fs::write(
        root.join("plugins/state.json"),
        serde_json::to_vec(&json!({
            "version": 1,
            "plugins": {"demo": {
                "channel": "stable", "enabled": true,
                "activeGeneration": "demo-0.1.0",
                "generations": {"demo-0.1.0": {
                    "releaseId": "demo-0.1.0",
                    "packagePath": "plugins/demo/generations/demo-0.1.0/package",
                    "runtimePath": null, "packageSha256": "local-directory",
                    "runtimeSha256": null, "installedAt": "2026-11-24T00:00:00Z"
                }}
            }}
        }))
        .unwrap(),
    )
    .unwrap();

    let listed = host
        .action("workspace", workspace.path(), "conversation", HostAction::List)
        .await
        .unwrap();
    let demo = listed["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "demo")
        .expect("the CLI-installed plugin is listed");
    assert_eq!(demo["configured"], true);
    assert_eq!(demo["enabled"], true);
    assert_eq!(demo["version"], "0.1.0");
    assert!(demo["package"]
        .as_str()
        .unwrap()
        .ends_with("plugins/demo/generations/demo-0.1.0/package"));
}
