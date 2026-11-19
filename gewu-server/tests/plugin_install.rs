#![cfg(unix)]

use gewu_protocol::{Artifact, GeWuCompatibility, Provenance, Release, RuntimeRef};
use gewu_server::plugins::{artifacts::sha256_file, lifecycle::InstallManager, runtime::current_target};
use std::{io::Write, path::Path, time::Duration};

fn zip_file(path: &Path, files: &[(&str, &[u8], u32)]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for (name, bytes, mode) in files {
        let mut options = zip::write::SimpleFileOptions::default();
        options = options.unix_permissions(*mode);
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

fn artifact(path: &Path) -> Artifact {
    Artifact { url: format!("https://example.invalid/{}", path.file_name().unwrap().to_string_lossy()), sha256: sha256_file(path).unwrap(), bytes: std::fs::metadata(path).unwrap().len() }
}

#[tokio::test]
async fn install_verifies_extracts_probes_and_commits_only_after_success() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("demo.package.zip");
    let runtime = temp.path().join("demo.runtime.zip");
    let manifest = br#"{"apiVersion":"gewu.plugin/v1","id":"demo","title":"Demo","version":"1.0.0","scope":"workspace","backend":{"connector":"gewu-stdio","config":{"entry":"adapter"}}}"#;
    let adapter = b"#!/bin/sh\nexit 0\n";
    zip_file(&package, &[("plugin.json", manifest, 0o644), ("adapter", adapter, 0o755)]);
    let target = current_target().unwrap();
    let runtime_manifest = serde_json::json!({"schemaVersion":"gewu.runtime/v1","id":"demo-runtime","version":"1.0.0","plugin":"demo","target":target,"launcher":"bin/launch","probe":"bin/probe","capabilities":[]}).to_string();
    zip_file(&runtime, &[("runtime.json", runtime_manifest.as_bytes(), 0o644), ("bin/launch", b"#!/bin/sh\nexit 0\n", 0o755), ("bin/probe", b"#!/bin/sh\ntest -n \"$GEWU_RUNTIME_ROOT\"\ntest -n \"$GEWU_PLUGIN_ROOT\"\n", 0o755)]);
    let release = Release {
        schema_version: "gewu.release/v1".into(), release_id: "demo-1.0.0".into(), plugin: "demo".into(), version: "1.0.0".into(), channel: "stable".into(),
        gewu: GeWuCompatibility { api: "gewu.plugin/v1".into(), host: ">=0.1.0".into() }, package: artifact(&package), runtimes: vec![RuntimeRef {
            id: "demo-runtime".into(), version: "1.0.0".into(), target: target.clone(), artifact: artifact(&runtime), manifest: Artifact { url: "https://example.invalid/runtime.json".into(), sha256: gewu_server::plugins::artifacts::sha256_bytes(runtime_manifest.as_bytes()), bytes: runtime_manifest.len() as u64 },
        }], capabilities: vec![], permissions: vec![], provenance: Provenance { builder: "test".into(), source: "fixture".into(), source_commit: "a".repeat(40), build_inputs: vec![], sbom: None, created_at: "2026-11-19T00:00:00Z".into() }, release_notes: None, revocation: None,
    };
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let manager = InstallManager::new(temp.path().join(".gewu"));
    let generation = manager.install_archives(&release, &package, &runtime, &target, &workspace, Duration::from_secs(2)).await.unwrap();
    assert_eq!(generation.release_id, "demo-1.0.0");
    assert_eq!(manager.state.load().unwrap().plugins["demo"].active_generation.as_deref(), Some("demo-1.0.0"));
}
