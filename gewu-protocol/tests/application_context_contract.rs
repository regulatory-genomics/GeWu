//! Contract for the opt-in application-context hook.
//!
//! The host never interprets a provider's snapshot: it forwards the opaque
//! `contextRef` and bounds the size. These tests pin the wire shape so a
//! provider cannot silently widen it, and so a host that does not support the
//! hook still sees the old frames.

use gewu_protocol::plugin::*;
use serde_json::{Value, json};

const CONTEXT_SCHEMA: &str = include_str!("../schema/application-context.v1.schema.json");

fn snapshot() -> Value {
    json!({
        "schema": "gewu.application-context/1",
        "contextRef": {
            "uri": "tachyon://context/doc-a",
            "type": "tachyon.context",
            "revision": "c12-v9"
        },
        "summary": {"datasets": [], "canvas": {"revision": 9}},
        "coverage": {"complete": false, "total": 120, "omitted": {"catalogItems": 120}},
        "readMore": [{"tool": "catalog.list", "args": {"limit": 20}}]
    })
}

#[test]
fn a_context_snapshot_matches_its_published_schema() {
    let schema: Value = serde_json::from_str(CONTEXT_SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&snapshot()));

    let mut wrong = snapshot();
    wrong["schema"] = json!("gewu.application-context/2");
    assert!(!validator.is_valid(&wrong));
}

#[test]
fn a_manifest_may_opt_into_a_context_provider() {
    let manifest: Manifest = serde_json::from_value(json!({
        "apiVersion": "gewu.plugin/v1",
        "id": "tachyon",
        "version": "0.2.0",
        "title": "Tachyon",
        "scope": "session",
        "backend": {"connector": "gewu-stdio", "config": {"entry": "bin/tachyon-gewu"}},
        "contextProvider": {"tool": "context.get"}
    }))
    .unwrap();
    assert_eq!(
        manifest.context_provider.as_ref().map(|p| p.tool.as_str()),
        Some("context.get")
    );

    // Omitted means "no hook": old packages keep their exact manifest shape.
    let legacy: Manifest = serde_json::from_value(json!({
        "apiVersion": "gewu.plugin/v1",
        "id": "demo",
        "version": "0.1.0",
        "title": "Demo",
        "scope": "workspace",
        "backend": {"connector": "gewu-stdio", "config": {"entry": "bin/demo"}}
    }))
    .unwrap();
    assert!(legacy.context_provider.is_none());
}

#[test]
fn a_call_context_carries_the_snapshot_it_was_derived_from() {
    let context: CallContext = serde_json::from_value(json!({
        "instanceId": "i",
        "generation": 2,
        "workspaceId": "w",
        "callId": "c",
        "sessionId": "s",
        "contextRef": {
            "uri": "tachyon://context/doc-a",
            "type": "tachyon.context",
            "revision": "c12-v9"
        }
    }))
    .unwrap();
    assert_eq!(
        context.context_ref.as_ref().and_then(|r| r.revision.as_deref()),
        Some("c12-v9")
    );

    // A provider that does not opt in receives no new field.
    let plain = serde_json::to_value(CallContext {
        context_ref: None,
        ..context
    })
    .unwrap();
    assert!(plain.get("contextRef").is_none());
}
