use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use gewu_core::agent::AgentBuilder;
use gewu_core::compaction::{estimate_message_tokens, estimate_tokens, is_overflow, prune};
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{AgentEvent, ContentPart, Message, ProviderEvent};
use std::error::Error;
use std::sync::{Arc, Mutex};

struct MockLlm {
    responses: Arc<Mutex<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let mut responses = self.responses.lock().unwrap();
        let response = responses.remove(0);
        let stream = async_stream::try_stream! {
            if let Message::Assistant { content: Some(content), .. } = response {
                yield ProviderEvent::ContentDelta { delta: content };
            }
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["mock".to_string()])
    }
}

#[test]
fn test_estimate_tokens_exact() {
    assert_eq!(estimate_tokens("hello"), 5 / 4);
    assert_eq!(estimate_tokens("hello world"), 11 / 4);
}

#[test]
fn test_estimate_tokens_empty() {
    assert_eq!(estimate_tokens(""), 0);
}

#[test]
fn test_estimate_tokens_long_text() {
    let text = "A".repeat(100);
    assert_eq!(estimate_tokens(&text), 25);
}

#[test]
fn test_is_overflow_under_limit() {
    assert!(!is_overflow(50_000, 128_000));
    assert!(!is_overflow(100_000, 128_000));
}

#[test]
fn test_is_overflow_at_limit() {
    assert!(is_overflow(108_000, 128_000));
}

#[test]
fn test_is_overflow_above_limit() {
    assert!(is_overflow(150_000, 128_000));
}

#[test]
fn test_is_overflow_zero_limit() {
    assert!(!is_overflow(100_000, 0));
}

#[test]
fn test_estimate_message_tokens_system() {
    let msg = Message::System {
        content: "Hello".to_string(),
    };
    assert_eq!(estimate_message_tokens(&msg), 5 / 4);
}

#[test]
fn test_estimate_message_tokens_user_text() {
    let msg = Message::User {
        content: vec![ContentPart::Text {
            text: "Hello world".to_string(),
        }],
    };
    assert_eq!(estimate_message_tokens(&msg), 11 / 4);
}

#[test]
fn test_estimate_message_tokens_user_with_media() {
    let msg = Message::User {
        content: vec![
            ContentPart::Text {
                text: "Hello".to_string(),
            },
            ContentPart::Image {
                data: "imagedata".to_string(),
                mime_type: "image/png".to_string(),
            },
        ],
    };
    let expected = estimate_tokens("Hello") + estimate_tokens("imagedata");
    assert_eq!(estimate_message_tokens(&msg), expected);
}

#[test]
fn test_estimate_message_tokens_assistant() {
    let msg = Message::Assistant {
        content: Some("Hello".to_string()),
        tool_calls: None,
    };
    assert_eq!(estimate_message_tokens(&msg), 5 / 4);
}

#[test]
fn test_estimate_message_tokens_assistant_with_tool_calls() {
    let msg = Message::Assistant {
        content: Some("Calling tool".to_string()),
        tool_calls: Some(vec![gewu_core::types::ToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: gewu_core::types::FunctionCall {
                name: "get_weather".to_string(),
                arguments: "{\"city\":\"NYC\"}".to_string(),
            },
        }]),
    };
    let text_tokens = estimate_tokens("Calling tool");
    let name_tokens = estimate_tokens("get_weather");
    let args_tokens = estimate_tokens("{\"city\":\"NYC\"}");
    assert_eq!(
        estimate_message_tokens(&msg),
        text_tokens + name_tokens + args_tokens
    );
}

#[test]
fn test_estimate_message_tokens_tool() {
    let msg = Message::Tool {
        tool_call_id: "call_1".to_string(),
        tool_name: Some("read_file".to_string()),
        content: "File contents here".to_string(),
        media: vec![],
    };
    assert_eq!(
        estimate_message_tokens(&msg),
        estimate_tokens("File contents here")
    );
}

#[test]
fn test_prune_does_nothing_when_empty() {
    let mut messages: Vec<Message> = vec![];
    assert_eq!(prune(&mut messages), 0);
}

#[test]
fn test_prune_does_nothing_when_under_threshold() {
    let mut messages = vec![
        Message::System {
            content: "You are helpful".to_string(),
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
        },
        Message::Assistant {
            content: Some("Hi".to_string()),
            tool_calls: None,
        },
        Message::Tool {
            tool_call_id: "call_1".to_string(),
            tool_name: Some("tool".to_string()),
            content: "Short output".to_string(),
            media: vec![],
        },
    ];
    let saved = prune(&mut messages);
    assert_eq!(saved, 0);
}

#[tokio::test]
async fn test_agent_ask_compacts_when_history_exceeds_context_limit() {
    let responses = vec![
        Message::Assistant {
            content: Some("Prior context summarized.".to_string()),
            tool_calls: None,
        },
        Message::Assistant {
            content: Some("I am ready.".to_string()),
            tool_calls: None,
        },
    ];
    let llm = Arc::new(MockLlm {
        responses: Arc::new(Mutex::new(responses)),
    });

    // Set a very low limit (e.g., 100 tokens).
    // "Hello" is small, but we will pre-fill history.
    let mut agent = AgentBuilder::default()
        .llm(llm)
        .context_limit(100)
        .system_prompt("System")
        .build()
        .unwrap();

    agent.reset().await.unwrap(); // Ensure system prompt is in history

    // Fill history with junk to exceed 100 tokens
    // 1 token ~ 4 chars. 100 tokens ~ 400 chars.
    let long_text = "A".repeat(500);
    agent
        .memory
        .add_message(Message::User {
            content: vec![gewu_core::types::ContentPart::Text { text: long_text }],
        })
        .await
        .unwrap();

    // This call should trigger compaction because history > 100
    let mut stream = Box::pin(agent.ask("Hello"));
    let mut response = String::new();
    while let Some(event) = stream.next().await {
        if let Ok(AgentEvent::ContentDelta { delta }) = event {
            response.push_str(&delta);
        } else if let Ok(AgentEvent::Finish { response: r }) = event {
            response = r;
        }
    }
    drop(stream); // Release borrow

    assert_eq!(response, "I am ready.");
}

#[test]
fn test_prune_protects_recent_turns() {
    let mut messages = vec![
        Message::System {
            content: "You are helpful".to_string(),
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: "First".to_string(),
            }],
        },
        Message::Assistant {
            content: Some("Response 1".to_string()),
            tool_calls: None,
        },
        Message::Tool {
            tool_call_id: "call_1".to_string(),
            tool_name: Some("tool1".to_string()),
            content: "Old output 1".to_string(),
            media: vec![],
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: "Second".to_string(),
            }],
        },
        Message::Assistant {
            content: Some("Response 2".to_string()),
            tool_calls: None,
        },
        Message::Tool {
            tool_call_id: "call_2".to_string(),
            tool_name: Some("tool2".to_string()),
            content: "Recent output".to_string(),
            media: vec![],
        },
    ];

    prune(&mut messages);

    if let Message::Tool { content, .. } = &messages[3] {
        assert_eq!(content, "Old output 1");
    } else {
        panic!("Expected Tool message at index 3");
    }
    if let Message::Tool { content, .. } = &messages[6] {
        assert_eq!(content, "Recent output");
    } else {
        panic!("Expected Tool message at index 6");
    }
}

#[test]
fn test_prune_stops_at_summary() {
    let mut messages = vec![
        Message::System {
            content: "You are helpful".to_string(),
        },
        Message::Assistant {
            content: Some("Summary: Previous conversation...".to_string()),
            tool_calls: None,
        },
        Message::User {
            content: vec![ContentPart::Text {
                text: "Recent".to_string(),
            }],
        },
        Message::Tool {
            tool_call_id: "call_1".to_string(),
            tool_name: Some("tool".to_string()),
            content: "Should not be pruned".to_string(),
            media: vec![],
        },
    ];

    prune(&mut messages);

    if let Message::Tool { content, .. } = &messages[3] {
        assert_eq!(content, "Should not be pruned");
    } else {
        panic!("Expected Tool message at index 3");
    }
}
