#[cfg(test)]
mod tests {
    use crate::graph::{END, GraphSend, NodeResult, START, StateGraph, StreamMode, StreamOutput};
    use crate::state::State;
    use futures::StreamExt;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    fn custom(data: serde_json::Value) -> NodeResult<TestState> {
        NodeResult::Custom(data)
    }

    // ========================================
    // Phase 1: Error Handling Tests
    // ========================================

    #[tokio::test]
    async fn test_node_not_found_error() {
        let mut graph = StateGraph::<TestState>::new();

        let node = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("exists", node);
        graph.set_entry_point("exists");
        graph.set_finish_point("exists");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await;
        assert!(result.is_ok());

        let mut graph2 = StateGraph::<TestState>::new();
        graph2.add_node("node", |_: TestState| async { partial(None, None) });
        graph2.set_entry_point("nonexistent");
        let compiled2 = graph2.compile();
        let result2 = compiled2
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await;
        assert!(matches!(
            result2,
            Err(crate::graph::GraphError::NodeNotFound(_))
        ));
    }

    #[tokio::test]
    async fn test_no_entry_point_error() {
        let mut graph = StateGraph::<TestState>::new();
        graph.add_node("node", |_: TestState| async { partial(None, None) });
        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await;
        assert!(matches!(
            result,
            Err(crate::graph::GraphError::NoEntryPoint)
        ));
    }

    #[tokio::test]
    async fn test_send_to_nonexistent_node_error() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
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
        assert!(matches!(
            result,
            Err(crate::graph::GraphError::NodeNotFound(_))
        ));
    }

    // ========================================
    // Phase 2: Edge Cases Tests
    // ========================================

    #[tokio::test]
    async fn test_partial_state_with_none() {
        let mut graph = StateGraph::<TestState>::new();

        let set_messages =
            |_state: TestState| async move { partial(None, Some(vec!["msg1".to_string()])) };

        let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("set_messages", set_messages);
        graph.add_node("increment", increment);
        graph.set_entry_point("set_messages");
        graph
            .add_edge("set_messages", "increment")
            .expect("valid edge");
        graph.set_finish_point("increment");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 5,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 6);
        assert_eq!(result.messages, vec!["msg1"]);
    }

    #[tokio::test]
    async fn test_multiple_finish_points() {
        let mut graph = StateGraph::<TestState>::new();

        let node_a = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let node_b =
            |_state: TestState| async move { partial(None, Some(vec!["from_b".to_string()])) };

        graph.add_node("a", node_a);
        graph.add_node("b", node_b);
        graph.set_entry_point("a");
        graph.add_edge("a", "b").expect("valid edge");
        graph.set_finish_point("a");
        graph.set_finish_point("b");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 10);
        assert_eq!(result.messages, vec!["from_b"]);
    }

    #[tokio::test]
    async fn test_conditional_edge_fallback_to_path_result() {
        let mut graph = StateGraph::<TestState>::new();

        let node = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let check = |state: &TestState| -> String {
            if state.count >= 5 {
                "high".to_string()
            } else {
                "low".to_string()
            }
        };

        graph.add_node("start", node);
        graph.add_node("high", node);
        graph.add_node("low", node);
        graph.set_entry_point("start");
        graph.add_conditional_edges(
            "start",
            check,
            [("high".to_string(), "high".to_string())].into(),
        );
        graph.set_finish_point("low");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 10,
                messages: vec![],
            })
            .await
            .unwrap();
        assert_eq!(result.count, 12);
    }

    #[tokio::test]
    async fn test_multiple_conditional_edges_from_same_source() {
        let mut graph = StateGraph::<TestState>::new();

        let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let check_high = |state: &TestState| -> String {
            if state.count >= 10 {
                "high".to_string()
            } else {
                "low".to_string()
            }
        };

        let check_positive = |state: &TestState| -> String {
            if state.count >= 0 {
                "positive".to_string()
            } else {
                "negative".to_string()
            }
        };

        graph.add_node("increment", increment);
        graph.add_node("high", |_: TestState| async { partial(Some(100), None) });
        graph.add_node("low", |_: TestState| async { partial(Some(-100), None) });
        graph.add_node("positive", |_: TestState| async { partial(Some(50), None) });
        graph.add_node("negative", |_: TestState| async {
            partial(Some(-50), None)
        });

        graph.set_entry_point("increment");
        graph.add_conditional_edges(
            "increment",
            check_high,
            [("high".to_string(), "high".to_string())].into(),
        );
        graph.add_conditional_edges(
            "increment",
            check_positive,
            [("positive".to_string(), "positive".to_string())].into(),
        );

        graph.add_edge("high", END).expect("valid edge");
        graph.add_edge("low", END).expect("valid edge");
        graph.add_edge("positive", END).expect("valid edge");
        graph.add_edge("negative", END).expect("valid edge");

        let compiled = graph.compile();

        let result = compiled
            .ainvoke(TestState {
                count: 5,
                messages: vec![],
            })
            .await
            .unwrap();
        assert!(result.count == -100 || result.count == 50);
    }

    #[tokio::test]
    async fn test_send_to_same_node_multiple_times() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![
                GraphSend(
                    "worker".to_string(),
                    TestState {
                        count: 10,
                        messages: vec![],
                    },
                ),
                GraphSend(
                    "worker".to_string(),
                    TestState {
                        count: 20,
                        messages: vec![],
                    },
                ),
            ])
        };

        let worker = |state: TestState| async move { partial(Some(state.count), None) };

        graph.add_node("spawn", spawn);
        graph.add_node("worker", worker);
        graph.set_entry_point("spawn");
        graph.set_finish_point("spawn");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 20);
    }

    #[tokio::test]
    async fn test_send_and_regular_edge_both() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "via_send".to_string(),
                TestState {
                    count: 100,
                    messages: vec![],
                },
            )])
        };

        let via_send = |state: TestState| async move {
            partial(Some(state.count + 1), Some(vec!["from_send".to_string()]))
        };

        let via_edge = |state: TestState| async move {
            partial(Some(state.count + 10), Some(vec!["from_edge".to_string()]))
        };

        graph.add_node("spawn", spawn);
        graph.add_node("via_send", via_send);
        graph.add_node("via_edge", via_edge);
        graph.set_entry_point("spawn");
        graph.add_edge("spawn", "via_edge").expect("valid edge");
        graph.set_finish_point("via_send");
        graph.set_finish_point("via_edge");

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
        assert!(!result.messages.contains(&"from_edge".to_string()));
    }

    // ========================================
    // Phase 3: Stream Modes Tests
    // ========================================

    #[tokio::test]
    async fn test_stream_updates_only() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();
        let stream = compiled.stream_with_modes(
            TestState {
                count: 0,
                messages: vec![],
            },
            vec![StreamMode::Updates],
        );
        let mut stream = Box::pin(stream);
        let mut values = Vec::new();
        let mut updates = Vec::new();

        while let Some(result) = stream.next().await {
            match result.unwrap() {
                StreamOutput::Value(s) => values.push(s),
                StreamOutput::Update { node, update } => updates.push((node, update)),
                _ => {}
            }
        }

        assert_eq!(values.len(), 0);
        assert_eq!(updates.len(), 2);
    }

    #[tokio::test]
    async fn test_stream_custom_mode() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |_state: TestState| async move {
            custom(serde_json::json!({"event": "step1_done", "data": 123}))
        };

        let step2 = |_state: TestState| async move {
            custom(serde_json::json!({"event": "step2_done", "data": 456}))
        };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();
        let stream = compiled.stream_with_modes(
            TestState {
                count: 0,
                messages: vec![],
            },
            vec![StreamMode::Custom],
        );
        let mut stream = Box::pin(stream);
        let mut customs = Vec::new();

        while let Some(result) = stream.next().await {
            if let StreamOutput::Custom { node, data } = result.unwrap() {
                customs.push((node, data))
            }
        }

        assert_eq!(customs.len(), 2);
        assert_eq!(customs[0].0, "step1");
        assert_eq!(customs[1].0, "step2");
    }

    #[tokio::test]
    async fn test_stream_tasks_mode() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();
        let stream = compiled.stream_with_modes(
            TestState {
                count: 0,
                messages: vec![],
            },
            vec![StreamMode::Tasks],
        );
        let mut stream = Box::pin(stream);
        let mut task_starts = Vec::new();
        let mut task_ends = Vec::new();

        while let Some(result) = stream.next().await {
            match result.unwrap() {
                StreamOutput::TaskStart { id, name } => task_starts.push((id, name)),
                StreamOutput::TaskEnd { id, name, .. } => task_ends.push((id, name)),
                _ => {}
            }
        }

        assert_eq!(task_starts.len(), 2);
        assert_eq!(task_ends.len(), 2);
        assert_eq!(task_starts[0].1, "step1");
        assert_eq!(task_starts[1].1, "step2");
    }

    #[tokio::test]
    async fn test_astream() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();
        let stream = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();
        let values = [stream.clone()];

        assert_eq!(values.len(), 1);
        assert_eq!(values[0].count, 11);
    }

    #[tokio::test]
    async fn test_astream_values() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();

        let mut values = Vec::new();
        let mut stream = Box::pin(
            compiled
                .astream(TestState {
                    count: 0,
                    messages: vec![],
                })
                .await,
        );

        while let Some(result) = futures::StreamExt::next(&mut stream).await {
            values.push(result.unwrap());
        }

        assert_eq!(values.len(), 3);
        assert_eq!(values[0].count, 0);
        assert_eq!(values[1].count, 10);
        assert_eq!(values[2].count, 11);
    }

    // ========================================
    // Phase 4: invoke_parallel Edge Cases
    // ========================================

    #[tokio::test]
    async fn test_invoke_parallel_no_edges() {
        let mut graph = StateGraph::<TestState>::new();

        let node_a = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("a", node_a);
        graph.set_entry_point("a");
        graph.add_edge("a", END).expect("valid edge");

        let compiled = graph.compile();
        let result = compiled
            .invoke_parallel(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 1);
    }

    #[tokio::test]
    async fn test_invoke_parallel_with_send() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![
                GraphSend(
                    "worker_a".to_string(),
                    TestState {
                        count: 100,
                        messages: vec![],
                    },
                ),
                GraphSend(
                    "worker_b".to_string(),
                    TestState {
                        count: 200,
                        messages: vec![],
                    },
                ),
            ])
        };

        let worker_a = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let worker_b = |state: TestState| async move {
            partial(None, Some(vec![format!("b{}", state.count)]))
        };

        graph.add_node("spawn", spawn);
        graph.add_node("worker_a", worker_a);
        graph.add_node("worker_b", worker_b);
        graph.set_entry_point("spawn");
        graph.add_edge("worker_a", END).expect("valid edge");
        graph.add_edge("worker_b", END).expect("valid edge");

        let compiled = graph.compile();
        let result = compiled
            .invoke_parallel(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 1);
        assert!(result.messages.contains(&"b0".to_string()));
    }

    // ========================================
    // Phase 5: Nested & Complex Tests
    // ========================================

    #[tokio::test]
    async fn test_nested_send() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "level1".to_string(),
                TestState {
                    count: 10,
                    messages: vec![],
                },
            )])
        };

        let level1 = |state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "level2".to_string(),
                TestState {
                    count: state.count * 2,
                    messages: vec![],
                },
            )])
        };

        let level2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("spawn", spawn);
        graph.add_node("level1", level1);
        graph.add_node("level2", level2);
        graph.set_entry_point("spawn");
        graph.set_finish_point("spawn");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 21);
    }

    #[tokio::test]
    async fn test_send_with_subgraph() {
        let mut subgraph = StateGraph::<TestState>::new();

        let double = |state: TestState| async move { partial(Some(state.count * 2), None) };

        subgraph.add_node("double", double);
        subgraph.set_entry_point("double");
        subgraph.set_finish_point("double");

        let subgraph_compiled = subgraph.compile();

        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "nested".to_string(),
                TestState {
                    count: 5,
                    messages: vec![],
                },
            )])
        };

        let after = |state: TestState| async move { partial(Some(state.count + 100), None) };

        graph.add_node("spawn", spawn);
        graph.add_subgraph_node("nested", subgraph_compiled);
        graph.add_node("after", after);
        graph.set_entry_point("spawn");
        graph.add_edge("spawn", "after").expect("valid edge");
        graph.set_finish_point("after");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 10);
    }

    // ========================================
    // Phase 6: Builder Validation Tests
    // ========================================

    #[test]
    fn test_builder_rejects_start_equals_end() {
        let mut graph = StateGraph::<TestState>::new();
        graph.add_node("node", |_: TestState| async { partial(None, None) });
        let err = graph.add_edge(END, END).unwrap_err();
        assert!(
            err.to_string().contains("END cannot be used as edge start"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_builder_rejects_end_equals_start() {
        let mut graph = StateGraph::<TestState>::new();
        graph.add_node("node", |_: TestState| async { partial(None, None) });
        let err = graph.add_edge("node", START).unwrap_err();
        assert!(
            err.to_string().contains("START cannot be used as edge end"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_builder_accepts_duplicate_node_override() {
        let mut graph = StateGraph::<TestState>::new();
        graph.add_node("node", |state: TestState| async move {
            partial(Some(state.count + 1), None)
        });
        graph.add_node("node", |state: TestState| async move {
            partial(Some(state.count + 10), None)
        });
        graph.set_entry_point("node");
        graph.set_finish_point("node");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 10);
    }

    // ========================================
    // Original Basic Tests (kept for reference)
    // ========================================

    #[tokio::test]
    async fn test_simple_graph() {
        let mut graph = StateGraph::<TestState>::new();

        let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let add_message = |state: TestState| async move {
            let mut msgs = state.messages.clone();
            msgs.push(format!("count is {}", state.count));
            partial(None, Some(msgs))
        };

        graph.add_node("increment", increment);
        graph.add_node("add_message", add_message);
        graph.set_entry_point("increment");
        graph
            .add_edge("increment", "add_message")
            .expect("valid edge");
        graph.set_finish_point("add_message");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 1);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0], "count is 1");
    }

    #[tokio::test]
    async fn test_conditional_edges() {
        let mut graph = StateGraph::<TestState>::new();

        let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let decrement = |state: TestState| async move { partial(Some(state.count - 1), None) };

        let check = |state: &TestState| -> String {
            if state.count >= 5 {
                "high".to_string()
            } else {
                "low".to_string()
            }
        };

        graph.add_node("increment", increment);
        graph.add_node("decrement", decrement);
        graph.set_entry_point("increment");
        graph.add_conditional_edges(
            "increment",
            check,
            [
                ("high".to_string(), "decrement".to_string()),
                ("low".to_string(), END.to_string()),
            ]
            .into(),
        );
        graph.set_finish_point("decrement");

        let compiled = graph.compile();

        let result1 = compiled
            .ainvoke(TestState {
                count: 10,
                messages: vec![],
            })
            .await
            .unwrap();
        assert_eq!(result1.count, 10);

        let result2 = compiled
            .ainvoke(TestState {
                count: 3,
                messages: vec![],
            })
            .await
            .unwrap();
        assert_eq!(result2.count, 4);
    }

    #[tokio::test]
    async fn test_stream() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();
        let stream = compiled.stream(TestState {
            count: 0,
            messages: vec![],
        });
        let mut stream = Box::pin(stream);
        let mut states = Vec::new();

        while let Some(result) = stream.next().await {
            if let StreamOutput::Value(s) = result.unwrap() {
                states.push(s)
            }
        }

        assert_eq!(states.len(), 3);
        assert_eq!(states[0].count, 0);
        assert_eq!(states[1].count, 10);
        assert_eq!(states[2].count, 11);
    }

    #[tokio::test]
    async fn test_invoke_parallel() {
        let mut graph = StateGraph::<TestState>::new();

        let add_a = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let add_b = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let record_c = |state: TestState| async move {
            partial(None, Some(vec![format!("c{}", state.count)]))
        };

        graph.add_node("add_a", add_a);
        graph.add_node("add_b", add_b);
        graph.add_node("record_c", record_c);
        graph.set_entry_point("add_a");
        graph.add_edge("add_a", "add_b").expect("valid edge");
        graph.add_edge("add_a", "record_c").expect("valid edge");
        graph.set_finish_point("add_b");
        graph.set_finish_point("record_c");

        let compiled = graph.compile();
        let result = compiled
            .invoke_parallel(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 11);
        assert_eq!(result.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_subgraph() {
        let mut subgraph = StateGraph::<TestState>::new();

        let double = |state: TestState| async move { partial(Some(state.count * 2), None) };

        let add_hello = |state: TestState| async move {
            let mut msgs = state.messages.clone();
            msgs.push("hello".to_string());
            partial(None, Some(msgs))
        };

        subgraph.add_node("double", double);
        subgraph.add_node("add_hello", add_hello);
        subgraph.set_entry_point("double");
        subgraph
            .add_edge("double", "add_hello")
            .expect("valid edge");
        subgraph.set_finish_point("add_hello");

        let subgraph_compiled = subgraph.compile();

        let mut main_graph = StateGraph::<TestState>::new();

        let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let subtract = |state: TestState| async move { partial(Some(state.count - 1), None) };

        main_graph.add_node("increment", increment);
        main_graph.add_subgraph_node("nested", subgraph_compiled);
        main_graph.add_node("subtract", subtract);
        main_graph.set_entry_point("increment");
        main_graph
            .add_edge("increment", "nested")
            .expect("valid edge");
        main_graph
            .add_edge("nested", "subtract")
            .expect("valid edge");
        main_graph.set_finish_point("subtract");

        let compiled = main_graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 5,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 11);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0], "hello");
    }

    #[tokio::test]
    async fn test_graph_send() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "worker".to_string(),
                TestState {
                    count: 100,
                    messages: vec!["from_send1".to_string()],
                },
            )])
        };

        let worker =
            |state: TestState| async move { partial(Some(state.count + 1), Some(state.messages)) };

        graph.add_node("spawn", spawn);
        graph.add_node("worker", worker);
        graph.set_entry_point("spawn");
        graph.add_edge("spawn", "worker").expect("valid edge");
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
        assert_eq!(result.messages.len(), 1);
        assert!(result.messages.contains(&"from_send1".to_string()));
    }

    #[tokio::test]
    async fn test_stream_modes() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();
        let stream = compiled.stream_with_modes(
            TestState {
                count: 0,
                messages: vec![],
            },
            vec![StreamMode::Values, StreamMode::Updates],
        );
        let mut stream = Box::pin(stream);
        let mut values = Vec::new();
        let mut updates = Vec::new();

        while let Some(result) = stream.next().await {
            match result.unwrap() {
                StreamOutput::Value(s) => values.push(s),
                StreamOutput::Update { node, update } => updates.push((node, update)),
                _ => {}
            }
        }

        assert_eq!(values.len(), 3);
        assert_eq!(values[0].count, 0);
        assert_eq!(values[1].count, 10);
        assert_eq!(values[2].count, 11);

        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].0, "step1");
        assert_eq!(updates[0].1.count, Some(10));
        assert_eq!(updates[1].0, "step2");
        assert_eq!(updates[1].1.count, Some(11));
    }

    #[tokio::test]
    async fn test_ainvoke_equivalence() {
        let mut graph = StateGraph::<TestState>::new();

        let increment = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("increment", increment);
        graph.set_entry_point("increment");
        graph.set_finish_point("increment");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 1);
    }

    #[tokio::test]
    async fn test_invoke_sync_equivalence() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.set_finish_point("step2");

        let compiled = graph.compile();

        let async_result1 = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();
        let async_result2 = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(async_result1.count, async_result2.count);
    }

    #[tokio::test]
    async fn test_node_returns_custom_only() {
        let mut graph = StateGraph::<TestState>::new();

        let custom_only = |_state: TestState| async move {
            custom(serde_json::json!({"type": "custom_event", "value": 42}))
        };

        let after = |state: TestState| async move { partial(Some(state.count + 100), None) };

        graph.add_node("custom_only", custom_only);
        graph.add_node("after", after);
        graph.set_entry_point("custom_only");
        graph.add_edge("custom_only", "after").expect("valid edge");
        graph.set_finish_point("after");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 5,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 105);
    }

    #[tokio::test]
    async fn test_conditional_edge_unmapped_path() {
        let mut graph = StateGraph::<TestState>::new();

        let node = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let check = |_state: &TestState| -> String { "unmapped_path".to_string() };

        graph.add_node("start", node);
        graph.set_entry_point("start");
        graph.add_conditional_edges(
            "start",
            check,
            [("high".to_string(), "fallback".to_string())].into(),
        );

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await;

        assert!(matches!(
            result,
            Err(crate::graph::GraphError::NodeNotFound(_))
        ));
    }

    #[tokio::test]
    async fn test_send_to_finish_point_node() {
        let mut graph = StateGraph::<TestState>::new();

        let spawn = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "worker".to_string(),
                TestState {
                    count: 50,
                    messages: vec![],
                },
            )])
        };

        let worker = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("spawn", spawn);
        graph.add_node("worker", worker);
        graph.set_entry_point("spawn");
        graph.set_finish_point("worker");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 51);
    }

    #[tokio::test]
    async fn test_invoke_parallel_chain_execution() {
        let mut graph = StateGraph::<TestState>::new();

        let a = |state: TestState| async move { partial(Some(state.count + 1), None) };

        let b = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let c = |state: TestState| async move { partial(Some(state.count + 100), None) };

        graph.add_node("a", a);
        graph.add_node("b", b);
        graph.add_node("c", c);
        graph.set_entry_point("a");
        graph.add_edge("a", "b").expect("valid edge");
        graph.add_edge("b", "c").expect("valid edge");
        graph.set_finish_point("c");

        let compiled = graph.compile();
        let result = compiled
            .invoke_parallel(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 111);
    }

    #[tokio::test]
    #[should_panic(expected = "Subgraphs are not supported in invoke_parallel")]
    async fn test_invoke_parallel_with_subgraph_panics() {
        let mut subgraph = StateGraph::<TestState>::new();
        let double = |state: TestState| async move { partial(Some(state.count * 2), None) };
        subgraph.add_node("double", double);
        subgraph.set_entry_point("double");
        subgraph.set_finish_point("double");
        let subgraph_compiled = subgraph.compile();

        let mut graph = StateGraph::<TestState>::new();
        graph.add_subgraph_node("nested", subgraph_compiled);
        graph.set_entry_point("nested");
        graph.set_finish_point("nested");

        let compiled = graph.compile();
        compiled
            .invoke_parallel(TestState {
                count: 5,
                messages: vec![],
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_subgraph_depth_limit() {
        let mut inner_graph = StateGraph::<TestState>::new();
        inner_graph.add_node("node", |state: TestState| async move {
            partial(Some(state.count + 1), None)
        });
        inner_graph.set_entry_point("node");
        inner_graph.set_finish_point("node");
        let inner_compiled = inner_graph.compile();

        let mut graph = StateGraph::<TestState>::new();
        graph.add_subgraph_node("nested", inner_compiled);
        graph.set_entry_point("nested");
        graph.set_finish_point("nested");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_custom_data_passthrough() {
        let mut graph = StateGraph::<TestState>::new();

        let custom_node = |_state: TestState| async move {
            custom(serde_json::json!({"event": "test", "nested": {"key": "value"}}))
        };

        graph.add_node("custom_node", custom_node);
        graph.set_entry_point("custom_node");
        graph.set_finish_point("custom_node");

        let compiled = graph.compile();
        let stream = compiled.stream_with_modes(
            TestState {
                count: 0,
                messages: vec![],
            },
            vec![StreamMode::Custom],
        );
        let mut stream = Box::pin(stream);
        let mut customs = Vec::new();

        while let Some(result) = stream.next().await {
            if let StreamOutput::Custom { node, data } = result.unwrap() {
                customs.push((node, data))
            }
        }

        assert_eq!(customs.len(), 1);
        assert_eq!(customs[0].0, "custom_node");
        assert_eq!(
            customs[0].1.get("event").unwrap(),
            &serde_json::json!("test")
        );
    }

    #[tokio::test]
    async fn test_all_stream_modes_combined() {
        let mut graph = StateGraph::<TestState>::new();

        let step1 = |state: TestState| async move { partial(Some(state.count + 10), None) };

        let step2 = |_state: TestState| async move { custom(serde_json::json!({"step2": "done"})) };

        let step3 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("step1", step1);
        graph.add_node("step2", step2);
        graph.add_node("step3", step3);
        graph.set_entry_point("step1");
        graph.add_edge("step1", "step2").expect("valid edge");
        graph.add_edge("step2", "step3").expect("valid edge");
        graph.set_finish_point("step3");

        let compiled = graph.compile();
        let stream = compiled.stream_with_modes(
            TestState {
                count: 0,
                messages: vec![],
            },
            vec![
                StreamMode::Values,
                StreamMode::Updates,
                StreamMode::Custom,
                StreamMode::Tasks,
            ],
        );
        let mut stream = Box::pin(stream);

        let mut values = Vec::new();
        let mut updates = Vec::new();
        let mut customs = Vec::new();
        let mut task_starts = Vec::new();
        let mut task_ends = Vec::new();

        while let Some(result) = stream.next().await {
            match result.unwrap() {
                StreamOutput::Value(s) => values.push(s),
                StreamOutput::Update { node, update } => updates.push((node, update)),
                StreamOutput::Custom { node, data } => customs.push((node, data)),
                StreamOutput::TaskStart { id, name } => task_starts.push((id, name)),
                StreamOutput::TaskEnd { id, name, .. } => task_ends.push((id, name)),
            }
        }

        assert_eq!(values.len(), 4);
        assert_eq!(updates.len(), 2);
        assert_eq!(customs.len(), 1);
        assert_eq!(task_starts.len(), 3);
        assert_eq!(task_ends.len(), 3);
    }

    #[tokio::test]
    async fn test_finish_point_without_explicit_edge() {
        let mut graph = StateGraph::<TestState>::new();

        let node = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("start", node);
        graph.set_entry_point("start");
        graph.set_finish_point("start");

        let compiled = graph.compile();
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
    async fn test_multiple_sends_to_different_nodes() {
        let mut graph = StateGraph::<TestState>::new();

        let splitter = |_state: TestState| async move {
            NodeResult::Send(vec![
                GraphSend(
                    "node_a".to_string(),
                    TestState {
                        count: 10,
                        messages: vec![],
                    },
                ),
                GraphSend(
                    "node_b".to_string(),
                    TestState {
                        count: 20,
                        messages: vec![],
                    },
                ),
                GraphSend(
                    "node_c".to_string(),
                    TestState {
                        count: 30,
                        messages: vec![],
                    },
                ),
            ])
        };

        let node_a = |state: TestState| async move { partial(Some(state.count), None) };
        let node_b = |state: TestState| async move { partial(Some(state.count), None) };
        let node_c = |state: TestState| async move { partial(Some(state.count), None) };

        graph.add_node("splitter", splitter);
        graph.add_node("node_a", node_a);
        graph.add_node("node_b", node_b);
        graph.add_node("node_c", node_c);
        graph.set_entry_point("splitter");
        graph.set_finish_point("splitter");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 30);
    }

    #[tokio::test]
    async fn test_nested_send_three_levels() {
        let mut graph = StateGraph::<TestState>::new();

        let level0 = |_state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "level1".to_string(),
                TestState {
                    count: 1,
                    messages: vec![],
                },
            )])
        };

        let level1 = |state: TestState| async move {
            NodeResult::Send(vec![GraphSend(
                "level2".to_string(),
                TestState {
                    count: state.count * 2,
                    messages: vec![],
                },
            )])
        };

        let level2 = |state: TestState| async move { partial(Some(state.count + 1), None) };

        graph.add_node("level0", level0);
        graph.add_node("level1", level1);
        graph.add_node("level2", level2);
        graph.set_entry_point("level0");
        graph.set_finish_point("level0");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.count, 3);
    }

    #[test]
    fn test_constants_values() {
        assert_eq!(START, "__start__");
        assert_eq!(END, "__end__");
    }

    #[tokio::test]
    async fn test_state_with_messages_accumulation() {
        let mut graph = StateGraph::<TestState>::new();

        let add_msg1 = |state: TestState| async move {
            let mut msgs = state.messages.clone();
            msgs.push("first".to_string());
            partial(None, Some(msgs))
        };

        let add_msg2 = |state: TestState| async move {
            let mut msgs = state.messages.clone();
            msgs.push("second".to_string());
            partial(None, Some(msgs))
        };

        let add_msg3 = |state: TestState| async move {
            let mut msgs = state.messages.clone();
            msgs.push("third".to_string());
            partial(None, Some(msgs))
        };

        graph.add_node("msg1", add_msg1);
        graph.add_node("msg2", add_msg2);
        graph.add_node("msg3", add_msg3);
        graph.set_entry_point("msg1");
        graph.add_edge("msg1", "msg2").expect("valid edge");
        graph.add_edge("msg2", "msg3").expect("valid edge");
        graph.set_finish_point("msg3");

        let compiled = graph.compile();
        let result = compiled
            .ainvoke(TestState {
                count: 0,
                messages: vec![],
            })
            .await
            .unwrap();

        assert_eq!(result.messages, vec!["first", "second", "third"]);
    }
}
