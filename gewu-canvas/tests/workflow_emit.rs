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
