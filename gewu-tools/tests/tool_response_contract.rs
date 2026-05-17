use gewu_tools::response::{tool_error, tool_success};
use serde_json::json;

#[test]
fn tool_success_uses_structured_ok_envelope() {
    let result = tool_success(
        "example",
        json!({ "value": 42 }),
        vec!["warning one".to_string()],
        vec!["next action".to_string()],
    );
    let value: serde_json::Value = serde_json::from_str(&result.output).expect("success output is JSON");

    assert_eq!(result.title, "example");
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["value"], 42);
    assert_eq!(value["warnings"][0], "warning one");
    assert_eq!(value["next_actions"][0], "next action");
}

#[test]
fn tool_error_uses_structured_error_envelope() {
    let result = tool_error(
        "example",
        "not_found",
        false,
        "resource missing",
        Some("choose another id".to_string()),
    );
    let value: serde_json::Value = serde_json::from_str(&result.output).expect("error output is JSON");

    assert_eq!(result.title, "example");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "not_found");
    assert_eq!(value["error"]["retryable"], false);
    assert_eq!(value["error"]["message"], "resource missing");
    assert_eq!(value["error"]["suggested_fix"], "choose another id");
}

#[test]
fn tool_error_omits_null_suggested_fix() {
    let result = tool_error("example", "bad_input", true, "try again", None);
    let value: serde_json::Value = serde_json::from_str(&result.output).expect("error output is JSON");

    assert_eq!(value["ok"], false);
    assert!(value["error"].get("suggested_fix").is_none());
}
