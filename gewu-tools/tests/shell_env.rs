//! Unit tests for the disposable shell runner.
//!
//! Most tests use a fake `singularity` that strips the options and executes the
//! remaining command on the host. That is enough to prove output bounds, exit
//! codes, timeout and cancellation without an image. The ignored integration
//! test exercises the real image and its isolation.

use std::path::PathBuf;
use std::time::Duration;

use gewu_core::CancelHandle;
use gewu_tools::shell_env::{ShellEnvironment, ShellMount, execute};

fn temp_root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "gewu-shell-env-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    for child in ["workspace", "outputs", "inputs", "tmp"] {
        std::fs::create_dir_all(path.join(child)).unwrap();
    }
    std::fs::write(path.join("fake-image.sif"), b"not-an-image").unwrap();
    path
}

fn fake_environment(root: &PathBuf) -> ShellEnvironment {
    let fake = root.join("fake-singularity");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    exec) shift ;;
    --bind|--network|--no-mount|--pwd|--env) shift 2 ;;
    --*) shift ;;
    *) break ;;
  esac
done
shift
exec "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut environment = ShellEnvironment::new(
        root.join("fake-image.sif"),
        root.join("workspace"),
        root.join("outputs"),
        root.join("inputs"),
        root.join("tmp"),
    );
    environment.singularity = fake.display().to_string();
    environment
}

#[tokio::test]
async fn captures_stdout_stderr_and_exit_code() {
    let root = temp_root("capture");
    let environment = fake_environment(&root);
    let outcome = execute(
        &environment,
        "echo hello; echo bad >&2; exit 3",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome.exit_code, Some(3));
    assert!(outcome.stdout.contains("hello"));
    assert!(outcome.stderr.contains("bad"));
    assert!(!outcome.timed_out);
    assert!(!outcome.succeeded());
}

#[tokio::test]
async fn output_is_bounded_but_still_drained() {
    let root = temp_root("bounded");
    let environment = fake_environment(&root).with_limit(Duration::from_secs(30), 1024);
    // The process must not block writing more than the kept bytes.
    let outcome = execute(
        &environment,
        "python3 -c 'print(\"x\" * 200000)'",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.stdout.len(), 1024);
    assert!(outcome.stdout_bytes > 200_000);
    assert!(outcome.output_truncated);
}

#[tokio::test]
async fn a_hanging_command_times_out_and_is_killed() {
    let root = temp_root("timeout");
    let environment = fake_environment(&root).with_limit(Duration::from_millis(700), 4096);
    let outcome = execute(&environment, "sleep 60", "/workspace", None)
        .await
        .unwrap();
    assert!(outcome.timed_out);
    assert!(!outcome.succeeded());
}

#[tokio::test]
async fn cancellation_stops_the_command() {
    let root = temp_root("cancel");
    let environment = fake_environment(&root).with_limit(Duration::from_secs(30), 4096);
    let cancel = CancelHandle::new();
    let handle = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel();
    });
    let outcome = execute(&environment, "sleep 60", "/workspace", Some(&cancel))
        .await
        .unwrap();
    assert!(outcome.cancelled);
    assert!(!outcome.succeeded());
}

#[tokio::test]
async fn background_children_do_not_keep_the_result_open() {
    let root = temp_root("background");
    let environment = fake_environment(&root).with_limit(Duration::from_secs(10), 4096);
    let outcome = execute(
        &environment,
        "setsid sh -c 'sleep 30' >/dev/null 2>&1 & echo done",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert!(outcome.stdout.contains("done"));
    assert!(!outcome.timed_out, "the foreground command ended");
}

#[tokio::test]
async fn workdir_outside_the_allowed_roots_is_refused_before_spawn() {
    let root = temp_root("workdir");
    let environment = fake_environment(&root);
    let error = execute(&environment, "echo hi", "/etc", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("workdir"));
}

#[tokio::test]
async fn a_missing_image_is_a_configuration_error() {
    let root = temp_root("missing-image");
    let mut environment = fake_environment(&root);
    environment.image = root.join("does-not-exist.sif");
    let error = execute(&environment, "echo hi", "/workspace", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("does not exist"));
}

#[tokio::test]
async fn extra_mounts_are_forwarded() {
    let root = temp_root("mount");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let environment = fake_environment(&root).with_mount(ShellMount {
        host: project,
        container: "/project".into(),
        read_only: true,
    });
    let outcome = execute(&environment, "echo mounted", "/project", None)
        .await
        .unwrap();
    assert!(outcome.stdout.contains("mounted"));
}

// ---------------------------------------------------------------------------
// Real image integration
// ---------------------------------------------------------------------------

fn real_image() -> Option<PathBuf> {
    std::env::var("TACHYON_ANALYSIS_IMAGE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

#[tokio::test]
#[ignore = "requires TACHYON_ANALYSIS_IMAGE and SingularityCE"]
async fn real_image_isolates_filesystem_and_network_and_reuses_the_workspace() {
    let Some(image) = real_image() else {
        panic!("set TACHYON_ANALYSIS_IMAGE to run the real-image test");
    };
    let root = temp_root("real");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("data.tsv"), "original\n").unwrap();
    std::fs::write(root.join("sentinel.txt"), "host-only\n").unwrap();
    let environment = ShellEnvironment::new(
        image,
        root.join("workspace"),
        root.join("outputs"),
        root.join("inputs"),
        root.join("tmp"),
    )
    .with_limit(Duration::from_secs(60), 64 * 1024)
    .with_mount(ShellMount {
        host: project.clone(),
        container: "/project".into(),
        read_only: true,
    });

    // 1. A failed call keeps the workspace for the next call.
    let first = execute(
        &environment,
        "printf 'half' > /workspace/state; exit 7",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert_eq!(first.exit_code, Some(7));
    let second = execute(
        &environment,
        "cat /workspace/state; printf ' done' >> /workspace/state",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert_eq!(second.stdout.trim(), "half");
    let third = execute(&environment, "cat /workspace/state", "/workspace", None)
        .await
        .unwrap();
    assert_eq!(third.stdout.trim(), "half done");

    // 2. Project is readable, not writable.
    let read = execute(&environment, "cat /project/data.tsv", "/workspace", None)
        .await
        .unwrap();
    assert!(read.succeeded());
    assert!(read.stdout.contains("original"));
    let write = execute(&environment, "printf x > /project/nope", "/workspace", None)
        .await
        .unwrap();
    assert!(!write.succeeded());

    // 3. The host sentinel is not reachable.
    let sentinel = execute(
        &environment,
        &format!("cat {}", root.join("sentinel.txt").display()),
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert!(
        !sentinel.succeeded(),
        "host path leaked: {}",
        sentinel.stdout
    );

    // 4. Network is off.
    let network = execute(
        &environment,
        "python -c 'import socket; socket.create_connection((\"1.1.1.1\", 80), 2)'",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert!(!network.succeeded(), "network reached: {}", network.stderr);

    // 5. A timed-out command does not leave its child writing.
    let timeout_environment = environment
        .clone()
        .with_limit(Duration::from_millis(800), 4096);
    let timed = execute(
        &timeout_environment,
        "setsid sh -c 'sleep 3; printf leaked > /workspace/leaked' & sleep 60",
        "/workspace",
        None,
    )
    .await
    .unwrap();
    assert!(timed.timed_out);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !root.join("workspace/leaked").exists(),
        "a killed command left a writer behind"
    );

    let _ = std::fs::remove_dir_all(&root);
}
