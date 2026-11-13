//! Service-specific providers sharing the Chat Completions implementation.
use crate::ChatCompletionsProvider as OpenAIProvider;
use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_core::traits::{LlmProvider, StatusSink, Tool};
use gewu_core::types::{Message, ProviderEvent};
use serde_json::Value;
use std::{error::Error, sync::Arc, time::Duration};

/// Fixed service policy, applied only when constructing a provider.
pub(crate) struct ChatPolicy {
    pub reasoning_field: &'static str,
}

macro_rules! chat_service {
    ($name:ident, $endpoint:literal, $field:literal) => {
        /// A service provider with fixed reasoning replay rules. HTTP, SSE,
        /// tools, and retry handling are shared with the Chat Completions adapter.
        pub struct $name {
            chat: OpenAIProvider,
        }

        impl $name {
            pub fn with_reasoning_effort(mut self, effort: &str) -> Self {
                self.chat = self.chat.with_reasoning_effort(effort);
                self
            }
            pub fn with_max_tokens(mut self, limit: Option<u32>) -> Self {
                self.chat = self.chat.with_max_tokens(limit);
                self
            }
            pub fn new(api_key: &str, model: &str) -> Self {
                Self::new_with_endpoint(api_key, $endpoint, model)
            }

            pub fn new_with_endpoint(api_key: &str, base_url: &str, model: &str) -> Self {
                Self {
                    chat: OpenAIProvider::for_service(
                        api_key,
                        base_url,
                        model,
                        ChatPolicy {
                            reasoning_field: $field,
                        },
                    ),
                }
            }

            pub fn with_retry_backoff(mut self, backoff: Duration) -> Self {
                self.chat = self.chat.with_retry_backoff(backoff);
                self
            }

            pub fn with_retry_max_wait(mut self, max_wait: Duration) -> Self {
                self.chat = self.chat.with_retry_max_wait(max_wait);
                self
            }
        }

        #[async_trait]
        impl LlmProvider for $name {
            fn build(
                &self,
                messages: &[Message],
                tools: &[Arc<dyn Tool>],
            ) -> Result<Value, Box<dyn Error + Send + Sync>> {
                self.chat.build(messages, tools)
            }

            async fn send<'a>(
                &'a self,
                request: Value,
            ) -> Result<
                BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
                Box<dyn Error + Send + Sync>,
            > {
                self.chat.send(request).await
            }

            fn set_status_sink(&self, sink: Option<StatusSink>) {
                self.chat.set_status_sink(sink);
            }

            async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
                self.chat.list_models().await
            }
        }
    };
}

chat_service!(
    DeepSeekProvider,
    "https://api.deepseek.com/v1",
    "reasoning_content"
);
chat_service!(
    MoonshotProvider,
    "https://api.moonshot.ai/v1",
    "reasoning_content"
);
chat_service!(
    GlmProvider,
    "https://open.bigmodel.cn/api/paas/v4",
    "reasoning"
);

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::ReasoningReplay;

    #[test]
    fn mixed_model_history_is_normalized_per_message_across_switches() {
        let endpoint = "https://relay.example/v1";
        let assistant = |model: &str, origin: &str| Message::Assistant {
            content: Some(format!("{model} answer")),
            reasoning: Some(format!("{model} thought")),
            replay: Some(ReasoningReplay {
                origin: format!("chat:{origin}"),
                model: model.into(),
                field: Some("reasoning_content".into()),
                blocks: vec![],
            }),
            tool_calls: None,
        };
        let history = vec![
            assistant("glm", endpoint),
            assistant("deepseek", endpoint),
            assistant("deepseek", "https://other.example/v1"),
        ];
        for model in ["deepseek", "glm", "deepseek"] {
            let provider = DeepSeekProvider::new_with_endpoint("key", endpoint, model);
            let payload = provider.build(&history, &[]).unwrap();
            for (index, source) in ["glm", "deepseek", "deepseek"].iter().enumerate() {
                let message = &payload["messages"][index];
                if *source == model && index != 2 {
                    assert_eq!(message["reasoning_content"], format!("{source} thought"));
                } else {
                    assert!(message.get("reasoning_content").is_none());
                    assert!(
                        message["content"]
                            .as_str()
                            .unwrap()
                            .contains(&format!("{source} thought"))
                    );
                }
            }
        }
    }

    #[test]
    fn service_providers_apply_fixed_rules_through_trait_objects() {
        let endpoint = "https://example.com/v1";
        let providers: Vec<(Box<dyn LlmProvider>, &str)> = vec![
            (
                Box::new(DeepSeekProvider::new_with_endpoint(
                    "key", endpoint, "model",
                )),
                "reasoning_content",
            ),
            (
                Box::new(MoonshotProvider::new_with_endpoint(
                    "key", endpoint, "model",
                )),
                "reasoning_content",
            ),
            (
                Box::new(GlmProvider::new_with_endpoint("key", endpoint, "model")),
                "reasoning",
            ),
        ];
        for (provider, field) in providers {
            let messages = [
                Message::Assistant {
                    content: None,
                    reasoning: Some("consider tools".into()),
                    replay: Some(ReasoningReplay {
                        origin: format!("chat:{endpoint}"),
                        model: "model".into(),
                        field: Some("reasoning_text".into()),
                        blocks: vec![],
                    }),
                    tool_calls: None,
                },
                Message::Assistant {
                    content: Some("done".into()),
                    reasoning: None,
                    replay: None,
                    tool_calls: None,
                },
            ];
            let payload = provider.build(&messages, &[]).unwrap();
            assert_eq!(payload["messages"][0][field], "consider tools");
            assert!(payload["messages"][1].get(field).is_none());
            assert!(payload["messages"][0].get("reasoning_text").is_none());
        }
        // A model name must never change the OpenAI provider's service rules.
        let openai = OpenAIProvider::new("key", "deepseek-reasoner");
        let payload = openai
            .build(
                &[Message::Assistant {
                    content: Some("done".into()),
                    reasoning: None,
                    replay: None,
                    tool_calls: None,
                }],
                &[],
            )
            .unwrap();
        assert!(payload["messages"][0].get("reasoning_content").is_none());
    }
}
