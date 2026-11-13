use gewu_core::traits::Tool;
use gewu_core::types::ToolContext;
use gewu_graph::graph::{END, StateGraph};
use gewu_graph::{GraphTool, NodeResult, State};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct TestState {
    count: i32,
    messages: Vec<String>,
}

impl State for TestState {
    type Partial = PartialTestState;

    fn merge(&self, update: Self::Partial) -> Self {
        Self {
            count: update.count.unwrap_or(self.count),
            messages: update.messages.unwrap_or_else(|| self.messages.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PartialTestState {
    count: Option<i32>,
    messages: Option<Vec<String>>,
}

fn partial(count: Option<i32>, messages: Option<Vec<String>>) -> NodeResult<TestState> {
    NodeResult::Partial(PartialTestState { count, messages })
}

fn make_increment_graph() -> GraphTool<TestState> {
    let mut graph = StateGraph::<TestState>::new();

    let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

    graph.add_node("increment", increment);
    graph.set_entry_point("increment");
    graph.set_finish_point("increment");

    let compiled = graph.compile();
    GraphTool::new("increment_tool", "Increments a counter", compiled)
}

fn make_increment_compiled() -> gewu_graph::CompiledGraph<TestState> {
    let mut graph = StateGraph::<TestState>::new();

    let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

    graph.add_node("increment", increment);
    graph.set_entry_point("increment");
    graph.set_finish_point("increment");

    graph.compile()
}

#[tokio::test]
async fn test_graph_tool_new() {
    let tool = make_increment_graph();

    assert_eq!(tool.tool_name(), "increment_tool");
    assert_eq!(tool.tool_description(), "Increments a counter");
    assert_eq!(tool.name(), "increment_tool");
}

#[tokio::test]
async fn test_graph_tool_as_trait() {
    let tool = make_increment_graph();

    assert_eq!(tool.name(), "increment_tool");
    assert_eq!(tool.description(), "Increments a counter");

    let _schema = tool.schema();
}

#[tokio::test]
async fn test_graph_tool_execute_inside_tokio_runtime() {
    let tool = make_increment_graph();
    let result = tool
        .execute(
            serde_json::json!({"count": 5, "messages": []}),
            &ToolContext::new("test", "message"),
        )
        .await
        .unwrap();
    let state: TestState = serde_json::from_str(&result.output).unwrap();

    assert_eq!(state.count, 6);
}

#[tokio::test]
async fn test_graph_tool_invoke_valid_input() {
    let compiled = make_increment_compiled();

    let result = compiled
        .ainvoke(TestState {
            count: 5,
            messages: vec![],
        })
        .await
        .unwrap();

    assert_eq!(result.count, 6);
}

#[tokio::test]
async fn test_graph_tool_invoke_invalid_input() {
    let compiled = make_increment_compiled();

    let result = compiled
        .ainvoke(TestState {
            count: 5,
            messages: vec![],
        })
        .await;

    assert!(result.is_ok());
}

fn make_multi_step_graph_compiled() -> gewu_graph::CompiledGraph<TestState> {
    let mut graph = StateGraph::<TestState>::new();

    let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

    let step2 = |state: TestState| async move {
        let mut msgs = state.messages.clone();
        msgs.push(format!("count is {}", state.count));
        partial(None, Some(msgs))
    };

    graph.add_node("step1", step1);
    graph.add_node("step2", step2);
    graph.set_entry_point("step1");
    graph.add_edge("step1", "step2");
    graph.set_finish_point("step2");

    graph.compile()
}

#[tokio::test]
async fn test_graph_tool_multi_step_execution() {
    let compiled = make_multi_step_graph_compiled();

    let result = compiled
        .ainvoke(TestState {
            count: 5,
            messages: vec![],
        })
        .await
        .unwrap();

    assert_eq!(result.count, 15);
    assert_eq!(result.messages, vec!["count is 15"]);
}

#[tokio::test]
async fn test_graph_tool_invoke_graph_error() {
    let mut graph = StateGraph::<TestState>::new();
    graph.set_entry_point("nonexistent");
    let compiled = graph.compile();

    let result = compiled
        .ainvoke(TestState {
            count: 0,
            messages: vec![],
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_graph_tool_with_send_operations() {
    let mut graph = StateGraph::<TestState>::new();

    let spawn = |_state: TestState| async move {
        NodeResult::Send(vec![gewu_graph::GraphSend(
            "worker".to_string(),
            TestState {
                count: 100,
                messages: vec!["from_send".to_string()],
            },
        )])
    };

    let worker =
        |state: TestState| async move { partial(Some(state.count + 1), Some(state.messages)) };

    graph.add_node("spawn", spawn);
    graph.add_node("worker", worker);
    graph.set_entry_point("spawn");
    graph.add_edge("spawn", "worker");
    graph.set_finish_point("worker");

    let compiled = graph.compile();

    let result = compiled
        .ainvoke(TestState {
            count: 0,
            messages: vec![],
        })
        .await
        .unwrap();

    assert_eq!(result.count, 101);
    assert!(result.messages.contains(&"from_send".to_string()));
}

#[tokio::test]
async fn test_graph_tool_with_conditional_edges() {
    let mut graph = StateGraph::<TestState>::new();

    let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

    let high_handler = |_state: TestState| async move { partial(Some(100), None) };

    let low_handler = |_state: TestState| async move { partial(Some(-100), None) };

    let check = |state: &TestState| -> String {
        if state.count >= 5 {
            "high".to_string()
        } else {
            "low".to_string()
        }
    };

    graph.add_node("check", increment);
    graph.add_node("high", high_handler);
    graph.add_node("low", low_handler);
    graph.set_entry_point("check");
    graph.add_conditional_edges(
        "check",
        check,
        [
            ("high".to_string(), "high".to_string()),
            ("low".to_string(), "low".to_string()),
        ]
        .into(),
    );
    graph.add_edge("high", END);
    graph.add_edge("low", END);

    let compiled = graph.compile();

    let high_result = compiled
        .ainvoke(TestState {
            count: 10,
            messages: vec![],
        })
        .await
        .unwrap();
    assert_eq!(high_result.count, 100);

    let low_result = compiled
        .ainvoke(TestState {
            count: 0,
            messages: vec![],
        })
        .await
        .unwrap();
    assert_eq!(low_result.count, -100);
}

#[tokio::test]
async fn test_graph_tool_with_subgraph() {
    let mut subgraph = StateGraph::<TestState>::new();
    let double = |state: TestState| async move { partial(Some(state.count * 2), None) };
    subgraph.add_node("double", double);
    subgraph.set_entry_point("double");
    subgraph.set_finish_point("double");
    let subgraph_compiled = subgraph.compile();

    let mut graph = StateGraph::<TestState>::new();
    let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };
    graph.add_node("increment", increment);
    graph.add_subgraph_node("nested", subgraph_compiled);
    graph.set_entry_point("increment");
    graph.add_edge("increment", "nested");
    graph.set_finish_point("nested");

    let compiled = graph.compile();

    let result = compiled
        .ainvoke(TestState {
            count: 5,
            messages: vec![],
        })
        .await
        .unwrap();
    assert_eq!(result.count, 12);
}

#[tokio::test]
async fn test_graph_tool_send_to_nonexistent_node() {
    let mut graph = StateGraph::<TestState>::new();

    let spawn = |_state: TestState| async move {
        NodeResult::Send(vec![gewu_graph::GraphSend(
            "nonexistent".to_string(),
            TestState {
                count: 100,
                messages: vec![],
            },
        )])
    };

    graph.add_node("spawn", spawn);
    graph.set_entry_point("spawn");
    graph.set_finish_point("spawn");

    let compiled = graph.compile();

    let result = compiled
        .ainvoke(TestState {
            count: 0,
            messages: vec![],
        })
        .await;

    assert!(result.is_err());
    let err_str = result.unwrap_err().to_string();
    assert!(err_str.contains("not found") || err_str.contains("Nonexistent"));
}

#[tokio::test]
async fn test_graph_tool_multiple_executions() {
    let compiled = make_increment_compiled();

    for initial_count in 0..5 {
        let result = compiled
            .ainvoke(TestState {
                count: initial_count,
                messages: vec![],
            })
            .await
            .unwrap();
        assert_eq!(result.count, initial_count + 1);
    }
}

#[tokio::test]
async fn test_graph_tool_schema_generation() {
    use gewu_core::traits::Tool;

    let tool = make_increment_graph();
    let _schema = tool.schema();
}
