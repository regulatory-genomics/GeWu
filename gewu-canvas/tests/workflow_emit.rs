use gewu_canvas::workflow_emit::WorkflowEmitTool;
use gewu_core::traits::Tool;

#[test]
fn workflow_emit_exposes_schema_for_workflow_events() {
    let tool = WorkflowEmitTool::new("http://localhost:3210".to_string());

    assert_eq!(tool.name(), "workflow.emit");

    let schema = tool.schema();
    let schema_str = serde_json::to_string(&schema).unwrap();
    assert!(schema_str.contains("kind"));
    assert!(schema_str.contains("payload"));
}

#[tokio::test]
async fn workflow_emit_normalizes_common_llm_payload_aliases() {
    use gewu_core::types::ToolContext;
    use mockito::Matcher;
    use serde_json::json;

    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/workflow/events")
        .match_body(Matcher::PartialJson(json!({
            "kind": "edge_declared",
            "actor": "agent",
            "session_id": "alias-test",
            "payload": {
                "from_id": "ucf_SM30_1",
                "to_id": "qc_table_SM30",
                "relationship": "input_to"
            }
        })))
        .with_status(200)
        .with_body(r#"{"ok":true}"#)
        .create_async()
        .await;

    let tool = WorkflowEmitTool::new(server.url());
    let result = tool
        .execute(
            json!({
                "kind": "edge_declared",
                "session_id": "alias-test",
                "payload": {
                    "from": "ucf_SM30_1",
                    "to": "qc_table_SM30",
                    "relationship": "input_to"
                }
            }),
            &ToolContext::new("alias-test", "msg-1"),
        )
        .await
        .unwrap();

    assert!(result.output.contains("\"ok\":true"), "{}", result.output);
    mock.assert_async().await;
}
