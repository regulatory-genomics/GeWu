use gewu_core::traits::Tool;
use gewu_core::types::ToolContext;
use gewu_web::workspace::code_execute::{CodeExecuteTool, PythonSandbox};
use gewu_web::workspace::{WorkflowEventKind, WorkspaceRuntime};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn python_sandbox_executes_simple_result() {
    let sandbox = PythonSandbox::new(30);
    let result = sandbox
        .execute("result = {'kind': 'markdown', 'content': 'hello'}")
        .await
        .unwrap();

    assert!(result.error.is_none());
    let value = result.result.unwrap();
    assert_eq!(value["kind"], "markdown");
    assert_eq!(value["content"], "hello");
}

#[tokio::test]
async fn python_sandbox_executes_list_result() {
    let sandbox = PythonSandbox::new(30);
    let code = r##"
result = [
    {"kind": "markdown", "content": "# Title"},
    {"kind": "data_table", "columns": [{"field": "x", "header": "X"}], "rows": [{"x": 1}]},
]
"##;
    let result = sandbox.execute(code).await.unwrap();

    assert!(result.error.is_none());
    let value = result.result.unwrap();
    assert!(value.is_array());
    assert_eq!(value.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn python_sandbox_reports_syntax_error() {
    let sandbox = PythonSandbox::new(30);
    let result = sandbox.execute("def foo(").await.unwrap();

    assert!(result.error.is_some());
}

#[tokio::test]
async fn python_sandbox_treats_missing_result_as_null() {
    let sandbox = PythonSandbox::new(30);
    let result = sandbox.execute("x = 42").await.unwrap();

    assert!(result.result.is_none() || result.result == Some(serde_json::Value::Null));
}

#[tokio::test]
async fn python_sandbox_times_out() {
    let sandbox = PythonSandbox::new(2);
    let result = sandbox
        .execute("import time; time.sleep(10); result = 'done'")
        .await
        .unwrap();

    assert!(result.timed_out);
}

#[tokio::test]
async fn code_execute_tool_emits_canvas_events() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        WorkspaceRuntime::open("default", dir.path().to_path_buf(), dir.path().join("store"))
            .unwrap(),
    );
    let tool = CodeExecuteTool::new(runtime.clone());
    let ctx = ToolContext::new("test_session", "msg-1");

    let result = tool
        .execute(
            json!({"code": "result = {'kind': 'markdown', 'content': '# Summary'}"}),
            &ctx,
        )
        .await
        .unwrap();

    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["nodes_created"], 1);

    let store = runtime.workflow_store.lock().await;
    let events = store.all();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, WorkflowEventKind::ArtifactProduced);
    assert_eq!(events[1].kind, WorkflowEventKind::ArtifactSurfaced);
}

#[tokio::test]
async fn code_execute_tool_handles_object_columns_and_record_rows() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        WorkspaceRuntime::open("default", dir.path().to_path_buf(), dir.path().join("store"))
            .unwrap(),
    );
    let tool = CodeExecuteTool::new(runtime.clone());
    let ctx = ToolContext::new("test_session", "msg-2");

    let result = tool
        .execute(
            json!({
                "code": r#"
result = {
    "kind": "data_table",
    "columns": [{"field": "gene", "header": "Gene"}, {"field": "score", "header": "Score"}],
    "rows": [{"gene": "TP53", "score": 0.95}],
}
"#
            }),
            &ctx,
        )
        .await
        .unwrap();

    let body: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["nodes_created"], 1);

    let store = runtime.workflow_store.lock().await;
    let events = store.all();
    let preview = &events[0].payload["preview"];
    assert_eq!(preview["kind"], "table");
    assert_eq!(preview["columns"], json!(["Gene", "Score"]));
    assert_eq!(preview["rows"], json!([["TP53", 0.95]]));
}

#[test]
fn code_execute_tool_name_is_stable_for_llm_selection() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        WorkspaceRuntime::open("default", dir.path().to_path_buf(), dir.path().join("store"))
            .unwrap(),
    );
    let tool = CodeExecuteTool::new(runtime);

    assert_eq!(tool.name(), "code_execute");
    assert!(tool.description().contains("Set a JSON-serializable `result` variable"));
    assert!(tool.description().contains("plotly"));
}
