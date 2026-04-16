//! OpenAI-compatible LLM provider implementation.
//!
//! This module provides an implementation of the [`LlmProvider`] trait
//! for OpenAI-compatible APIs. It supports both direct OpenAI API calls
//! and custom endpoints (e.g., for proxies or self-hosted models).
//!
//! # Features
//!
//! - Streaming responses via Server-Sent Events (SSE)
//! - Tool/function calling support
//! - Multi-modal content handling (images, audio, video, files)
//! - Automatic capability detection based on model name
//! - Reasoning content support (for models like o1)
//!
//! # Architecture
//!
//! The provider consists of several components:
//!
//! - [`OpenAIProvider`] - Main provider implementation
//! - [`RequestBuilder`] - Trait for building API requests
//! - [`ResponseParser`] - Trait for parsing streaming responses
//! - [`ToolCallAggregator`] - Handles fragmented tool call streaming

use crate::Client;
use crate::openai_types::{
    AssistantMessage, ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageArgs,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartFile,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionStream, ChatCompletionTool, ChatCompletionTools, CreateChatCompletionRequest,
    FileData, FunctionCall as OpenAIFunctionCall, FunctionObjectArgs, ImageUrl, OpenAIConfig,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Capabilities, ContentPart, Message, ProviderEvent};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

/// Trait for building API requests from GeWu types.
///
/// Implement this trait to create custom request formats for different
/// OpenAI-compatible APIs. The default implementation handles the
/// standard OpenAI request format.
pub trait RequestBuilder: Send + Sync {
    /// Builds a chat completion request.
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation messages to send.
    /// * `tools` - The available tools for function calling.
    fn build(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<CreateChatCompletionRequest, Box<dyn Error + Send + Sync>>;
}

/// Trait for parsing streaming responses into provider events.
///
/// Implement this trait to handle different response formats from
/// OpenAI-compatible APIs.
pub trait ResponseParser: Send + Sync {
    fn parse_stream(
        &self,
        stream: ChatCompletionStream,
    ) -> BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>;
}

/// OpenAI-compatible LLM provider.
///
/// This provider works with:
/// - OpenAI's official API
/// - OpenAI-compatible proxies
/// - Self-hosted models with OpenAI-compatible endpoints
///
/// # Model Capabilities
///
/// The provider automatically detects capabilities based on model name:
/// - **GPT-4o**: text + image input, PDF support
/// - **GPT-4-turbo**: text + image input, PDF support
/// - **GPT-3.5-turbo**: text-only
/// - **Other models**: text-only (conservative default)
///
/// # Example
///
/// ```ignore
/// use gewu_providers::OpenAIProvider;
///
/// // Direct OpenAI API
/// let provider = OpenAIProvider::new("api-key", "gpt-4");
///
/// // With custom endpoint (proxy)
/// let provider = OpenAIProvider::new_with_endpoint(
///     "api-key",
///     "https://my-proxy.com/v1",
///     "gpt-4"
/// );
/// ```
pub struct OpenAIProvider {
    client: Client,
    model: String,
    capabilities: Capabilities,
}

impl OpenAIProvider {
    /// Creates a new provider for the official OpenAI API.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your OpenAI API key.
    /// * `model` - The model to use (e.g., "gpt-4", "gpt-4-turbo").
    pub fn new(api_key: &str, model: &str) -> Self {
        let config = OpenAIConfig::new(api_key, "https://api.openai.com/v1");
        let capabilities = Self::default_capabilities_for_model(model);
        Self {
            client: Client::with_config(config),
            model: model.to_string(),
            capabilities,
        }
    }

    /// Creates a new provider with a custom endpoint.
    ///
    /// Use this for proxies, caching servers, or self-hosted models
    /// that are OpenAI-compatible.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your API key (may be ignored by some endpoints).
    /// * `base_url` - The base URL of the API (e.g., "https://api.openai.com/v1").
    /// * `model` - The model identifier expected by the endpoint.
    pub fn new_with_endpoint(api_key: &str, base_url: &str, model: &str) -> Self {
        let config = OpenAIConfig::new(api_key, base_url);
        let capabilities = Self::default_capabilities_for_model(model);
        Self {
            client: Client::with_config(config),
            model: model.to_string(),
            capabilities,
        }
    }

    /// Determines capabilities based on model name.
    ///
    /// This is a heuristic based on common model naming patterns.
    /// For accurate capability settings, use a custom provider implementation.
    fn default_capabilities_for_model(model: &str) -> Capabilities {
        let model_lower = model.to_lowercase();
        if model_lower.contains("gpt-4o") || model_lower.contains("gpt-5") {
            Capabilities {
                input: gewu_core::types::InputCapabilities {
                    text: true,
                    image: true,
                    audio: false,
                    video: false,
                    pdf: true,
                },
                output: gewu_core::types::OutputCapabilities {
                    text: true,
                    image: false,
                    audio: false,
                    video: false,
                    pdf: false,
                },
            }
        } else if model_lower.contains("gpt-4-turbo") {
            Capabilities {
                input: gewu_core::types::InputCapabilities {
                    text: true,
                    image: true,
                    audio: false,
                    video: false,
                    pdf: true,
                },
                output: gewu_core::types::OutputCapabilities {
                    text: true,
                    image: false,
                    audio: false,
                    video: false,
                    pdf: false,
                },
            }
        } else if model_lower.contains("gpt-3.5-turbo") {
            Capabilities {
                input: gewu_core::types::InputCapabilities {
                    text: true,
                    image: false,
                    audio: false,
                    video: false,
                    pdf: false,
                },
                output: gewu_core::types::OutputCapabilities {
                    text: true,
                    image: false,
                    audio: false,
                    video: false,
                    pdf: false,
                },
            }
        } else {
            Capabilities {
                input: gewu_core::types::InputCapabilities {
                    text: true,
                    image: false,
                    audio: false,
                    video: false,
                    pdf: false,
                },
                output: gewu_core::types::OutputCapabilities {
                    text: true,
                    image: false,
                    audio: false,
                    video: false,
                    pdf: false,
                },
            }
        }
    }

    /// Converts a [`ContentPart`] to an OpenAI API content part.
    ///
    /// Non-text content (image, audio, video, file) is converted to data URIs
    /// (base64-encoded inline data) for API compatibility.
    fn convert_content_part(
        &self,
        part: &ContentPart,
    ) -> Option<ChatCompletionRequestUserMessageContentPart> {
        match part {
            ContentPart::Text { text } => Some(ChatCompletionRequestUserMessageContentPart::Text(
                ChatCompletionRequestMessageContentPartText {
                    content_type: "text".to_string(),
                    text: text.clone(),
                },
            )),
            ContentPart::Image { data, mime_type } => {
                let url = format!("data:{};base64,{}", mime_type, data);
                Some(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                    ChatCompletionRequestMessageContentPartImage {
                        content_type: "image_url".to_string(),
                        image_url: ImageUrl {
                            url: url.into(),
                            detail: None,
                        },
                    },
                ))
            }
            ContentPart::Audio { data, mime_type } => {
                let url = format!("data:{};base64,{}", mime_type, data);
                Some(ChatCompletionRequestUserMessageContentPart::Text(
                    ChatCompletionRequestMessageContentPartText {
                        content_type: "text".to_string(),
                        text: format!("[Audio file: {}]({})", mime_type, url),
                    },
                ))
            }
            ContentPart::Video { data, mime_type } => {
                let url = format!("data:{};base64,{}", mime_type, data);
                Some(ChatCompletionRequestUserMessageContentPart::Text(
                    ChatCompletionRequestMessageContentPartText {
                        content_type: "text".to_string(),
                        text: format!("[Video file: {}]({})", mime_type, url),
                    },
                ))
            }
            ContentPart::File { data, mime_type } => {
                if mime_type == "application/pdf" && self.capabilities.input.pdf {
                    Some(ChatCompletionRequestUserMessageContentPart::File(
                        ChatCompletionRequestMessageContentPartFile {
                            content_type: "file".to_string(),
                            file: FileData {
                                filename: Some("document.pdf".to_string()),
                                file_data: Some(data.clone()),
                                file_id: None,
                            },
                        },
                    ))
                } else {
                    let url = format!("data:{};base64,{}", mime_type, data);
                    Some(ChatCompletionRequestUserMessageContentPart::Text(
                        ChatCompletionRequestMessageContentPartText {
                            content_type: "text".to_string(),
                            text: format!("[File: {}]({})", mime_type, url),
                        },
                    ))
                }
            }
        }
    }

    /// Checks if a content part type is supported by the current model.
    #[allow(dead_code)]
    fn modality_supported(&self, part: &ContentPart) -> bool {
        match part {
            ContentPart::Text { .. } => true,
            ContentPart::Image { .. } => self.capabilities.input.image,
            ContentPart::Audio { .. } => self.capabilities.input.audio,
            ContentPart::Video { .. } => self.capabilities.input.video,
            ContentPart::File { mime_type, .. } => {
                if mime_type == "application/pdf" {
                    self.capabilities.input.pdf
                } else {
                    self.capabilities.input.text
                }
            }
        }
    }

    /// Converts a [`Message`] to OpenAI API message format.
    ///
    /// Tool messages may produce multiple API messages: one for the tool result
    /// and optionally an additional user message for media attachments.
    fn convert_message(
        &self,
        msg: &Message,
    ) -> Result<Vec<ChatCompletionRequestMessage>, Box<dyn Error + Send + Sync>> {
        match msg {
            Message::System { content } => {
                let m = ChatCompletionRequestSystemMessageArgs::default()
                    .content(content.clone())
                    .build()
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                Ok(vec![ChatCompletionRequestMessage::System(m)])
            }
            Message::User { content } => {
                let parts: Vec<ChatCompletionRequestUserMessageContentPart> = content
                    .iter()
                    .filter_map(|p| self.convert_content_part(p))
                    .collect();
                let m = ChatCompletionRequestUserMessageArgs::default()
                    .content(parts)
                    .build()
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                Ok(vec![ChatCompletionRequestMessage::User(m)])
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let openai_calls = tool_calls.clone().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| {
                            ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: tc.id.clone(),
                                    tool_type: "function".to_string(),
                                    function: OpenAIFunctionCall {
                                        name: tc.function.name.clone(),
                                        arguments: tc.function.arguments.clone(),
                                    },
                                },
                            )
                        })
                        .collect()
                });

                Ok(vec![ChatCompletionRequestMessage::Assistant(
                    AssistantMessage {
                        content: content.clone(),
                        tool_calls: openai_calls,
                    },
                )])
            }
            Message::Tool {
                tool_call_id,
                tool_name,
                content,
                media,
            } => {
                let m = ChatCompletionRequestToolMessageArgs::default()
                    .tool_call_id(tool_call_id.clone())
                    .content(content.clone())
                    .build()
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                let mut messages = vec![ChatCompletionRequestMessage::Tool(m)];

                if !media.is_empty() {
                    let mut parts: Vec<ChatCompletionRequestUserMessageContentPart> = Vec::new();
                    let label = match tool_name {
                        Some(name) => format!("Tool '{}' returned additional media:", name),
                        None => "Tool returned additional media:".to_string(),
                    };
                    parts.push(ChatCompletionRequestUserMessageContentPart::Text(
                        ChatCompletionRequestMessageContentPartText {
                            content_type: "text".to_string(),
                            text: label,
                        },
                    ));
                    for part in media {
                        if let Some(converted) = self.convert_content_part(part) {
                            parts.push(converted);
                        } else {
                            let unsupported_label = format!(
                                "[Unsupported media type: {} - this model does not support {} input]",
                                part.mime_type(),
                                part.mime_type()
                            );
                            parts.push(ChatCompletionRequestUserMessageContentPart::Text(
                                ChatCompletionRequestMessageContentPartText {
                                    content_type: "text".to_string(),
                                    text: unsupported_label,
                                },
                            ));
                        }
                    }
                    let extra = ChatCompletionRequestUserMessageArgs::default()
                        .content(parts)
                        .build()
                        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                    messages.push(ChatCompletionRequestMessage::User(extra));
                }

                Ok(messages)
            }
        }
    }

    /// Converts tool definitions to OpenAI tools format.
    fn convert_tools(
        &self,
        tools: &[Arc<dyn Tool>],
    ) -> Result<Vec<ChatCompletionTools>, Box<dyn Error + Send + Sync>> {
        let mut openai_tools: Vec<ChatCompletionTools> = Vec::new();
        for tool in tools {
            let schema = tool.schema();
            let parameters = serde_json::to_value(&schema)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
            let function = FunctionObjectArgs::default()
                .name(tool.name())
                .description(tool.description())
                .parameters(parameters)
                .build()
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

            let t = ChatCompletionTools::Function(ChatCompletionTool {
                tool_type: "function".to_string(),
                function,
            });
            openai_tools.push(t);
        }
        Ok(openai_tools)
    }
}

impl RequestBuilder for OpenAIProvider {
    fn build(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<CreateChatCompletionRequest, Box<dyn Error + Send + Sync>> {
        let mut openai_messages: Vec<ChatCompletionRequestMessage> = Vec::new();
        for msg in messages {
            let mut converted = self.convert_message(msg)?;
            openai_messages.append(&mut converted);
        }

        let openai_tools = self.convert_tools(tools)?;

        Ok(CreateChatCompletionRequest {
            model: self.model.clone(),
            messages: openai_messages,
            tools: if openai_tools.is_empty() {
                None
            } else {
                Some(openai_tools)
            },
            stream: None,
            max_tokens: None,
            temperature: None,
        })
    }
}

/// Internal state for tracking tool call argument streaming.
///
/// The OpenAI API may stream tool call arguments in multiple chunks.
/// This struct tracks the state of each in-progress tool call to properly
/// reassemble the arguments and emit events when both the ID and
/// arguments are available.
#[derive(Debug, Default, Clone)]
struct ToolCallState {
    id: Option<String>,
    name: Option<String>,
    argument_buffer: String,
    sent_len: usize,
    started: bool,
    finalized: bool,
}

/// Aggregates fragmented tool call events from streaming responses.
///
/// When the LLM generates tool calls, the API may stream:
/// 1. The tool call ID
/// 2. The function name
/// 3. The arguments (potentially in many small chunks)
///
/// This aggregator buffers arguments until:
/// - The tool call ID is received (required before emitting events)
/// - A valid JSON object is complete (to avoid emitting malformed arguments)
#[derive(Default)]
struct ToolCallAggregator {
    states: Vec<ToolCallState>,
}

impl ToolCallAggregator {
    /// Processes a single tool call chunk from the stream.
    ///
    /// May emit multiple [`ProviderEvent`] events:
    /// - [`ProviderEvent::ToolCallStart`] when both ID and name are available
    /// - [`ProviderEvent::ToolCallDelta`] when new argument content arrives
    fn process_chunk(&mut self, chunk: &ChatCompletionMessageToolCallChunk) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let index = chunk.index as usize;

        if index >= self.states.len() {
            self.states.resize_with(index + 1, ToolCallState::default);
        }

        let state = &mut self.states[index];

        if let Some(id) = &chunk.id {
            if !id.is_empty() {
                state.id = Some(id.clone());
            }
        }

        if let Some(function) = &chunk.function {
            if let Some(name) = &function.name {
                if !name.is_empty() {
                    state.name = Some(name.clone());
                }
            }

            if let Some(args) = &function.arguments {
                if !args.is_empty() && !state.finalized {
                    state.argument_buffer.push_str(args);
                    Self::update_finalized_flag(state);
                    if state.started {
                        if let Some(id) = state.id.clone() {
                            Self::emit_pending_arguments(state, &id, &mut events);
                        }
                    }
                }
            }
        }

        if !state.started {
            if let (Some(id), Some(name)) = (state.id.clone(), state.name.clone()) {
                events.push(ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name,
                });
                state.started = true;

                Self::emit_pending_arguments(state, &id, &mut events);
            }
        }

        events
    }

    /// Emits pending argument deltas to the events vector.
    fn emit_pending_arguments(
        state: &mut ToolCallState,
        id: &str,
        events: &mut Vec<ProviderEvent>,
    ) {
        let new_len = state.argument_buffer.len();
        if new_len <= state.sent_len {
            return;
        }

        let delta = state.argument_buffer[state.sent_len..new_len].to_string();
        state.sent_len = new_len;
        if delta.is_empty() {
            return;
        }

        events.push(ProviderEvent::ToolCallDelta {
            id: id.to_string(),
            delta,
        });
    }

    /// Marks the state as finalized when a complete JSON object is buffered.
    fn update_finalized_flag(state: &mut ToolCallState) {
        if state.finalized {
            return;
        }

        if serde_json::from_str::<Value>(&state.argument_buffer).is_ok() {
            state.finalized = true;
        }
    }
}

impl ResponseParser for OpenAIProvider {
    fn parse_stream(
        &self,
        stream: ChatCompletionStream,
    ) -> BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>> {
        let s = async_stream::try_stream! {
            let mut tool_aggregator = ToolCallAggregator::default();

            let mut stream = stream;
            while let Some(result) = stream.next().await {
                let response = result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                if let Some(choice) = response.choices.first() {
                    let delta = &choice.delta;

                    if let Some(reasoning) = &delta.reasoning_content {
                        if !reasoning.is_empty() {
                            yield ProviderEvent::ReasoningDelta { delta: reasoning.clone() };
                        }
                    }

                    if let Some(content) = &delta.content {
                        if !content.is_empty() {
                            yield ProviderEvent::ContentDelta { delta: content.clone() };
                        }
                    }

                    if let Some(tool_calls) = &delta.tool_calls {
                        for tool_call in tool_calls {
                            for event in tool_aggregator.process_chunk(tool_call) {
                                yield event;
                            }
                        }
                    }

                    if let Some(reason) = &choice.finish_reason {
                        yield ProviderEvent::Finish {
                            reason: Some(format!("{:?}", reason)),
                        };
                    }
                }
            }
        };

        Box::pin(s)
    }
}

#[async_trait]
impl LlmProvider for OpenAIProvider {
    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let request = self.build(messages, tools)?;

        let stream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        let parsed_stream = self.parse_stream(stream);

        Ok(parsed_stream)
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        let models = self.client.models();
        let response = models
            .list()
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
        Ok(response.data.into_iter().map(|m| m.id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_types::FunctionCallStream;

    #[test]
    fn test_new_provider() {
        let api_key = "test-key";
        let model = "gpt-4";
        let provider = OpenAIProvider::new(api_key, model);

        assert!(provider.build(&[], &[]).is_ok());
    }

    #[test]
    fn test_new_provider_with_endpoint() {
        let api_key = "test-key";
        let base_url = "http://localhost:8080/v1";
        let model = "gpt-3.5-turbo";
        let provider = OpenAIProvider::new_with_endpoint(api_key, base_url, model);

        assert!(provider.build(&[], &[]).is_ok());
    }

    #[test]
    fn convert_message_emits_media_followup() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let msg = Message::Tool {
            tool_call_id: "call_1".to_string(),
            tool_name: Some("read_file".to_string()),
            content: "{\"type\":\"media\"}".to_string(),
            media: vec![
                ContentPart::Text {
                    text: "Image read successfully".to_string(),
                },
                ContentPart::Image {
                    data: "ZGF0YQ==".to_string(),
                    mime_type: "image/png".to_string(),
                },
            ],
        };

        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 2);
        match &converted[0] {
            ChatCompletionRequestMessage::Tool(tool_msg) => {
                assert_eq!(tool_msg.tool_call_id.as_str(), "call_1");
            }
            _ => panic!("first message should be tool"),
        }
        match &converted[1] {
            ChatCompletionRequestMessage::User(_) => {}
            _ => panic!("second message should be user"),
        }
    }

    fn make_chunk(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ChatCompletionMessageToolCallChunk {
        ChatCompletionMessageToolCallChunk {
            index,
            id: id.map(|s| s.to_string()),
            function_type: None,
            function: Some(FunctionCallStream {
                name: name.map(|s| s.to_string()),
                arguments: arguments.map(|s| s.to_string()),
            }),
        }
    }

    #[test]
    fn tool_call_arguments_buffer_until_id_arrives() {
        let mut aggregator = ToolCallAggregator::default();

        assert!(
            aggregator
                .process_chunk(&make_chunk(0, None, Some("demo_tool"), Some("{\"foo\":")))
                .is_empty()
        );
        assert!(
            aggregator
                .process_chunk(&make_chunk(0, None, Some("demo_tool"), Some("\"bar\"")))
                .is_empty()
        );

        let events =
            aggregator.process_chunk(&make_chunk(0, Some("call_1"), Some("demo_tool"), None));
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ProviderEvent::ToolCallStart { .. }));
        assert!(matches!(events[1], ProviderEvent::ToolCallDelta { .. }));
        if let ProviderEvent::ToolCallDelta { id, delta } = &events[1] {
            assert_eq!(id, "call_1");
            assert_eq!(delta, "{\"foo\":\"bar\"");
        } else {
            panic!("Expected ToolCallDelta event");
        }

        let events = aggregator.process_chunk(&make_chunk(0, None, None, Some("\"baz\"}")));
        assert_eq!(events.len(), 1);
        if let ProviderEvent::ToolCallDelta { id, delta } = &events[0] {
            assert_eq!(id, "call_1");
            assert_eq!(delta, "\"baz\"}");
        } else {
            panic!("Expected ToolCallDelta event");
        }
    }

    #[test]
    fn duplicate_arguments_after_completion_are_ignored() {
        let mut aggregator = ToolCallAggregator::default();

        assert!(
            aggregator
                .process_chunk(&make_chunk(
                    0,
                    None,
                    Some("glob"),
                    Some("{\"pattern\":\"*\"}")
                ))
                .is_empty()
        );

        let events = aggregator.process_chunk(&make_chunk(0, Some("call_42"), Some("glob"), None));
        assert_eq!(events.len(), 2);

        let events = aggregator.process_chunk(&make_chunk(
            0,
            Some("call_42"),
            Some("glob"),
            Some("{\"pattern\": \"*\"}"),
        ));
        assert!(
            events.is_empty(),
            "Duplicate arguments should not emit new deltas"
        );
    }
}
