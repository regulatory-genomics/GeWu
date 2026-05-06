use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::agent::AgentBuilder;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{AgentEvent, Message, ProviderEvent};
use std::error::Error;
use std::sync::{Arc, Mutex};

struct MockLlm {
    responses: Arc<Mutex<Vec<Message>>>,
}

#[derive(Debug)]
#[allow(dead_code)]
struct MockError(String);
impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Error for MockError {}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let last_msg = messages.last().unwrap();
        if let Message::User { content } = last_msg {
            let text = content
                .iter()
                .find_map(|p| match p {
                    gewu_core::types::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            if text.contains("Provide a detailed prompt for continuing") {
                let s = async_stream::try_stream! {
                    yield ProviderEvent::ContentDelta { delta: "## Goal\nTest compaction\n## Accomplished\nNothing yet".to_string() };
                };
                return Ok(Box::pin(s));
            }
        }

        let mut responses = self.responses.lock().unwrap();
        let response = if responses.is_empty() {
            // Fallback default response
            Message::Assistant {
                content: Some("Default response".to_string()),
                tool_calls: None,
            }
        } else {
            responses.remove(0)
        };

        // Convert Message to Stream events
        let s = async_stream::try_stream! {
            if let Message::Assistant { content, tool_calls } = response {
                if let Some(c) = content {
                    yield ProviderEvent::ContentDelta { delta: c };
                }
                if let Some(calls) = tool_calls {
                     for call in calls {
                         yield ProviderEvent::ToolCallStart { id: call.id.clone(), name: call.function.name };
                         yield ProviderEvent::ToolCallDelta { id: call.id, delta: call.function.arguments };
                     }
                }
            }
        };
        Ok(Box::pin(s))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["mock".to_string()])
    }
}

#[tokio::test]
async fn test_compaction_trigger() {
    let responses = vec![
        // Response to "Hello" (after compaction)
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

    // Verify history structure
    // 1. System
    // 2. Summary (Assistant)
    // 3. User ("Hello")
    // 4. Assistant ("I am ready.")

    assert_eq!(agent.memory.messages().len(), 4);

    match &agent.memory.messages()[1] {
        Message::Assistant { content, .. } => {
            let text = content.as_ref().unwrap();
            assert!(text.contains("Summary of previous conversation"));
            assert!(text.contains("## Goal"));
        }
        _ => panic!("Expected summary message at index 1"),
    }
}
