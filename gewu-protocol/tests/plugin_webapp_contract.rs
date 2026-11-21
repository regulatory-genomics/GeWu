use gewu_protocol::plugin::{Manifest, SCHEMA};
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({"apiVersion":"gewu.plugin/v1","id":"video","version":"0.1.0","title":"Video",
        "scope":"resource","backend":{"connector":"gewu-stdio","config":{"entry":"bin/adapter"}},
        "views":[{"id":"studio","title":"Studio","kind":"webapp","placement":"main",
            "resourceTypes":["hyperframes.project"],"serviceId":"studio","entryPath":"/",
            "stateSchema":{"type":"object"}}]})
}

#[test]
fn webapp_roundtrips_and_requires_a_contained_service_entry() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let valid = manifest();
    assert!(validator.is_valid(&valid), "webapp must validate");
    let decoded: Manifest = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), valid);
    for entry in [
        "https://example.com/",
        "//example.com/",
        "/../outside",
        "/a#b",
        "/a?b",
        "/a\\b",
    ] {
        let mut invalid = manifest();
        invalid["views"][0]["entryPath"] = json!(entry);
        assert!(!validator.is_valid(&invalid), "accepted {entry}");
    }
    let mut missing = manifest();
    missing["views"][0]
        .as_object_mut()
        .unwrap()
        .remove("serviceId");
    assert!(!validator.is_valid(&missing));
}
