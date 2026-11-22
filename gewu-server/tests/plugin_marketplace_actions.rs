#![cfg(unix)]
//! The GUI reaches installation through host actions, not a second installer.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gewu_protocol::plugin::HostAction;
use gewu_server::plugins::{PluginHost, stdio::Limits};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::collections::BTreeMap;

#[tokio::test]
async fn marketplace_search_reads_the_signed_offline_cache() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join(".gewu/marketplace/official");
    std::fs::create_dir_all(&cache).unwrap();
    let keypair = Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).unwrap();
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion":"gewu.marketplace/v1","repository":"official",
        "generatedAt":"2026-11-19T00:00:00Z","expiresAt":"2027-11-19T00:00:00Z","keyId":"official-v1",
        "releases":[{
            "schemaVersion":"gewu.release/v1","releaseId":"demo-1.0.0","plugin":"demo","version":"1.0.0",
            "channel":"beta","gewu":{"api":"gewu.plugin/v1","host":">=99.0.0"},
            "package":{"url":"https://example.invalid/package.zip","sha256":"a".repeat(64),"bytes":1},
            "runtimes":[{"id":"demo-runtime","version":"1.0.0","target":{"os":"linux","arch":"x86_64","libc":"glibc"},
                "artifact":{"url":"https://example.invalid/runtime.zip","sha256":"c".repeat(64),"bytes":1},
                "manifest":{"url":"https://example.invalid/runtime.json","sha256":"d".repeat(64),"bytes":1}}],
            "capabilities":[],"permissions":[],
            "provenance":{"builder":"test","source":"https://github.com/example/demo","sourceCommit":"b".repeat(40),"buildInputs":[],"createdAt":"2026-11-19T00:00:00Z"}
        }]
    })).unwrap();
    std::fs::write(cache.join("index.v1.json"), &index).unwrap();
    std::fs::write(
        cache.join("index.v1.json.sig"),
        BASE64.encode(keypair.sign(&index).as_ref()),
    )
    .unwrap();
    // SAFETY: this binary has one test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var(
            "GEWU_MARKETPLACE_PUBLIC_KEY",
            BASE64.encode(keypair.public_key().as_ref()),
        );
    }
    let host = PluginHost::from_config(&BTreeMap::new(), None, Limits::default()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let value = host
        .action(
            "workspace",
            workspace.path(),
            "owner",
            HostAction::MarketplaceSearch {
                query: None,
                refresh: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(value["repository"], "official");
    assert_eq!(value["releases"][0]["installCompatible"], false);
    assert_eq!(
        value["releases"][0]["installBlocker"]["code"],
        "host_version"
    );
    assert!(
        value["releases"][0]["installBlocker"]["message"]
            .as_str()
            .unwrap()
            .contains(">=99.0.0")
    );
    assert!(value["installed"].is_object());
}

#[tokio::test]
async fn bundled_marketplace_cannot_be_disabled() {
    let host = PluginHost::from_config(&BTreeMap::new(), None, Limits::default()).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let error = host
        .action(
            "workspace",
            workspace.path(),
            "owner",
            HostAction::SetEnabled {
                plugin_id: "gewu-marketplace".into(),
                enabled: false,
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot be disabled"), "{error}");
}
