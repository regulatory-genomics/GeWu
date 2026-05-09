//! LLM provider implementations.

pub mod anthropic;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAIProvider;
use futures::stream::BoxStream;
use gewu_core::traits::Tool;
use gewu_core::types::{Message, ProviderEvent};
use std::error::Error;
use std::sync::Arc;

pub trait RequestBuilder: Send + Sync {
    fn build(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<crate::openai::CreateChatCompletionRequest, Box<dyn Error + Send + Sync>>;
}

pub trait ResponseParser: Send + Sync {
    fn parse_stream(
        &self,
        stream: crate::openai::ChatCompletionStream,
    ) -> BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>;
}
