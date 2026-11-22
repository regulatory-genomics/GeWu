use gewu_protocol::plugin::*;

#[test]
fn marketplace_install_attempt_is_optional_and_reports_are_readable() {
    let install =
        serde_json::json!({"action":"marketplace_install","pluginId":"demo","channel":"stable"});
    assert!(serde_json::from_value::<HostAction>(install).is_ok());
    let report = serde_json::json!({"action":"marketplace_install_report","attemptId":"774872c7-4a4c-4702-a70b-c39bf3e0249a"});
    assert!(serde_json::from_value::<HostAction>(report).is_ok());
}
use serde_json::{Value, json};

#[test]
fn wire_fixtures_roundtrip_and_validate() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    for (name, contents) in [
        (
            "activate",
            include_str!("fixtures/plugin-v1/activate.jsonl"),
        ),
        ("call", include_str!("fixtures/plugin-v1/call.jsonl")),
        (
            "progress",
            include_str!("fixtures/plugin-v1/progress.jsonl"),
        ),
        ("cancel", include_str!("fixtures/plugin-v1/cancel.jsonl")),
        ("resolve", include_str!("fixtures/plugin-v1/resolve.jsonl")),
        (
            "deactivate",
            include_str!("fixtures/plugin-v1/deactivate.jsonl"),
        ),
    ] {
        let mut pending = Pending::default();
        for line in contents.lines() {
            let wire: Value = serde_json::from_str(line).unwrap();
            let definition = if wire.get("op").is_some() || wire["type"] == "process.accepted" {
                "HostFrame"
            } else {
                "AdapterFrame"
            };
            let mut schema = schema.clone();
            schema["$ref"] = json!(format!("#/$defs/{definition}"));
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert!(validator.is_valid(&wire), "{name}: {wire}");
            let roundtrip = if definition == "HostFrame" {
                let host: HostFrame = decode(line.as_bytes()).unwrap();
                if let HostFrame::Request(request) = &host {
                    pending.start(request).unwrap();
                }
                serde_json::to_value(host).unwrap()
            } else {
                let response = decode::<AdapterFrame>(line.as_bytes()).unwrap();
                pending.receive(&response).unwrap();
                serde_json::to_value(response).unwrap()
            };
            assert_eq!(roundtrip, wire);
        }
        assert!(pending.is_empty(), "fixture left request pending: {name}");
    }
}

#[test]
fn rejects_bad_frames_and_mismatched_or_repeated_terminals() {
    for line in include_str!("fixtures/plugin-v1/invalid.jsonl").lines() {
        assert!(decode::<HostFrame>(line.as_bytes()).is_err(), "{line}");
    }
    assert!(decode::<HostFrame>(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
    let request: Request =
        serde_json::from_value(json!({"id":"r","op":"cancel","params":{"callId":"c"}})).unwrap();
    let mut pending = Pending::default();
    pending.start(&request).unwrap();
    assert!(pending.start(&request).is_err());
    let wrong = serde_json::from_value(json!({"id":"r","type":"result","result":{"stopped":true}}))
        .unwrap();
    assert!(pending.receive(&wrong).is_err());
    let right =
        serde_json::from_value(json!({"id":"r","type":"result","result":{"state":"requested"}}))
            .unwrap();
    pending.receive(&right).unwrap();
    assert!(pending.receive(&right).is_err());
}

#[test]
fn marketplace_install_defaults_to_auto_for_older_clients() {
    let action: HostAction = serde_json::from_value(json!({
        "action": "marketplace_install",
        "pluginId": "paper",
        "version": "0.2.0",
        "channel": "stable"
    }))
    .unwrap();
    assert!(matches!(
        action,
        HostAction::MarketplaceInstall { runtime, .. } if runtime == "auto"
    ));
}
