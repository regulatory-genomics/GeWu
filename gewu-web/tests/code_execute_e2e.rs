use gewu_core::traits::Tool;
use gewu_core::types::ToolContext;
use gewu_web::workspace::{WorkflowEventKind, WorkspaceRuntime, code_execute::CodeExecuteTool};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn code_execute_produces_plotly_canvas_node() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        WorkspaceRuntime::open("default", dir.path().to_path_buf(), dir.path().join("store"))
            .unwrap(),
    );
    let tool = CodeExecuteTool::new(runtime.clone());
    let ctx = ToolContext::new("e2e_session", "msg-1");

    let code = r#"
result = {
    'kind': 'plotly',
    'data': [{'x': [1, 2], 'y': [3, 4], 'type': 'scatter'}],
    'layout': {'title': 'Test'}
}
"#;
    let result = tool.execute(json!({"code": code}), &ctx).await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["nodes_created"], 1);

    let graph = runtime.graph_store.load("e2e_session").unwrap();
    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].node_type, "plotly");
}

#[tokio::test]
async fn code_execute_multi_output_creates_multiple_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        WorkspaceRuntime::open("default", dir.path().to_path_buf(), dir.path().join("store"))
            .unwrap(),
    );
    let tool = CodeExecuteTool::new(runtime.clone());
    let ctx = ToolContext::new("e2e_session_2", "msg-1");

    let code = r#"
result = [
    {'kind': 'data_table', 'title': 'Gene Scores', 'columns': [{'field': 'gene', 'header': 'Gene'}, {'field': 'score', 'header': 'Score'}], 'rows': [{'gene': 'TP53', 'score': 0.95}]},
    {'kind': 'markdown', 'content': '## Summary\n2 genes analyzed'},
]
"#;
    let result = tool.execute(json!({"code": code}), &ctx).await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(body["nodes_created"], 2);

    let graph = runtime.graph_store.load("e2e_session_2").unwrap();
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.nodes.iter().filter(|node| node.node_type == "data_table").count(), 1);
    assert_eq!(graph.nodes.iter().filter(|node| node.node_type == "markdown").count(), 1);
}

#[tokio::test]
async fn code_execute_emits_workflow_events_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        WorkspaceRuntime::open("default", dir.path().to_path_buf(), dir.path().join("store"))
            .unwrap(),
    );
    let tool = CodeExecuteTool::new(runtime.clone());
    let ctx = ToolContext::new("e2e_session_3", "msg-1");

    let result = tool
        .execute(
            json!({"code": "result = {'kind': 'markdown', 'content': '# Hello'}"}),
            &ctx,
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(body["ok"], true);

    let store = runtime.workflow_store.lock().await;
    let events = store.all();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, WorkflowEventKind::ArtifactProduced);
    assert_eq!(events[1].kind, WorkflowEventKind::ArtifactSurfaced);
}
