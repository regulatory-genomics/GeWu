use gewu_server::plugins::state::{GenerationState, StateStore};
use std::collections::BTreeMap;

fn generation(id: &str) -> GenerationState {
    GenerationState {
        release_id: id.into(), package_path: format!("plugins/paper/generations/{id}/package"), runtime_path: Some(format!("plugins/paper/generations/{id}/runtime")),
        package_sha256: "a".repeat(64), runtime_sha256: Some("b".repeat(64)), installed_at: "2026-11-19T00:00:00Z".into(),
    }
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
    store.stage_generation("paper", &first, |root| {
        std::fs::create_dir_all(root.join("package"))?;
        Ok(())
    }).unwrap();
    store.commit_active("paper", first, "stable").unwrap();
    store.stage_generation("paper", &second, |root| {
        std::fs::create_dir_all(root.join("runtime"))?;
        Ok(())
    }).unwrap();
    store.commit_active("paper", second, "stable").unwrap();
    assert_eq!(store.rollback("paper").unwrap(), "paper-0.1.0");
    let state = store.load().unwrap();
    let paper = state.plugins.get("paper").unwrap();
    assert_eq!(paper.active_generation.as_deref(), Some("paper-0.1.0"));
    assert_eq!(paper.generations, BTreeMap::from([
        ("paper-0.1.0".into(), generation("paper-0.1.0")),
        ("paper-0.2.0".into(), generation("paper-0.2.0")),
    ]));
    assert_eq!(std::fs::read_to_string(workspace.join("keep.txt")).unwrap(), "keep");
}

#[test]
fn failed_staging_does_not_create_a_generation_or_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(temp.path().join(".gewu"));
    assert!(store.stage_generation("paper", &generation("paper-0.2.0"), |_root| anyhow::bail!("probe failed")).is_err());
    assert!(!store.root().join("plugins/paper/generations/paper-0.2.0").exists());
    assert!(store.load().unwrap().plugins.is_empty());
}
