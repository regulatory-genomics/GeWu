//! LLM provider implementations.
//!
//! This module provides implementations of the `LlmProvider` trait (from `gewu_core`)
//! for various LLM APIs. Currently supports OpenAI-compatible APIs
//! and Anthropic's Claude API.
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
//! - [`OpenAIProvider`] - OpenAI-compatible provider implementation
//! - [`AnthropicProvider`] - Anthropic Claude provider implementation
//! - [`ResponseParser`] - Trait for parsing streaming responses

use crate::openai::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartImage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionTools, ChatCompletionStream, ChatCompletionTool,
    CreateChatCompletionRequest, FunctionCall as OpenAIFunctionCall,
    FunctionObjectArgs, ImageUrl, OpenAIConfig, AssistantMessage as OpenAIAssistantMessage,
    ChatCompletionRequestMessageContentPartText, FileData,
    ChatCompletionRequestMessageContentPartFile, OpenAIClient,
};
use crate::anthropic::{
    AnthropicChunk, AnthropicConfig, AnthropicMessage, AnthropicStream, AnthropicTool,
    ContentBlock, ContentBlockDeltaChunk, ContentBlockStart,
    ContentBlockStartChunk, ContentDelta, CreateMessageRequest,
    MessageStartChunk, UserMessage,
    AssistantMessage as AnthropicAssistantMessage, Client as AnthropicClient,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Capabilities, ContentPart, Message, ProviderEvent};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

static MULTIMODAL_CAP: Capabilities = Capabilities {
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
};

static TEXT_ONLY_CAP: Capabilities = Capabilities {
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
};

/// Trait for parsing streaming responses into provider events.
///
/// Implement this trait to handle different response formats from
/// various LLM APIs, converting them into GeWu's [`ProviderEvent`] stream.
pub trait ResponseParser: Send + Sync {
    /// Parses a provider-specific stream into a stream of [`ProviderEvent`]s.
    ///
    /// # Arguments
    ///
    /// * `stream` - The provider's streaming response.
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
    client: OpenAIClient,
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
            client: OpenAIClient::with_config(config),
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
    /// * `base_url` - The base URL of the API (e.g., <https://api.openai.com/v1>).
    /// * `model` - The model identifier expected by the endpoint.
    pub fn new_with_endpoint(api_key: &str, base_url: &str, model: &str) -> Self {
        let config = OpenAIConfig::new(api_key, base_url);
        let capabilities = Self::default_capabilities_for_model(model);
        Self {
            client: OpenAIClient::with_config(config),
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

        static MODEL_RULES: &[(&[&str], &Capabilities)] = &[
            (&["gpt-4o", "gpt-5"], &MULTIMODAL_CAP),
            (&["gpt-4-turbo"], &MULTIMODAL_CAP),
            (&["gpt-3.5-turbo"], &TEXT_ONLY_CAP),
        ];

        MODEL_RULES
            .iter()
            .filter(|(patterns, _)| patterns.iter().any(|p| model_lower.contains(p)))
            .map(|(_, caps)| *caps)
            .next()
            .unwrap_or(&TEXT_ONLY_CAP)
            .clone()
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
                            ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                                id: tc.id.clone(),
                                tool_type: "function".to_string(),
                                function: OpenAIFunctionCall {
                                    name: tc.function.name.clone(),
                                    arguments: tc.function.arguments.clone(),
                                },
                            })
                        })
                        .collect()
                });

                Ok(vec![ChatCompletionRequestMessage::Assistant(
                    OpenAIAssistantMessage {
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

impl OpenAIProvider {
    /// Builds the typed OpenAI chat completion request from core messages and tools.
    fn build_typed(
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
            tools: if openai_tools.is_empty() { None } else { Some(openai_tools) },
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
    fn build(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let request = self.build_typed(messages, tools)?;
        serde_json::to_value(&request).map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }

    async fn send<'a>(
        &'a self,
        request: serde_json::Value,
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let request: CreateChatCompletionRequest = serde_json::from_value(request)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

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

/// Anthropic Claude API provider.
///
/// This provider works with:
/// - Anthropic's official Claude API
/// - Anthropic-compatible proxies
///
/// # Model Capabilities
///
/// The provider automatically detects capabilities based on model name:
/// - **Claude 3.5 Sonnet / Claude 3 Opus**: text + image input, PDF support
/// - **Claude 3 Haiku**: text-only
/// - **Other models**: text + image input (conservative default)
///
/// # Extended Thinking
///
/// For models that support extended thinking (like Claude 3.7), use
/// [`with_extended_thinking`](AnthropicProvider::with_extended_thinking):
///
/// ```ignore
/// let provider = AnthropicProvider::new("api-key", "claude-3-7-sonnet")
///     .with_extended_thinking();
/// ```
pub struct AnthropicProvider {
    client: AnthropicClient,
    model: String,
    #[allow(dead_code)]
    capabilities: Capabilities,
}

impl AnthropicProvider {
    /// Creates a new provider for the official Anthropic API.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your Anthropic API key.
    /// * `model` - The model to use (e.g., "claude-3-5-sonnet-20241022").
    pub fn new(api_key: &str, model: &str) -> Self {
        let config = AnthropicConfig::new(api_key, "https://api.anthropic.com");
        let capabilities = Self::default_capabilities_for_model(model);
        Self {
            client: AnthropicClient::with_config(config),
            model: model.to_string(),
            capabilities,
        }
    }

    /// Creates a new provider with a custom endpoint.
    ///
    /// Use this for proxies, caching servers, or self-hosted models
    /// that are Anthropic-compatible.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your API key (may be ignored by some endpoints).
    /// * `base_url` - The base URL of the API (e.g., <https://api.anthropic.com>).
    /// * `model` - The model identifier expected by the endpoint.
    pub fn new_with_endpoint(api_key: &str, base_url: &str, model: &str) -> Self {
        let config = AnthropicConfig::new(api_key, base_url);
        let capabilities = Self::default_capabilities_for_model(model);
        Self {
            client: AnthropicClient::with_config(config),
            model: model.to_string(),
            capabilities,
        }
    }

    /// Enables extended thinking for supported models.
    ///
    /// Extended thinking allows models like Claude 3.7 to show their reasoning process.
    /// This is a builder-style method that returns a new provider instance.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = AnthropicProvider::new("api-key", "claude-3-7-sonnet")
    ///     .with_extended_thinking();
    /// ```
    pub fn with_extended_thinking(mut self) -> Self {
        self.client.config.beta = Some("thinking-2025-05-14".to_string());
        self
    }

    fn default_capabilities_for_model(model: &str) -> Capabilities {
        let model_lower = model.to_lowercase();

        static MODEL_RULES: &[(&[&str], &Capabilities)] = &[
            (&["claude-3-5-sonnet", "claude-3-opus"], &MULTIMODAL_CAP),
            (&["claude-3-haiku"], &TEXT_ONLY_CAP),
        ];

        MODEL_RULES
            .iter()
            .filter(|(patterns, _)| patterns.iter().any(|p| model_lower.contains(p)))
            .map(|(_, caps)| *caps)
            .next()
            .unwrap_or(&MULTIMODAL_CAP)
            .clone()
    }

    fn convert_content_part(&self, part: &ContentPart) -> Option<ContentBlock> {
        match part {
            ContentPart::Text { text } => Some(ContentBlock::Text {
                text: text.clone(),
            }),
            ContentPart::Image { data, mime_type } => Some(ContentBlock::Text {
                text: format!("[Image: {}](data:{};base64,{})", mime_type, mime_type, data),
            }),
            ContentPart::Audio { data, mime_type } => Some(ContentBlock::Text {
                text: format!("[Audio: {}](data:{};base64,{})", mime_type, mime_type, data),
            }),
            ContentPart::Video { data, mime_type } => Some(ContentBlock::Text {
                text: format!("[Video: {}](data:{};base64,{})", mime_type, mime_type, data),
            }),
            ContentPart::File { data, mime_type } => {
                Some(ContentBlock::Text {
                    text: format!("[File: {}](data:{};base64,{})", mime_type, mime_type, data),
                })
            }
        }
    }

    fn convert_message(
        &self,
        msg: &Message,
    ) -> Result<Vec<AnthropicMessage>, Box<dyn Error + Send + Sync>> {
        match msg {
            Message::System { content: _ } => Ok(vec![]),
            Message::User { content } => {
                let blocks: Vec<ContentBlock> = content
                    .iter()
                    .filter_map(|p| self.convert_content_part(p))
                    .collect();
                Ok(vec![AnthropicMessage::User(UserMessage { content: blocks })])
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if let Some(text) = content {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text: text.clone() });
                    }
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        let input: Value = serde_json::from_str(&call.function.arguments).map_err(|e| {
                            format!("Invalid arguments for Anthropic tool call '{}': {}", call.function.name, e)
                        })?;
                        blocks.push(ContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            input,
                        });
                    }
                }
                if blocks.is_empty() {
                    Ok(vec![])
                } else {
                    Ok(vec![AnthropicMessage::Assistant(AnthropicAssistantMessage {
                        content: Some(blocks),
                    })])
                }
            }
            Message::Tool {
                tool_call_id,
                tool_name: _,
                content,
                media,
            } => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: content.clone(),
                    is_error: None,
                });
                for part in media {
                    if let Some(block) = self.convert_content_part(part) {
                        blocks.push(block);
                    }
                }
                Ok(vec![AnthropicMessage::User(UserMessage { content: blocks })])
            }
        }
    }

    fn convert_tools(
        &self,
        tools: &[Arc<dyn Tool>],
    ) -> Result<Vec<AnthropicTool>, Box<dyn Error + Send + Sync>> {
        let mut anthropic_tools: Vec<AnthropicTool> = Vec::new();
        for tool in tools {
            let schema = tool.schema();
            let parameters = serde_json::to_value(&schema)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
            anthropic_tools.push(AnthropicTool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: parameters,
            });
        }
        Ok(anthropic_tools)
    }

    /// Builds the typed Anthropic message request from core messages and tools.
    fn build_typed(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<CreateMessageRequest, Box<dyn Error + Send + Sync>> {
        let mut anthropic_messages: Vec<AnthropicMessage> = Vec::new();
        let mut system_prompt = String::new();

        for msg in messages {
            if let Message::System { content } = msg {
                if !system_prompt.is_empty() {
                    system_prompt.push('\n');
                }
                system_prompt.push_str(content);
            } else {
                let converted = self.convert_message(msg)?;
                anthropic_messages.extend(converted);
            }
        }

        let anthropic_tools = self.convert_tools(tools)?;

        Ok(CreateMessageRequest {
            model: self.model.clone(),
            messages: anthropic_messages,
            system: if system_prompt.is_empty() {
                None
            } else {
                Some(system_prompt)
            },
            tools: if anthropic_tools.is_empty() {
                None
            } else {
                Some(anthropic_tools)
            },
            stream: true,
            max_tokens: Some(8192),
        })
    }
}

#[derive(Default)]
struct AnthropicToolCallAggregator {
    states: Vec<ToolCallState>,
}

impl AnthropicToolCallAggregator {
    fn process_chunk(&mut self, chunk: &ContentBlockStartChunk, index: u32) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let idx = index as usize;

        if idx >= self.states.len() {
            self.states.resize_with(idx + 1, ToolCallState::default);
        }

        match &chunk.content_block {
            ContentBlockStart::ToolUse { id, name } => {
                let state = &mut self.states[idx];
                state.id = Some(id.clone());
                state.name = Some(name.clone());
                state.started = true;
                events.push(ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                });
            }
            ContentBlockStart::Thinking { thinking, .. } => {
                if !thinking.is_empty() {
                    events.push(ProviderEvent::ReasoningDelta {
                        delta: thinking.clone(),
                    });
                }
            }
            ContentBlockStart::Text { .. } => {}
        }

        events
    }

    fn process_delta(&mut self, chunk: &ContentBlockDeltaChunk, index: u32) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let idx = index as usize;

        if idx >= self.states.len() {
            return events;
        }

        let state = &mut self.states[idx];

        match &chunk.delta {
            ContentDelta::InputJsonDelta { partial_json } => {
                if !state.started {
                    return events;
                }
                state.argument_buffer.push_str(partial_json);
                if let Some(id) = state.id.clone() {
                    let new_len = state.argument_buffer.len();
                    if new_len > state.sent_len {
                        let delta = state.argument_buffer[state.sent_len..new_len].to_string();
                        state.sent_len = new_len;
                        if !delta.is_empty() {
                            events.push(ProviderEvent::ToolCallDelta {
                                id,
                                delta,
                            });
                        }
                    }
                }
            }
            ContentDelta::ThinkingDelta { thinking } => {
                if !thinking.is_empty() {
                    events.push(ProviderEvent::ReasoningDelta {
                        delta: thinking.clone(),
                    });
                }
            }
            ContentDelta::TextDelta { .. } => {}
        }

        events
    }
}

impl AnthropicProvider {
    fn parse_stream(
        &self,
        stream: AnthropicStream,
    ) -> futures::stream::BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>> {
        let s = async_stream::try_stream! {
            let mut tool_aggregator = AnthropicToolCallAggregator::default();
            let mut stream = stream;

            while let Some(result) = stream.next().await {
                let chunk = result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

                match chunk {
                    AnthropicChunk::MessageStart(MessageStartChunk { .. }) => {}
                    AnthropicChunk::ContentBlockStart(chunk) => {
                        for event in tool_aggregator.process_chunk(&chunk, chunk.index) {
                            yield event;
                        }
                    }
                    AnthropicChunk::ContentBlockDelta(chunk) => {
                        for event in tool_aggregator.process_delta(&chunk, chunk.index) {
                            yield event;
                        }
                        match &chunk.delta {
                            ContentDelta::TextDelta { text } => {
                                if !text.is_empty() {
                                    yield ProviderEvent::ContentDelta { delta: text.clone() };
                                }
                            }
                            ContentDelta::ThinkingDelta { thinking } => {
                                if !thinking.is_empty() {
                                    yield ProviderEvent::ReasoningDelta { delta: thinking.clone() };
                                }
                            }
                            ContentDelta::InputJsonDelta { .. } => {}
                        }
                    }
                    AnthropicChunk::ContentBlockStop(_) => {}
                    AnthropicChunk::MessageDelta(chunk) => {
                        if let Some(reason) = chunk.delta.stop_reason {
                            yield ProviderEvent::Finish { reason: Some(reason) };
                        }
                    }
                    AnthropicChunk::MessageStop => {}
                    AnthropicChunk::Error(e) => {
                        yield ProviderEvent::Error { error: e.error.message };
                    }
                }
            }
        };

        Box::pin(s)
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn build(
        &self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let request = self.build_typed(messages, tools)?;
        serde_json::to_value(&request).map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }

    async fn send<'a>(
        &'a self,
        request: serde_json::Value,
    ) -> Result<
        futures::stream::BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let request: CreateMessageRequest = serde_json::from_value(request)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        let stream = self
            .client
            .messages()
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
    use crate::openai::FunctionCallStream;
    use async_trait::async_trait;
    use gewu_core::types::{FunctionCall as CoreFunctionCall, ToolCall as CoreToolCall};
    use gewu_core::{ToolContext, ToolResult, Tool};
    use schemars::schema::RootSchema;
    use serde_json::Value;

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

    #[test]
    fn openai_default_capabilities_gpt4o() {
        let caps = OpenAIProvider::default_capabilities_for_model("gpt-4o");
        assert!(caps.input.image);
        assert!(caps.input.pdf);
        assert!(!caps.input.audio);
        assert!(!caps.input.video);
    }

    #[test]
    fn openai_default_capabilities_gpt35() {
        let caps = OpenAIProvider::default_capabilities_for_model("gpt-3.5-turbo");
        assert!(!caps.input.image);
        assert!(!caps.input.pdf);
        assert!(caps.input.text);
    }

    #[test]
    fn openai_default_capabilities_unknown() {
        let caps = OpenAIProvider::default_capabilities_for_model("unknown-model");
        assert!(caps.input.text);
        assert!(!caps.input.image);
    }

    #[test]
    fn convert_message_system() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let msg = Message::System {
            content: "You are helpful".to_string(),
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 1);
        assert!(matches!(
            converted[0],
            ChatCompletionRequestMessage::System(_)
        ));
    }

    #[test]
    fn convert_message_user_text() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let msg = Message::User {
            content: vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 1);
        assert!(matches!(
            converted[0],
            ChatCompletionRequestMessage::User(_)
        ));
    }

    #[test]
    fn convert_message_assistant_with_content() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let msg = Message::Assistant {
            content: Some("Hello world".to_string()),
            tool_calls: None,
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            ChatCompletionRequestMessage::Assistant(asm) => {
                assert_eq!(asm.content.as_deref(), Some("Hello world"));
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn convert_message_assistant_with_tool_calls() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let msg = Message::Assistant {
            content: None,
            tool_calls: Some(vec![CoreToolCall {
                id: "call_1".to_string(),
                r#type: "function".to_string(),
                function: CoreFunctionCall {
                    name: "get_weather".to_string(),
                    arguments: "{\"city\":\"NYC\"}".to_string(),
                },
            }]),
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            ChatCompletionRequestMessage::Assistant(asm) => {
                assert!(asm.tool_calls.is_some());
                let calls = asm.tool_calls.as_ref().unwrap();
                assert_eq!(calls.len(), 1);
                match &calls[0] {
                    ChatCompletionMessageToolCalls::Function(f) => {
                        assert_eq!(f.function.name, "get_weather");
                    }
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn convert_message_tool_simple() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let msg = Message::Tool {
            tool_call_id: "call_1".to_string(),
            tool_name: Some("get_weather".to_string()),
            content: "{\"temp\":72}".to_string(),
            media: vec![],
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            ChatCompletionRequestMessage::Tool(tm) => {
                assert_eq!(tm.tool_call_id, "call_1");
                assert_eq!(tm.content, "{\"temp\":72}");
            }
            _ => panic!("expected Tool message"),
        }
    }

    #[test]
    fn anthropic_default_capabilities_sonnet() {
        let caps = AnthropicProvider::default_capabilities_for_model("claude-3-5-sonnet");
        assert!(caps.input.image);
        assert!(caps.input.pdf);
        assert!(!caps.input.audio);
    }

    #[test]
    fn anthropic_default_capabilities_haiku() {
        let caps = AnthropicProvider::default_capabilities_for_model("claude-3-haiku");
        assert!(!caps.input.image);
        assert!(caps.input.text);
    }

    #[test]
    fn anthropic_default_capabilities_unknown() {
        let caps = AnthropicProvider::default_capabilities_for_model("unknown-model");
        assert!(caps.input.image);
        assert!(caps.input.pdf);
    }

    #[test]
    fn anthropic_convert_message_user() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let msg = Message::User {
            content: vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert_eq!(converted.len(), 1);
    }

    #[test]
    fn anthropic_convert_message_assistant_with_tool_use() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let msg = Message::Assistant {
            content: None,
            tool_calls: Some(vec![CoreToolCall {
                id: "toolu_1".to_string(),
                r#type: "function".to_string(),
                function: CoreFunctionCall {
                    name: "websearch".to_string(),
                    arguments: r#"{"query":"Kai Zhang"}"#.to_string(),
                },
            }]),
        };

        let converted = provider.convert_message(&msg).expect("convert success");
        match &converted[0] {
            AnthropicMessage::Assistant(message) => match message.content.as_ref().unwrap().first() {
                Some(ContentBlock::ToolUse { id, name, input }) => {
                    assert_eq!(id, "toolu_1");
                    assert_eq!(name, "websearch");
                    assert_eq!(input["query"], "Kai Zhang");
                }
                _ => panic!("expected Anthropic tool_use block"),
            },
            _ => panic!("expected Anthropic assistant message"),
        }
    }

    #[test]
    fn anthropic_convert_message_tool_result() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let msg = Message::Tool {
            tool_call_id: "toolu_1".to_string(),
            tool_name: Some("websearch".to_string()),
            content: "Search results".to_string(),
            media: vec![],
        };

        let converted = provider.convert_message(&msg).expect("convert success");
        match &converted[0] {
            AnthropicMessage::User(message) => match message.content.first() {
                Some(ContentBlock::ToolResult { tool_use_id, content, is_error }) => {
                    assert_eq!(tool_use_id, "toolu_1");
                    assert_eq!(content, "Search results");
                    assert_eq!(*is_error, None);
                }
                _ => panic!("expected Anthropic tool_result block"),
            },
            _ => panic!("expected Anthropic user message"),
        }
    }

    #[test]
    fn anthropic_convert_message_system_returns_empty() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let msg = Message::System {
            content: "You are helpful".to_string(),
        };
        let converted = provider.convert_message(&msg).expect("convert success");
        assert!(converted.is_empty(), "System messages should return empty for Anthropic");
    }

    struct MockTool {
        name: String,
        description: String,
    }

    impl MockTool {
        fn new(name: &str, description: &str) -> Self {
            Self {
                name: name.to_string(),
                description: description.to_string(),
            }
        }
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.description
        }
        fn schema(&self) -> RootSchema {
            schemars::schema_for!(serde_json::Value)
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
            Ok(ToolResult::new(&self.name, "ok"))
        }
    }

    #[test]
    fn openai_convert_tools() {
        let provider = OpenAIProvider::new("test-key", "gpt-4");
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool::new("get_weather", "Get weather for a city")),
            Arc::new(MockTool::new("search", "Search the web")),
        ];
        let converted = provider.convert_tools(&tools).expect("convert success");
        assert_eq!(converted.len(), 2);
        match &converted[0] {
            ChatCompletionTools::Function(tf) => {
                assert_eq!(tf.function.name, "get_weather");
                assert_eq!(tf.function.description, "Get weather for a city");
            }
        }
    }

    #[test]
    fn anthropic_convert_tools() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool::new("get_weather", "Get weather for a city")),
        ];
        let converted = provider.convert_tools(&tools).expect("convert success");
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, "get_weather");
        assert_eq!(converted[0].description, "Get weather for a city");
    }

    #[test]
    fn openai_build_produces_wire_json_with_messages_and_tools() {
        let provider = OpenAIProvider::new("test-key", "gpt-4o");
        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool::new("get_weather", "Get weather"))];

        let request = provider.build(&messages, &tools).expect("build success");

        assert_eq!(request["model"], "gpt-4o");
        assert_eq!(request["messages"].as_array().unwrap().len(), 1);
        let wire_tools = request["tools"].as_array().unwrap();
        assert_eq!(wire_tools.len(), 1);
        assert_eq!(wire_tools[0]["type"], "function");
        assert_eq!(wire_tools[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn anthropic_build_produces_wire_json_with_messages_and_tools() {
        let provider = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool::new("get_weather", "Get weather"))];

        let request = provider.build(&messages, &tools).expect("build success");

        assert_eq!(request["model"], "claude-3-5-sonnet");
        assert_eq!(request["stream"], true);
        let wire_tools = request["tools"].as_array().unwrap();
        assert_eq!(wire_tools.len(), 1);
        assert_eq!(wire_tools[0]["name"], "get_weather");
        assert!(wire_tools[0]["input_schema"].is_object());
    }

    /// A request-log handle over a shared in-memory buffer.
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn handle(&self) -> gewu_core::traits::RequestLog {
            Arc::new(std::sync::Mutex::new(Box::new(self.clone())))
        }

        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn parse_log_entries(log: &str) -> Vec<Value> {
        log.lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("log line must be valid JSON"))
            .collect()
    }

    #[tokio::test]
    async fn stream_response_logs_wire_request_before_sending() {
        // Unroutable endpoint: send fails, but the request is logged first.
        let provider = OpenAIProvider::new_with_endpoint("test-key", "http://127.0.0.1:1", "gpt-4o");
        let buffer = SharedBuffer::default();
        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool::new("get_weather", "Get weather"))];

        let result = provider
            .stream_response(&messages, &tools, Some(&buffer.handle()))
            .await;
        assert!(result.is_err(), "send should fail against 127.0.0.1:1");

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["timestamp"].is_string());
        assert_eq!(entries[0]["request"]["model"], "gpt-4o");
        assert_eq!(entries[0]["request"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            entries[0]["request"]["tools"].as_array().unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn stream_response_without_log_sends_normally() {
        let provider = OpenAIProvider::new_with_endpoint("test-key", "http://127.0.0.1:1", "gpt-4o");
        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];

        let result = provider.stream_response(&messages, &[], None).await;
        assert!(result.is_err(), "send should fail against 127.0.0.1:1");
    }

    #[test]
    fn openai_assistant_tool_calls_serialize_with_function_type() {
        // Regression: the OpenAI API (strict servers such as vLLM) requires
        // `"type": "function"` on each entry of assistant message tool_calls.
        let provider = OpenAIProvider::new("test-key", "gpt-4o");
        let messages = vec![Message::Assistant {
            content: None,
            tool_calls: Some(vec![CoreToolCall {
                id: "call_1".to_string(),
                r#type: "function".to_string(),
                function: CoreFunctionCall {
                    name: "list".to_string(),
                    arguments: "{\"path\":\"/\"}".to_string(),
                },
            }]),
        }];

        let request = provider.build(&messages, &[]).expect("build success");
        let tool_call = &request["messages"][0]["tool_calls"][0];
        assert_eq!(tool_call["type"], "function");
        assert_eq!(tool_call["id"], "call_1");
        assert_eq!(tool_call["function"]["name"], "list");
    }

    #[test]
    fn anthropic_config_strips_trailing_v1_from_base_url() {
        // Regression: the client appends `/v1/messages` itself, so a base URL
        // that already ends in `/v1` must not produce a doubled prefix.
        let config = crate::anthropic::AnthropicConfig::new("test-key", "https://host/v1/");
        assert_eq!(config.base_url, "https://host");

        let config = crate::anthropic::AnthropicConfig::new("test-key", "https://host");
        assert_eq!(config.base_url, "https://host");
    }

    #[tokio::test]
    async fn openai_parse_stream_yields_reasoning_deltas_for_glm_field() {
        // Regression: GLM-style gateways stream thinking under
        // `delta.reasoning`; the parser must emit ReasoningDelta events for
        // them instead of silently dropping the whole reasoning phase.
        let sse = concat!(
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"Step\"}}]}\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\" one\"}}]}\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Answer\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let body: std::collections::VecDeque<Result<bytes::Bytes, reqwest::Error>> =
            std::collections::VecDeque::from(vec![Ok(bytes::Bytes::from(sse))]);
        let wire = futures::stream::iter(body);
        let stream = OpenAIProvider::new("test-key", "glm-flash")
            .parse_stream(ChatCompletionStream::new(wire));

        let events: Vec<_> = stream
            .map(|e| e.expect("event should parse"))
            .collect()
            .await;
        let reasoning: String = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ReasoningDelta { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        let content: String = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ContentDelta { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "Step one");
        assert_eq!(content, "Answer");
    }
}
