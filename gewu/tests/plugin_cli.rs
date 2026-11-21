#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn install_check_remove_are_static_and_preserve_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("package");
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let marker = workspace.join("executed");
    fs::create_dir(&package).unwrap();
    fs::create_dir(&home).unwrap();
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("document.txt"), "keep").unwrap();
    fs::write(package.join("adapter"), "#!/bin/sh\ntouch executed\n").unwrap();
    fs::set_permissions(package.join("adapter"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(package.join("plugin.json"), r#"{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"1.0.0","scope":"workspace","backend":{"connector":"gewu-stdio","config":{"entry":"adapter"}}}"#).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_gewu"))
            .args(["--no-update-check", "plugin"])
            .args(args)
            .env("HOME", &home)
            .current_dir(&workspace)
            .output()
            .unwrap()
    };
    let install = run(&["install", package.to_str().unwrap()]);
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(
        !run(&["install", package.to_str().unwrap()])
            .status
            .success()
    );
    assert!(run(&["check", "demo"]).status.success());
    assert!(
        run(&["check", "demo", "--version", "1.0.0"])
            .status
            .success()
    );
    assert!(String::from_utf8_lossy(&run(&["list"]).stdout).contains("demo\t1.0.0"));
    assert!(run(&["remove", "demo"]).status.success());
    assert!(!run(&["check", "demo"]).status.success());
    assert!(!marker.exists());
    assert_eq!(
        fs::read_to_string(workspace.join("document.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn failed_install_has_a_local_report_but_dry_run_does_not_write_one() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_gewu"))
            .args(["--no-update-check", "plugin"])
            .args(args)
            .env("HOME", home.path())
            .output()
            .unwrap()
    };
    let attempts = home.path().join(".gewu/install-attempts");
    assert!(
        !run(&["marketplace-install", "demo", "--offline", "--dry-run"])
            .status
            .success()
    );
    assert!(!attempts.exists());
    let result = run(&["marketplace-install", "demo", "--offline"]);
    assert!(!result.status.success());
    let entry = fs::read_dir(attempts).unwrap().next().unwrap().unwrap();
    let id = entry.file_name().into_string().unwrap();
    assert!(String::from_utf8_lossy(&result.stderr).contains(&id));
    let result = run(&["install-report", &id, "--json"]);
    assert!(result.status.success());
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["attemptId"], id);
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["stage"], "catalog");
}

#[test]
fn lifecycle_keeps_previous_generation_and_workspace_data() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    for (root, version) in [(&first, "1.0.0"), (&second, "2.0.0")] {
        std::fs::create_dir(root).unwrap();
        std::fs::write(root.join("adapter"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            root.join("adapter"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(root.join("plugin.json"), format!(r#"{{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"{version}","scope":"workspace","backend":{{"connector":"gewu-stdio","config":{{"entry":"adapter"}}}}}}"#)).unwrap();
    }
    std::fs::write(workspace.join("keep.txt"), "keep").unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_gewu"))
            .args(["--no-update-check", "plugin"])
            .args(args)
            .env("HOME", &home)
            .current_dir(&workspace)
            .output()
            .unwrap()
    };
    assert!(run(&["install", first.to_str().unwrap()]).status.success());
    assert!(run(&["install", second.to_str().unwrap()]).status.success());
    assert!(run(&["disable", "demo"]).status.success());
    assert!(run(&["enable", "demo"]).status.success());
    let rollback = run(&["rollback", "demo"]);
    assert!(
        rollback.status.success(),
        "{}",
        String::from_utf8_lossy(&rollback.stderr)
    );
    let doctor = run(&["doctor", "demo"]);
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("generations\":2"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("keep.txt")).unwrap(),
        "keep"
    );
    assert!(run(&["uninstall", "demo"]).status.success());
    assert!(!home.join(".gewu/plugins/demo").exists());
}

#[test]
fn uninstall_cleans_legacy_install_config_entry_and_override() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    // Legacy layout: a version directory with no state.json entry, the shape a
    // config `package` path produces.
    let legacy = home.join(".gewu/plugins/demo/1.0.0");
    fs::create_dir_all(&legacy).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(legacy.join("adapter"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(legacy.join("adapter"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        legacy.join("plugin.json"),
        r#"{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"1.0.0","scope":"workspace","backend":{"connector":"gewu-stdio","config":{"entry":"adapter"}}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".gewu/config.json"),
        r#"{"plugins":{"demo":{"package":"/tmp/demo","enabled":true},"other":{"package":"/tmp/other","enabled":true}}}"#,
    )
    .unwrap();
    fs::write(home.join(".gewu/plugin-state.json"), r#"{"demo":false}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_gewu"))
        .args(["--no-update-check", "plugin", "uninstall", "demo"])
        .env("HOME", &home)
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !legacy.exists(),
        "the legacy version directory must be removed"
    );

    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".gewu/config.json")).unwrap()).unwrap();
    assert!(
        config["plugins"].get("demo").is_none(),
        "config entry must be dropped: {config}"
    );
    assert!(
        config["plugins"].get("other").is_some(),
        "unrelated entries stay: {config}"
    );

    let overrides: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".gewu/plugin-state.json")).unwrap())
            .unwrap();
    assert!(
        overrides.get("demo").is_none(),
        "stale override must be dropped: {overrides}"
    );
}

#[test]
fn uninstall_preserves_jsonc_comments_and_workspace_files() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let legacy = home.join(".gewu/plugins/demo/1.0.0");
    fs::create_dir_all(&legacy).unwrap();
    fs::create_dir(&workspace).unwrap();
    fs::write(legacy.join("adapter"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(legacy.join("adapter"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(legacy.join("plugin.json"), r#"{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"1.0.0","scope":"workspace","backend":{"connector":"gewu-stdio","config":{"entry":"adapter"}}}"#).unwrap();
    let config = "// keep this explanation\n{\"plugins\":{\"demo\":{\"enabled\":true}}}\n";
    fs::write(home.join(".gewu/config.json"), config).unwrap();
    fs::write(workspace.join("document.txt"), "user document").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gewu"))
        .args(["--no-update-check", "plugin", "uninstall", "demo"])
        .env("HOME", &home)
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!legacy.exists());
    assert_eq!(
        fs::read_to_string(home.join(".gewu/config.json")).unwrap(),
        config
    );
    assert_eq!(
        fs::read_to_string(workspace.join("document.txt")).unwrap(),
        "user document"
    );
}

#[test]
fn deep_links_reject_repository_and_runtime_overrides_before_installing() {
    let home = tempfile::tempdir().unwrap();
    for link in [
        "gewu://plugin/install?repo=other&id=demo",
        "gewu://plugin/install?repo=official&id=demo&executable=/usr/bin/python3",
        "file:///tmp/plugin.zip",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_gewu"))
            .args(["--no-update-check", "plugin", "open-link", link])
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {link}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("official repository")
                || error.contains("unsupported deep-link parameter")
                || error.contains("expected gewu://plugin/install"),
            "{error}"
        );
    }
    assert!(!home.path().join(".gewu/plugins/state.json").exists());
}
