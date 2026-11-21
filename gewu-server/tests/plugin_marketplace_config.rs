#![cfg(unix)]
//! A configured plugin may omit `package` and use the active marketplace generation.

use gewu_server::plugins::{PluginHost, package::PluginEntry, stdio::Limits};
use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn empty_package_path_resolves_to_the_active_generation() {
    let home = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    let package = home.path().join(".gewu/plugins/demo/generations/demo-1.0.0/package");
    std::fs::create_dir_all(package.join("web")).unwrap();
    for file in ["plugin.json", "adapter.py", "web/index.html"] {
        std::fs::copy(fixture.join(file), package.join(file)).unwrap();
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python/gewu_plugin.py"),
        package.join("gewu_plugin.py"),
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join(".gewu/plugins")).unwrap();
    std::fs::write(
        home.path().join(".gewu/plugins/state.json"),
        serde_json::json!({
            "version": 1,
            "plugins": {
                "demo": {
                    "channel": "stable",
                    "enabled": true,
                    "activeGeneration": "demo-1.0.0",
                    "generations": {
                        "demo-1.0.0": {
                            "releaseId": "demo-1.0.0",
                            "packagePath": "plugins/demo/generations/demo-1.0.0/package",
                            "runtimePath": null,
                            "packageSha256": "local",
                            "installedAt": "local"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    // SAFETY: this binary has one test, so no other thread reads the environment.
    unsafe { std::env::set_var("HOME", home.path()) };

    let host = PluginHost::from_config(
        &BTreeMap::from([(
            "demo".to_string(),
            PluginEntry {
                package: Default::default(),
                enabled: true,
                settings: Default::default(),
                workspace_settings: Default::default(),
                service_origins: Vec::new(),
                sources: Default::default(),
            },
        )]),
        None,
        Limits::default(),
    )
    .unwrap();
    let entry = host.catalog().into_iter().find(|entry| entry["id"] == "demo").unwrap();
    assert_eq!(entry["enabled"], true);
    assert_eq!(entry["package"], serde_json::json!(package));
}
