use gewu_server::plugins::state::{GenerationState, StateStore};

fn generation(id: &str) -> GenerationState {
    GenerationState {
        release_id: id.into(),
        package_path: format!("plugins/demo/generations/{id}/package"),
        runtime_path: None,
        package_sha256: "a".repeat(64),
        runtime_sha256: None,
        installed_at: "2026-11-22T00:00:00Z".into(),
        runtime: None,
    }
}

fn stage(store: &StateStore, id: &str) {
    let manifest = format!(
        r#"{{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"{}","scope":"workspace","backend":{{"connector":"gewu-stdio","config":{{"entry":"adapter"}}}}}}"#,
        id.strip_prefix("demo-").unwrap()
    );
    store
        .stage_generation("demo", &generation(id), |root| {
            let package = root.join("package");
            std::fs::create_dir(&package)?;
            std::fs::write(package.join("plugin.json"), manifest)?;
            Ok(())
        })
        .unwrap();
    store
        .commit_active("demo", generation(id), "stable")
        .unwrap();
}

#[test]
fn recovery_restores_the_old_remove_window_only_when_unique() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    stage(&store, "demo-1.0.0");
    let root = store.generation_root("demo", "demo-1.0.0").unwrap();
    let trash = root.parent().unwrap().join(".generation-remove-crash");
    std::fs::rename(&root, &trash).unwrap();

    store.recover().unwrap();
    assert!(root.join("package/plugin.json").is_file());
    assert!(!trash.exists());
    assert!(
        store.load().unwrap().plugins["demo"]
            .generations
            .contains_key("demo-1.0.0")
    );
}

#[test]
fn recovery_refuses_to_guess_between_two_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    stage(&store, "demo-1.0.0");
    let root = store.generation_root("demo", "demo-1.0.0").unwrap();
    let parent = root.parent().unwrap();
    for name in [".generation-remove-a", ".generation-remove-b"] {
        let trash = parent.join(name);
        std::fs::create_dir_all(trash.join("package")).unwrap();
        std::fs::write(
            trash.join("package/plugin.json"),
            r#"{"id":"demo","version":"1.0.0"}"#,
        )
        .unwrap();
    }
    std::fs::remove_dir_all(&root).unwrap();

    let error = store.recover().unwrap_err().to_string();
    assert!(error.contains("multiple"), "{error}");
    assert!(!root.exists());
}

#[test]
fn removing_a_generation_does_not_roll_back_when_the_derived_pointer_fails() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    stage(&store, "demo-1.0.0");
    stage(&store, "demo-2.0.0");
    let active_pointer = store.root().join("plugins/demo/active.json");
    std::fs::remove_file(&active_pointer).unwrap();
    std::fs::create_dir(&active_pointer).unwrap();

    store.remove_generation("demo", "demo-2.0.0").unwrap();
    let state = store.load().unwrap();
    assert_eq!(
        state.plugins["demo"].active_generation.as_deref(),
        Some("demo-1.0.0")
    );
    assert!(
        !store
            .generation_root("demo", "demo-2.0.0")
            .unwrap()
            .exists()
    );
}
