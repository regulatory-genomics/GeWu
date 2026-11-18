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
    assert!(String::from_utf8_lossy(&run(&["list"]).stdout).contains("demo\t1.0.0"));
    assert!(run(&["remove", "demo"]).status.success());
    assert!(!run(&["check", "demo"]).status.success());
    assert!(!marker.exists());
    assert_eq!(
        fs::read_to_string(workspace.join("document.txt")).unwrap(),
        "keep"
    );
}
