use gewu_server::plugins::state::{GenerationState, StateStore};
use std::collections::BTreeMap;

fn generation(id: &str) -> GenerationState {
    GenerationState {
        release_id: id.into(),
        package_path: format!("plugins/paper/generations/{id}/package"),
        runtime_path: Some(format!("plugins/paper/generations/{id}/runtime")),
        package_sha256: "a".repeat(64),
        runtime_sha256: Some("b".repeat(64)),
        installed_at: "2026-11-19T00:00:00Z".into(),
        runtime: None,
    }
}

#[test]
fn concurrent_commits_preserve_both_plugins() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path());
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for plugin in ["paper", "manimble"] {
            let (store, barrier) = (&store, &barrier);
            scope.spawn(move || {
                barrier.wait();
                store
                    .commit_active(plugin, generation(&format!("{plugin}-1.0.0")), "stable")
                    .unwrap();
            });
        }
    });
    assert_eq!(store.load().unwrap().plugins.len(), 2);
}

#[test]
fn uninstall_preserves_another_plugins_update_while_waiting_for_state_lock() {
    use std::{
        fs::OpenOptions,
        time::{Duration, Instant},
    };
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path());
    let paper = generation("paper-1.0.0");
    store.stage_generation("paper", &paper, |_| Ok(())).unwrap();
    store.commit_active("paper", paper, "stable").unwrap();
    store
        .commit_active("manimble", generation("manimble-1.0.0"), "stable")
        .unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join("plugins/state.lock"))
        .unwrap();
    lock.lock().unwrap();
    let worker_store = store.clone();
    let worker = std::thread::spawn(move || worker_store.uninstall("paper"));
    let lease = temp.path().join("leases/paper/paper-1.0.0");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(&lease)
            && file.try_lock().is_err()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "uninstall did not acquire its generation lease"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // This writer owns the state lock while uninstall is waiting for it.
    let mut state = store.load().unwrap();
    state.plugins.get_mut("manimble").unwrap().enabled = false;
    store.save(&state).unwrap();
    lock.unlock().unwrap();
    worker.join().unwrap().unwrap();
    let state = store.load().unwrap();
    assert!(!state.plugins.contains_key("paper"));
    assert!(
        !state.plugins["manimble"].enabled,
        "uninstall replaced another plugin's newer state"
    );
}

#[test]
fn active_pointer_is_atomic_and_rollback_keeps_user_data_outside_plugin_root() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("keep.txt"), "keep").unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    let first = generation("paper-0.1.0");
    let second = generation("paper-0.2.0");
    store
        .stage_generation("paper", &first, |root| {
            std::fs::create_dir_all(root.join("package"))?;
            Ok(())
        })
        .unwrap();
    store.commit_active("paper", first, "stable").unwrap();
    store
        .stage_generation("paper", &second, |root| {
            std::fs::create_dir_all(root.join("runtime"))?;
            Ok(())
        })
        .unwrap();
    store.commit_active("paper", second, "stable").unwrap();
    assert_eq!(store.rollback("paper").unwrap(), "paper-0.1.0");
    let state = store.load().unwrap();
    let paper = state.plugins.get("paper").unwrap();
    assert_eq!(paper.active_generation.as_deref(), Some("paper-0.1.0"));
    assert_eq!(
        paper.generations,
        BTreeMap::from([
            ("paper-0.1.0".into(), generation("paper-0.1.0")),
            ("paper-0.2.0".into(), generation("paper-0.2.0")),
        ])
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("keep.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn failed_staging_does_not_create_a_generation_or_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    assert!(
        store
            .stage_generation("paper", &generation("paper-0.2.0"), |_root| anyhow::bail!(
                "probe failed"
            ))
            .is_err()
    );
    assert!(
        !store
            .root()
            .join("plugins/paper/generations/paper-0.2.0")
            .exists()
    );
    assert!(store.load().unwrap().plugins.is_empty());
}

#[test]
fn removing_one_generation_preserves_the_other_generation_and_rollback_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    let first = generation("paper-0.1.0");
    let second = generation("paper-0.2.0");
    store
        .stage_generation("paper", &first, |root| {
            std::fs::create_dir_all(root.join("package"))?;
            Ok(())
        })
        .unwrap();
    store.commit_active("paper", first, "stable").unwrap();
    store
        .stage_generation("paper", &second, |root| {
            std::fs::create_dir_all(root.join("package"))?;
            Ok(())
        })
        .unwrap();
    store.commit_active("paper", second, "stable").unwrap();

    store.remove_generation("paper", "paper-0.1.0").unwrap();
    let state = store.load().unwrap();
    let paper = state.plugins.get("paper").unwrap();
    assert_eq!(paper.active_generation.as_deref(), Some("paper-0.2.0"));
    assert_eq!(paper.previous_generation, None);
    assert!(
        !store
            .generation_root("paper", "paper-0.1.0")
            .unwrap()
            .exists()
    );
    assert!(
        store
            .generation_root("paper", "paper-0.2.0")
            .unwrap()
            .exists()
    );
}

#[test]
fn removing_the_active_generation_rolls_back_to_the_previous_generation() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    let first = generation("paper-0.1.0");
    let second = generation("paper-0.2.0");
    for value in [first, second] {
        store
            .stage_generation("paper", &value, |root| {
                std::fs::create_dir_all(root.join("package"))?;
                Ok(())
            })
            .unwrap();
        store.commit_active("paper", value, "stable").unwrap();
    }

    store.remove_generation("paper", "paper-0.2.0").unwrap();
    let state = store.load().unwrap();
    let paper = state.plugins.get("paper").unwrap();
    assert_eq!(paper.active_generation.as_deref(), Some("paper-0.1.0"));
    assert_eq!(paper.previous_generation, None);
}
