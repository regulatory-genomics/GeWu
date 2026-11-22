use super::*;
use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

#[test]
fn a_plugin_mutation_lock_blocks_a_concurrent_uninstall() {
    let temp = tempfile::tempdir().unwrap();
    let store = state::StateStore::new(temp.path().join(".gewu"));
    let generation = state::GenerationState {
        release_id: "demo-1.0.0".into(),
        package_path: "plugins/demo/generations/demo-1.0.0/package".into(),
        runtime_path: None,
        package_sha256: "a".repeat(64),
        runtime_sha256: None,
        installed_at: "2026-11-22T00:00:00Z".into(),
        runtime: None,
    };
    store
        .stage_generation("demo", &generation, |root| {
            fs::create_dir(root.join("package"))?;
            Ok(())
        })
        .unwrap();
    store.commit_active("demo", generation, "stable").unwrap();
    let held = store.lock_plugin_mutation("demo").unwrap();
    assert!(store.uninstall("demo").is_err());
    drop(held);
    store.uninstall("demo").unwrap();
}

#[tokio::test]
async fn cancelled_activation_keeps_generation_leased_until_child_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let store = state::StateStore::new(temp.path().join(".gewu"));
    let generation = state::GenerationState {
        release_id: "demo-1.0.0".into(),
        package_path: "plugins/demo/generations/demo-1.0.0/package".into(),
        runtime_path: None,
        package_sha256: "a".repeat(64),
        runtime_sha256: None,
        installed_at: "2026-11-22T00:00:00Z".into(),
        runtime: None,
    };
    store.stage_generation("demo", &generation, |root| {
        let package = root.join("package");
        fs::create_dir(&package)?;
        fs::write(package.join("plugin.json"), r#"{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"1.0.0","scope":"workspace","backend":{"connector":"gewu-stdio","config":{"entry":"adapter"}}}"#)?;
        let adapter = package.join("adapter");
        fs::write(&adapter, "#!/bin/sh\necho $$ > adapter.pid\nexec sleep 60\n")?;
        fs::set_permissions(adapter, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }).unwrap();
    store.commit_active("demo", generation, "stable").unwrap();
    let host = PluginHost {
        installation_root: Some(store.root().into()),
        ..PluginHost::default()
    };
    host.refresh_installation("demo").unwrap();
    let mut opening = Box::pin(host.open("demo", "workspace", temp.path(), None));
    let pid_file = temp.path().join("adapter.pid");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pid_file.exists() {
            tokio::select! {
                result = &mut opening => panic!("activation unexpectedly finished: {}", result.is_ok()),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }).await.unwrap();
    assert!(store.uninstall("demo").is_err());
    drop(opening);
    // No yield: the child supervisor has not had a chance to finish cleanup.
    assert!(
        store.uninstall("demo").is_err(),
        "cancellation released a live child's generation"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store.uninstall("demo").is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
