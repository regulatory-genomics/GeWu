#![cfg(unix)]
use gewu_protocol::release::{RuntimeRef, Target};
use gewu_server::plugins::runtime::{RuntimeBundle, RuntimeError, current_target, select_runtime};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn executable(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn target() -> Target {
    current_target().unwrap()
}

fn bundle(root: &std::path::Path, probe: &str) {
    let target = target();
    std::fs::write(root.join("runtime.json"), serde_json::json!({
        "schemaVersion":"gewu.runtime/v1", "id":"paper-runtime", "version":"0.2.0", "plugin":"paper", "target":target, "launcher":"bin/launch", "probe":"bin/probe", "capabilities":["html"]
    }).to_string()).unwrap();
    std::fs::create_dir(root.join("bin")).unwrap();
    executable(&root.join("bin/launch"), "#!/bin/sh\nexit 0\n");
    executable(&root.join("bin/probe"), probe);
}

#[tokio::test]
async fn runtime_load_probe_and_environment_are_independent_of_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    std::fs::create_dir(&root).unwrap();
    bundle(
        &root,
        "#!/bin/sh\ntest -n \"$GEWU_RUNTIME_ROOT\"\ntest -n \"$GEWU_PLUGIN_ROOT\"\ntest -n \"$GEWU_WORKSPACE_ROOT\"\ntest -n \"$GEWU_RUNTIME_TARGET\"\n",
    );
    let loaded = RuntimeBundle::load(&root, "paper", &target()).unwrap();
    loaded
        .probe(temp.path(), temp.path(), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        loaded
            .environment(temp.path(), temp.path())
            .variables()
            .contains_key("GEWU_RUNTIME_ROOT")
    );
}

#[tokio::test]
async fn missing_incompatible_and_failed_runtime_probes_are_typed() {
    let target = target();
    assert!(matches!(
        select_runtime(&[], &target),
        Err(RuntimeError::Missing)
    ));
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    std::fs::create_dir(&root).unwrap();
    bundle(&root, "#!/bin/sh\necho failed >&2\nexit 2\n");
    let mut incompatible = target.clone();
    incompatible.arch = "aarch64".into();
    assert!(matches!(
        RuntimeBundle::load(&root, "paper", &incompatible),
        Err(RuntimeError::Incompatible(_))
    ));
    let loaded = RuntimeBundle::load(&root, "paper", &target).unwrap();
    assert!(matches!(
        loaded
            .probe(temp.path(), temp.path(), Duration::from_secs(2))
            .await,
        Err(RuntimeError::ProbeFailed(_))
    ));
}

#[test]
fn runtime_selector_requires_exact_target() {
    let target = target();
    let runtime = RuntimeRef {
        id: "paper-runtime".into(),
        version: "0.2.0".into(),
        target: target.clone(),
        artifact: gewu_protocol::Artifact {
            url: "https://example.invalid/runtime.zip".into(),
            sha256: "a".repeat(64),
            bytes: 1,
        },
        manifest: gewu_protocol::Artifact {
            url: "https://example.invalid/runtime.json".into(),
            sha256: "b".repeat(64),
            bytes: 1,
        },
    };
    assert_eq!(
        select_runtime(&[runtime], &target).unwrap().id,
        "paper-runtime"
    );
}
