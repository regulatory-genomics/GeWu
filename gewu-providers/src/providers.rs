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

use crate::anthropic::{
    AnthropicChunk, AnthropicConfig, AnthropicError, AnthropicMessage, AnthropicStream,
    AnthropicTool, AssistantMessage as AnthropicAssistantMessage, Client as AnthropicClient,
    ContentBlock, ContentBlockDeltaChunk, ContentBlockStart, ContentBlockStartChunk, ContentDelta,
    CreateMessageRequest, MessageStartChunk, SourceBlock, UserMessage,
};
use crate::openai::{
    AssistantMessage as OpenAIAssistantMessage, ChatCompletionMessageToolCall,
    ChatCompletionMessageToolCallChunk, ChatCompletionMessageToolCalls,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartFile,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionStream, ChatCompletionTool, ChatCompletionTools, CreateChatCompletionRequest,
    FileData, FunctionCall as OpenAIFunctionCall, FunctionObjectArgs, ImageUrl, OpenAIClient,
    OpenAIConfig, OpenAIError,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use gewu_core::traits::{LlmProvider, StatusSink, Tool};
use gewu_core::types::{
    Capabilities, ContentPart, Message, ProviderEvent, ReasoningReplay, TokenUsage,
};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

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
pub struct ChatCompletionsProvider {
    reasoning_effort: Option<String>,
    max_tokens: Option<u32>,
    base_url: String,
    reasoning_field: Option<String>,
    client: OpenAIClient,
    model: String,
    capabilities: Capabilities,
    retry_backoff: Duration,
    /// Upper bound on a single retry wait; longer `Retry-After` hints are
    /// clamped down to this.
    retry_max_wait: Duration,
    /// Sink for transient status messages (retry backoff), registered by
    /// the agent layer so frontends can show progress during waits.
    status_sink: Mutex<Option<StatusSink>>,
}

// Internal shorthand retained for the protocol adapter's existing tests.
type OpenAIProvider = ChatCompletionsProvider;

impl OpenAIProvider {
    pub fn with_reasoning_effort(mut self, effort: &str) -> Self {
        self.reasoning_effort = Some(effort.to_owned());
        self
    }
    pub fn with_max_tokens(mut self, limit: Option<u32>) -> Self {
        self.max_tokens = limit;
        self
    }
    pub(crate) fn for_service(
        api_key: &str,
        base_url: &str,
        model: &str,
        policy: crate::services::ChatPolicy,
    ) -> Self {
        let mut provider = Self::new_with_endpoint(api_key, base_url, model);
        provider.reasoning_field = Some(policy.reasoning_field.to_string());
        provider
    }
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
            max_tokens: None,
            reasoning_effort: None,
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning_field: None,
            client: OpenAIClient::with_config(config),
            model: model.to_string(),
            capabilities,
            retry_backoff: crate::http::RETRY_BACKOFF,
            retry_max_wait: crate::http::MAX_RETRY_WAIT,
            status_sink: Mutex::new(None),
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
            max_tokens: None,
            reasoning_effort: None,
            base_url: base_url.trim_end_matches('/').to_string(),
            reasoning_field: None,
            client: OpenAIClient::with_config(config),
            model: model.to_string(),
            capabilities,
            retry_backoff: crate::http::RETRY_BACKOFF,
            retry_max_wait: crate::http::MAX_RETRY_WAIT,
            status_sink: Mutex::new(None),
        }
    }

    /// Overrides the base delay between retries of a failed request setup
    /// (each retry waits four times longer than the previous one).
    pub fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// Overrides the upper bound on a single retry wait; a provider's
    /// `Retry-After` hint longer than this is clamped down to it.
    pub fn with_retry_max_wait(mut self, max_wait: Duration) -> Self {
        self.retry_max_wait = max_wait;
        self
    }

    /// Snapshot of the registered status sink, if any. Taken before the
    /// retry loop so no lock guard is held across an await point.
    fn status_sink_snapshot(&self) -> Option<StatusSink> {
        self.status_sink
            .lock()
            .expect("status sink lock poisoned")
            .clone()
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
            ContentPart::Text { text } | ContentPart::Reminder { text } => {
                Some(ChatCompletionRequestUserMessageContentPart::Text(
                    ChatCompletionRequestMessageContentPartText {
                        content_type: "text".to_string(),
                        text: text.clone(),
                    },
                ))
            }
            ContentPart::Image { data, mime_type } => {
                let url = format!("data:{};base64,{}", mime_type, data);
                Some(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                    ChatCompletionRequestMessageContentPartImage {
                        content_type: "image_url".to_string(),
                        image_url: ImageUrl { url, detail: None },
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
                                // The API expects a full data URI here, not raw
                                // base64 (matches the image_url branch above).
                                file_data: Some(format!("data:{};base64,{}", mime_type, data)),
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
            Message::Compaction { summary, recent } => self.convert_message(
                &gewu_core::compaction::build_checkpoint_message(summary, recent),
            ),
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
                reasoning,
                replay,
                tool_calls,
            } => {
                let origin = format!("chat:{}", self.base_url);
                let compatible = replay
                    .as_ref()
                    .filter(|r| r.origin == origin && r.model == self.model);
                let mut reasoning_fields = std::collections::BTreeMap::new();
                let field = self
                    .reasoning_field
                    .as_deref()
                    .or_else(|| compatible.and_then(|r| r.field.as_deref()))
                    .unwrap_or("reasoning_content");
                if ["role", "content", "refusal", "tool_calls"].contains(&field) {
                    return Err("invalid reasoning field in stored replay state".into());
                }
                if let Some(text) = reasoning.as_ref().filter(|_| compatible.is_some()) {
                    reasoning_fields.insert(field.to_string(), text.clone());
                }
                let content = if compatible.is_none() {
                    [reasoning.as_deref(), content.as_deref()]
                        .into_iter()
                        .flatten()
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                } else {
                    content.clone().unwrap_or_default()
                };
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
                                        // An empty arguments string is rejected
                                        // by the API; normalize to an empty
                                        // object.
                                        arguments: if tc.function.arguments.trim().is_empty() {
                                            "{}".to_string()
                                        } else {
                                            tc.function.arguments.clone()
                                        },
                                    },
                                },
                            )
                        })
                        .collect()
                });

                Ok(vec![ChatCompletionRequestMessage::Assistant(
                    OpenAIAssistantMessage {
                        reasoning: reasoning_fields,
                        content: (!content.is_empty()).then_some(content),
                        tool_calls: openai_calls,
                    },
                )])
            }
            Message::Tool {
                tool_call_id,
                tool_name: _,
                content,
                media: _,
            } => {
                // Media is NOT appended here: a user message between tool
                // messages breaks the `assistant(tool_calls) → tool` contiguity
                // that strict backends require. `build_typed` defers media to
                // after the run of tool messages.
                let m = ChatCompletionRequestToolMessageArgs::default()
                    .tool_call_id(tool_call_id.clone())
                    .content(content.clone())
                    .build()
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                Ok(vec![ChatCompletionRequestMessage::Tool(m)])
            }
        }
    }

    /// Emits deferred tool-result media as user message(s).
    ///
    /// Called once the run of tool messages ends so the produced user
    /// messages never interleave between two `tool` messages.
    fn flush_pending_media(
        &self,
        messages: &mut Vec<ChatCompletionRequestMessage>,
        pending: &mut Vec<(Option<String>, Vec<ContentPart>)>,
    ) {
        for (tool_name, media) in pending.drain(..) {
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
                if let Some(converted) = self.convert_content_part(&part) {
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
            if let Ok(extra) = ChatCompletionRequestUserMessageArgs::default()
                .content(parts)
                .build()
            {
                messages.push(ChatCompletionRequestMessage::User(extra));
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
            let parameters = crate::schema::tool_schema_value(&tool.schema())
                .map_err(|e| OpenAIError::Build(e.to_string()))?;
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
        // Media attached to tool results is deferred until the run of tool
        // messages ends; a user message between two `tool` messages breaks
        // the `assistant(tool_calls) → tool` contiguity strict backends
        // require.
        let mut pending_media: Vec<(Option<String>, Vec<ContentPart>)> = Vec::new();
        for msg in messages {
            if matches!(msg, Message::Tool { .. }) {
                let mut converted = self.convert_message(msg)?;
                openai_messages.append(&mut converted);
                if let Message::Tool {
                    tool_name, media, ..
                } = msg
                    && !media.is_empty()
                {
                    pending_media.push((tool_name.clone(), media.clone()));
                }
            } else {
                self.flush_pending_media(&mut openai_messages, &mut pending_media);
                let mut converted = self.convert_message(msg)?;
                openai_messages.append(&mut converted);
            }
        }
        self.flush_pending_media(&mut openai_messages, &mut pending_media);

        let openai_tools = self.convert_tools(tools)?;

        Ok(CreateChatCompletionRequest {
            reasoning_effort: self.reasoning_effort.clone(),
            model: self.model.clone(),
            messages: openai_messages,
            tools: if openai_tools.is_empty() {
                None
            } else {
                Some(openai_tools)
            },
            // This provider always streams (`create_stream` sets it on the
            // wire); logging the request without `stream: true` would make
            // the request log misrepresent what is sent.
            stream: Some(true),
            // Usage (and its cache figures) ride on the final chunk only
            // when requested; `create_stream` sets the same value on the
            // wire request.
            stream_options: Some(crate::openai::StreamOptions {
                include_usage: Some(true),
            }),
            max_tokens: self.max_tokens,
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

        if let Some(id) = &chunk.id
            && !id.is_empty()
        {
            state.id = Some(id.clone());
        }

        if let Some(function) = &chunk.function {
            if let Some(name) = &function.name
                && !name.is_empty()
            {
                state.name = Some(name.clone());
            }

            if let Some(args) = &function.arguments
                && !args.is_empty()
                && !state.finalized
            {
                state.argument_buffer.push_str(args);
                Self::update_finalized_flag(state);
                if state.started
                    && let Some(id) = state.id.clone()
                {
                    Self::emit_pending_arguments(state, &id, &mut events);
                }
            }
        }

        if !state.started
            && let (Some(id), Some(name)) = (state.id.clone(), state.name.clone())
        {
            events.push(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name,
            });
            state.started = true;

            Self::emit_pending_arguments(state, &id, &mut events);
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
        let field = self.reasoning_field.clone();
        let origin = format!("chat:{}", self.base_url);
        let model = self.model.clone();
        let s = async_stream::try_stream! {
            let mut tool_aggregator = ToolCallAggregator::default();

            let mut stream = stream;
            yield ProviderEvent::ReasoningReplay { replay: ReasoningReplay {
                origin: origin.clone(), model: model.clone(), field: field.clone(), blocks: vec![],
            }};
            while let Some(result) = stream.next().await {
                let response = result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                // The final chunk (requested via `stream_options.include_usage`)
                // carries the whole-request usage with an empty `choices`.
                if let Some(usage) = &response.usage
                    && (!response.choices.is_empty() || usage.prompt_tokens > 0 || usage.total_tokens > 0)
                {
                    // `cached_tokens` is a subset of `prompt_tokens`;
                    // the rest was processed uncached. Gateways without
                    // the details object report no cache hits.
                    let cached = usage
                        .prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0) as u64;
                    let reasoning = usage
                        .completion_tokens_details
                        .as_ref()
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0) as u64;
                    yield ProviderEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: (usage.prompt_tokens as u64).saturating_sub(cached),
                            cached_tokens: cached,
                            // OpenAI's automatic prefix cache reports no
                            // separate write figure.
                            cache_write_tokens: 0,
                            output_tokens: usage.completion_tokens as u64,
                            reasoning_tokens: reasoning,
                        },
                    };
                }
                if let Some(choice) = response.choices.first() {
                    let delta = &choice.delta;

                    let reasoning = field.as_deref().and_then(|name| {
                        if name == "reasoning_content" { delta.reasoning_content.as_deref() }
                        else { delta.extra.get(name).and_then(Value::as_str) }
                    }).map(|text| (field.as_deref().unwrap(), text))
                    .or_else(|| delta.reasoning_content.as_deref().map(|text| ("reasoning_content", text)))
                    .or_else(|| ["reasoning", "reasoning_text"].into_iter().find_map(|name|
                        delta.extra.get(name).and_then(Value::as_str).map(|text| (name, text))));
                    if let Some((name, reasoning)) = reasoning
                        && !reasoning.is_empty() {
                            yield ProviderEvent::ReasoningReplay { replay: ReasoningReplay {
                                origin: origin.clone(), model: model.clone(), field: Some(name.to_string()), blocks: vec![],
                            }};
                            yield ProviderEvent::ReasoningDelta { delta: reasoning.to_string() };
                        }

                    if let Some(content) = &delta.content
                        && !content.is_empty() {
                            yield ProviderEvent::ContentDelta { delta: content.clone() };
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
                            // `finish_reason` is a plain String; Debug-
                            // formatting it would wrap the value in literal
                            // quotes and break the consumer's match on
                            // "max_tokens"/"length".
                            reason: Some(reason.clone()),
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
        // Retry the request-setup phase on transport-level failures (connect
        // errors, read timeouts on a stalled connection). The wire request is
        // re-parsed per attempt because `create_stream` consumes it; parse
        // failures are permanent and never retried.
        let status_sink = self.status_sink_snapshot();
        let stream = crate::http::with_retries_capped(
            self.retry_backoff,
            self.retry_max_wait,
            || {
                let request = request.clone();
                async move {
                    let request: CreateChatCompletionRequest = serde_json::from_value(request)
                        .map_err(|e| OpenAIError::Build(e.to_string()))?;
                    self.client.chat().create_stream(request).await
                }
            },
            classify_openai_failure,
            status_sink.as_ref(),
        )
        .await
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        let parsed_stream = self.parse_stream(stream);

        Ok(parsed_stream)
    }

    fn set_status_sink(&self, sink: Option<StatusSink>) {
        *self.status_sink.lock().expect("status sink lock poisoned") = sink;
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
/// Output budget for the Anthropic provider when the config does not pick
/// one. Reasoning models (e.g. MiniMax M3) spend thinking tokens from this
/// budget, so a small value can end a turn with no text at all — MiniMax's
/// own API docs note that too small a `max_tokens` yields
/// `stop_reason: "max_tokens"` with no text output. Newer reasoning models
/// therefore get a much larger default than the legacy 8192.
const ANTHROPIC_LEGACY_MAX_TOKENS: u32 = 8192;
const ANTHROPIC_REASONING_MODEL_MAX_TOKENS: u32 = 32768;

pub struct AnthropicProvider {
    reasoning_effort: Option<String>,
    client: AnthropicClient,
    model: String,
    #[allow(dead_code)]
    capabilities: Capabilities,
    retry_backoff: Duration,
    /// Upper bound on a single retry wait; longer `Retry-After` hints are
    /// clamped down to this.
    retry_max_wait: Duration,
    /// Output token budget sent as `max_tokens` on every request.
    max_tokens: u32,
    /// Sink for transient status messages (retry backoff), registered by
    /// the agent layer so frontends can show progress during waits.
    status_sink: Mutex<Option<StatusSink>>,
}

impl AnthropicProvider {
    /// Per-model output budget: reasoning models think from the same budget
    /// and need far more than the legacy 8192.
    fn default_max_tokens_for_model(model: &str) -> u32 {
        let model_lower = model.to_lowercase();
        if model_lower.contains("minimax") {
            ANTHROPIC_REASONING_MODEL_MAX_TOKENS
        } else {
            ANTHROPIC_LEGACY_MAX_TOKENS
        }
    }

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
            retry_backoff: crate::http::RETRY_BACKOFF,
            retry_max_wait: crate::http::MAX_RETRY_WAIT,
            max_tokens: Self::default_max_tokens_for_model(model),
            reasoning_effort: None,
            status_sink: Mutex::new(None),
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
            retry_backoff: crate::http::RETRY_BACKOFF,
            retry_max_wait: crate::http::MAX_RETRY_WAIT,
            max_tokens: Self::default_max_tokens_for_model(model),
            reasoning_effort: None,
            status_sink: Mutex::new(None),
        }
    }

    /// Overrides the base delay between retries of a failed request setup
    /// (each retry waits four times longer than the previous one).
    pub fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// Overrides the upper bound on a single retry wait; a provider's
    /// `Retry-After` hint longer than this is clamped down to it.
    pub fn with_retry_max_wait(mut self, max_wait: Duration) -> Self {
        self.retry_max_wait = max_wait;
        self
    }

    /// Snapshot of the registered status sink, if any. Taken before the
    /// retry loop so no lock guard is held across an await point.
    fn status_sink_snapshot(&self) -> Option<StatusSink> {
        self.status_sink
            .lock()
            .expect("status sink lock poisoned")
            .clone()
    }

    /// Overrides the output token budget sent as `max_tokens`.
    ///
    /// Reasoning models spend thinking tokens from this budget; raise it when
    /// turns come back empty or truncated (`stop_reason: "max_tokens"`).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: &str) -> Self {
        self.reasoning_effort = Some(effort.to_owned());
        self
    }

    /// The output token budget sent as `max_tokens` on every request.
    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
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
            ContentPart::Text { text } | ContentPart::Reminder { text } => {
                Some(ContentBlock::Text { text: text.clone() })
            }
            ContentPart::Image { data, mime_type } => Some(ContentBlock::Image {
                source: SourceBlock {
                    source_type: "base64".to_string(),
                    media_type: mime_type.clone(),
                    data: data.clone(),
                },
            }),
            ContentPart::Audio { mime_type, .. } => Some(ContentBlock::Text {
                text: format!("[Audio file: {} - not supported by this model]", mime_type),
            }),
            ContentPart::Video { mime_type, .. } => Some(ContentBlock::Text {
                text: format!("[Video file: {} - not supported by this model]", mime_type),
            }),
            ContentPart::File { data, mime_type } if mime_type == "application/pdf" => {
                Some(ContentBlock::Document {
                    source: SourceBlock {
                        source_type: "base64".to_string(),
                        media_type: mime_type.clone(),
                        data: data.clone(),
                    },
                })
            }
            ContentPart::File { mime_type, .. } if mime_type != "application/pdf" => {
                Some(ContentBlock::Text {
                    text: format!("[File: {} - not supported by this model]", mime_type),
                })
            }
            ContentPart::File { .. } => None,
        }
    }

    fn convert_message(
        &self,
        msg: &Message,
    ) -> Result<Vec<AnthropicMessage>, Box<dyn Error + Send + Sync>> {
        match msg {
            Message::Compaction { summary, recent } => self.convert_message(
                &gewu_core::compaction::build_checkpoint_message(summary, recent),
            ),
            Message::System { content: _ } => Ok(vec![]),
            Message::User { content } => {
                let blocks: Vec<ContentBlock> = content
                    .iter()
                    .filter_map(|p| self.convert_content_part(p))
                    .collect();
                Ok(vec![AnthropicMessage::User(UserMessage {
                    content: blocks,
                })])
            }
            Message::Assistant {
                content,
                reasoning,
                replay,
                tool_calls,
            } => {
                let origin = format!(
                    "anthropic:{}",
                    self.client.config.base_url.trim_end_matches('/')
                );
                if let Some(replay) = replay
                    .as_ref()
                    .filter(|r| r.origin == origin && r.model == self.model)
                {
                    let blocks: Vec<ContentBlock> = replay
                        .blocks
                        .iter()
                        .cloned()
                        .map(serde_json::from_value)
                        .collect::<Result<_, _>>()?;
                    for block in &blocks {
                        if let ContentBlock::Thinking { signature, .. } = block
                            && signature.is_empty()
                        {
                            return Err(
                                "cannot replay incomplete Anthropic thinking: missing signature"
                                    .into(),
                            );
                        }
                    }
                    return Ok(vec![AnthropicMessage::Assistant(
                        AnthropicAssistantMessage {
                            content: Some(blocks),
                        },
                    )]);
                }
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if let Some(text) = reasoning.as_ref().filter(|text| !text.is_empty()) {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                if let Some(text) = content
                    && !text.is_empty()
                {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        // An empty arguments string (e.g. a no-argument tool
                        // call whose fragments never arrived) must become an
                        // empty JSON object; `from_str("")` is a hard error.
                        let args = call.function.arguments.trim();
                        let input: Value = if args.is_empty() {
                            Value::Object(Default::default())
                        } else {
                            serde_json::from_str(args).map_err(|e| {
                                format!(
                                    "Invalid arguments for Anthropic tool call '{}': {}",
                                    call.function.name, e
                                )
                            })?
                        };
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
                    Ok(vec![AnthropicMessage::Assistant(
                        AnthropicAssistantMessage {
                            content: Some(blocks),
                        },
                    )])
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
                Ok(vec![AnthropicMessage::User(UserMessage {
                    content: blocks,
                })])
            }
        }
    }

    fn convert_tools(
        &self,
        tools: &[Arc<dyn Tool>],
    ) -> Result<Vec<AnthropicTool>, Box<dyn Error + Send + Sync>> {
        let mut anthropic_tools: Vec<AnthropicTool> = Vec::new();
        for tool in tools {
            let parameters = crate::schema::tool_schema_value(&tool.schema())
                .map_err(|e| OpenAIError::Build(e.to_string()))?;
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
            output_config: self
                .reasoning_effort
                .as_ref()
                .map(|effort| serde_json::json!({"effort": effort})),
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
            max_tokens: Some(self.max_tokens),
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
            ContentBlockStart::ToolUse { id, name, input } => {
                let state = &mut self.states[idx];
                state.id = Some(id.clone());
                state.name = Some(name.clone());
                state.started = true;
                events.push(ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                // Anthropic delivers the complete `input` object on the start
                // block for some calls (notably no-argument tools) and sends
                // no `input_json_delta` fragments afterwards. Seed the buffer
                // so the arguments reach the agent instead of ending up empty.
                if let Some(input) = input
                    && input.as_object().map(|o| !o.is_empty()).unwrap_or(false)
                {
                    state.argument_buffer = input.to_string();
                    let new_len = state.argument_buffer.len();
                    let delta = state.argument_buffer[state.sent_len..new_len].to_string();
                    state.sent_len = new_len;
                    if !delta.is_empty() {
                        events.push(ProviderEvent::ToolCallDelta {
                            id: id.clone(),
                            delta,
                        });
                    }
                }
            }
            ContentBlockStart::Thinking { thinking, .. } => {
                if !thinking.is_empty() {
                    events.push(ProviderEvent::ReasoningDelta {
                        delta: thinking.clone(),
                    });
                }
            }
            ContentBlockStart::RedactedThinking { .. } => {}
            ContentBlockStart::Text { .. } => {}
        }

        events
    }

    fn process_delta(&mut self, chunk: &ContentBlockDeltaChunk, index: u32) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let idx = index as usize;

        if idx >= self.states.len() {
            // A delta without a preceding start block can still carry
            // reasoning text; create the state so it is not dropped.
            // (InputJsonDelta below is still guarded by `started`.)
            self.states.resize_with(idx + 1, ToolCallState::default);
        }

        let state = &mut self.states[idx];

        match &chunk.delta {
            ContentDelta::InputJson { partial_json } => {
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
                            events.push(ProviderEvent::ToolCallDelta { id, delta });
                        }
                    }
                }
            }
            ContentDelta::Thinking { thinking } => {
                if !thinking.is_empty() {
                    events.push(ProviderEvent::ReasoningDelta {
                        delta: thinking.clone(),
                    });
                }
            }
            ContentDelta::Text { .. } => {}
            ContentDelta::Signature { .. } => {}
        }

        events
    }
}

impl AnthropicProvider {
    fn parse_stream(
        &self,
        stream: AnthropicStream,
    ) -> futures::stream::BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>
    {
        let origin = format!(
            "anthropic:{}",
            self.client.config.base_url.trim_end_matches('/')
        );
        let model = self.model.clone();
        let s = async_stream::try_stream! {
            let mut tool_aggregator = AnthropicToolCallAggregator::default();
            let mut blocks = std::collections::BTreeMap::<u32, Value>::new();
            let mut arguments = std::collections::BTreeMap::<u32, String>::new();
            let mut has_reasoning = false;
            let mut stream = stream;

            // Cache figures may be zeroed on `message_start` by proxies
            // (LiteLLM/OpenRouter report real numbers only in the final
            // `message_delta`), so each field keeps the latest non-zero
            // value across both events.
            let mut usage = TokenUsage::default();

            while let Some(result) = stream.next().await {
                let chunk = result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

                match chunk {
                    AnthropicChunk::MessageStart(MessageStartChunk { _message }) => {
                        if let Some(message_usage) = _message.usage {
                            if message_usage.input_tokens > 0 {
                                usage.input_tokens = message_usage.input_tokens;
                            }
                            if message_usage.cache_read_input_tokens > 0 {
                                usage.cached_tokens = message_usage.cache_read_input_tokens;
                            }
                            if message_usage.cache_creation_input_tokens > 0 {
                                usage.cache_write_tokens = message_usage.cache_creation_input_tokens;
                            }
                            if message_usage.output_tokens > 0 {
                                usage.output_tokens = message_usage.output_tokens;
                            }
                        }
                    }
                    AnthropicChunk::ContentBlockStart(chunk) => {
                        let block = match &chunk.content_block {
                            ContentBlockStart::Thinking { thinking, signature } => {
                                has_reasoning = true;
                                serde_json::json!({"type":"thinking", "thinking":thinking, "signature":signature})
                            }
                            ContentBlockStart::RedactedThinking { data } => {
                                has_reasoning = true;
                                serde_json::json!({"type":"redacted_thinking", "data":data})
                            }
                            ContentBlockStart::Text { _text } => {
                                if !_text.is_empty() {
                                    yield ProviderEvent::ContentDelta { delta: _text.clone() };
                                }
                                serde_json::json!({"type":"text", "text":_text})
                            }
                            ContentBlockStart::ToolUse { id, name, input } =>
                                serde_json::json!({"type":"tool_use", "id":id, "name":name, "input":input.clone().unwrap_or_else(|| serde_json::json!({}))}),
                        };
                        blocks.insert(chunk.index, block);
                        for event in tool_aggregator.process_chunk(&chunk, chunk.index) {
                            yield event;
                        }
                    }
                    AnthropicChunk::ContentBlockDelta(chunk) => {
                        if let Some(block) = blocks.get_mut(&chunk.index) {
                            let (field, delta) = match &chunk.delta {
                                ContentDelta::Text { text } => ("text", text.as_str()),
                                ContentDelta::Thinking { thinking } => ("thinking", thinking.as_str()),
                                ContentDelta::Signature { signature } => ("signature", signature.as_str()),
                                ContentDelta::InputJson { partial_json } => {
                                    arguments.entry(chunk.index).or_default().push_str(partial_json);
                                    ("", "")
                                }
                            };
                            if !field.is_empty() {
                                let mut text = block[field].as_str().unwrap_or("").to_string();
                                text.push_str(delta);
                                block[field] = Value::String(text);
                            }
                        }
                        for event in tool_aggregator.process_delta(&chunk, chunk.index) {
                            yield event;
                        }
                        match &chunk.delta {
                            ContentDelta::Text { text } => {
                                if !text.is_empty() {
                                    yield ProviderEvent::ContentDelta { delta: text.clone() };
                                }
                            }
                            // ThinkingDelta is already emitted by the
                            // aggregator above; emitting it here too would
                            // duplicate every reasoning token in the stream.
                            ContentDelta::Thinking { .. } => {}
                            ContentDelta::InputJson { .. } => {}
                            ContentDelta::Signature { .. } => {}
                        }
                    }
                    AnthropicChunk::ContentBlockStop(_) => {}
                    AnthropicChunk::MessageDelta(chunk) => {
                        // `message_delta.usage` is the cumulative figure;
                        // merge it over the `message_start` values (latest
                        // non-zero per field, see above).
                        let message_usage = &chunk._usage;
                        if message_usage.input_tokens > 0 {
                            usage.input_tokens = message_usage.input_tokens;
                        }
                        if message_usage.cache_read_input_tokens > 0 {
                            usage.cached_tokens = message_usage.cache_read_input_tokens;
                        }
                        if message_usage.cache_creation_input_tokens > 0 {
                            usage.cache_write_tokens = message_usage.cache_creation_input_tokens;
                        }
                        if message_usage.output_tokens > 0 {
                            usage.output_tokens = message_usage.output_tokens;
                        }
                        if let Some(reason) = chunk.delta.stop_reason {
                            if has_reasoning {
                                for (index, json) in &arguments {
                                    if let Some(block) = blocks.get_mut(index) {
                                        block["input"] = serde_json::from_str(json)?;
                                    }
                                }
                                yield ProviderEvent::ReasoningReplay { replay: ReasoningReplay {
                                    origin: origin.clone(), model: model.clone(), field: None,
                                    blocks: blocks.values().cloned().collect(),
                                }};
                            }
                            // Emit once, with the terminal cumulative
                            // figures: some flows (server tools) deliver
                            // several cumulative `message_delta` events, and
                            // emitting each would double-count downstream.
                            if usage.prompt_tokens() > 0 || usage.output_tokens > 0 {
                                yield ProviderEvent::Usage { usage };
                            }
                            yield ProviderEvent::Finish { reason: Some(reason) };
                        }
                    }
                    AnthropicChunk::MessageStop => {}
                    AnthropicChunk::Error(e) => {
                        // An `error` event is terminal per the Anthropic
                        // streaming spec: the server sends nothing else and
                        // keeps the connection open. Continuing to poll would
                        // hang the turn forever. Surfacing it as a stream
                        // failure lets the retry layer restart transient
                        // outages (overloaded servers) before anything was
                        // yielded.
                        Err::<(), Box<dyn Error + Send + Sync>>(Box::new(
                            crate::anthropic::stream_error(&e.error),
                        ))?;
                    }
                }
            }
        };

        Box::pin(s)
    }
}

/// Whether a chat-completions request-level failure may be retried from
/// scratch. The default is retryable — transport errors, server outages
/// (5xx/408 with their hints), SSE parse failures, unknown shapes — so
/// future error modes self-heal instead of killing the ask. Only
/// deterministic failures a re-send cannot fix are permanent: request
/// building and 4xx client errors.
fn classify_openai_failure(error: &OpenAIError) -> crate::http::RetryDecision {
    match error {
        // Deterministic failures a re-send cannot fix.
        OpenAIError::Build(_) | OpenAIError::Client { .. } => crate::http::RetryDecision::No,
        // Provider-suggested waits are honored, capped by the retry layer.
        OpenAIError::RateLimit { retry_after, .. } | OpenAIError::Server { retry_after, .. } => {
            crate::http::RetryDecision::Yes {
                after: *retry_after,
            }
        }
        // Transport errors, parse failures, unknown shapes. The retry layer
        // only restarts while nothing was yielded, which keeps a restart
        // duplication-free.
        _ => crate::http::RetryDecision::Yes { after: None },
    }
}

/// Whether an Anthropic request- or stream-level failure may be retried from
/// scratch. The default is retryable — transport errors, server outages
/// (5xx/408 with their hints), parse failures, unknown shapes — so future
/// error modes self-heal instead of killing the ask. Only deterministic
/// failures a re-send cannot fix are permanent: request building and 4xx
/// client errors, plus typed in-stream errors not marked transient by
/// [`crate::anthropic::stream_error`].
fn classify_anthropic_failure(error: &crate::anthropic::Failure) -> crate::http::RetryDecision {
    match error.downcast_ref::<AnthropicError>() {
        // Deterministic failures a re-send cannot fix.
        Some(AnthropicError::Build(_)) | Some(AnthropicError::Client { .. }) => {
            crate::http::RetryDecision::No
        }
        Some(AnthropicError::Stream {
            transient: false, ..
        }) => crate::http::RetryDecision::No,
        // Provider-suggested waits are honored, capped by the retry layer.
        Some(AnthropicError::RateLimit { retry_after, .. })
        | Some(AnthropicError::Server { retry_after, .. }) => crate::http::RetryDecision::Yes {
            after: *retry_after,
        },
        Some(AnthropicError::Stream {
            transient: true,
            retry_after,
            ..
        }) => crate::http::RetryDecision::Yes {
            after: *retry_after,
        },
        // Transport errors, parse failures, unknown shapes. (A boxed
        // reqwest::Error lands here too.) The retry layer only restarts
        // while nothing was yielded, which keeps a restart duplication-free.
        _ => crate::http::RetryDecision::Yes { after: None },
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
        // Every attempt (re)issues the whole request; stream-level failures
        // restart too, as long as nothing has been yielded yet. Request-setup
        // failures (transport errors, rate limits) enter the same loop as a
        // single-item error stream; parse failures are permanent.
        let status_sink = self.status_sink_snapshot();
        let make_stream = move || {
            let request = request.clone();
            futures::stream::once(async move {
                let request: CreateMessageRequest = match serde_json::from_value(request) {
                    Ok(request) => request,
                    Err(e) => {
                        return futures::stream::once(async move {
                            Err(Box::new(AnthropicError::Build(e.to_string()))
                                as Box<dyn Error + Send + Sync>)
                        })
                        .boxed();
                    }
                };
                match self.client.messages().create_stream(request).await {
                    Ok(stream) => self.parse_stream(stream),
                    Err(e) => futures::stream::once(async move {
                        Err(Box::new(e) as Box<dyn Error + Send + Sync>)
                    })
                    .boxed(),
                }
            })
            .flatten()
            .boxed()
        };
        Ok(Box::pin(crate::http::retrying_stream(
            make_stream,
            classify_anthropic_failure,
            status_sink,
            self.retry_backoff,
            self.retry_max_wait,
        )))
    }

    fn set_status_sink(&self, sink: Option<StatusSink>) {
        *self.status_sink.lock().expect("status sink lock poisoned") = sink;
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
    #[tokio::test]
    async fn chat_reasoning_replays_detected_field_and_respects_origin() {
        let body = concat!(
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_text\":\"consider \"}}]}\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_text\":\"tools\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 200 OK",
            &[("Content-Type", "text/event-stream")],
            body,
        )])
        .await;
        let provider =
            OpenAIProvider::new_with_endpoint("key", &format!("http://{addr}/v1"), "reasoner");
        let mut stream = provider.stream_response(&[], &[], None).await.unwrap();
        let mut text = String::new();
        let mut replay = None;
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                ProviderEvent::ReasoningDelta { delta } => text.push_str(&delta),
                ProviderEvent::ReasoningReplay { replay: state } => replay = Some(state),
                _ => {}
            }
        }
        let message = Message::Assistant {
            content: None,
            reasoning: Some(text),
            replay,
            tool_calls: Some(vec![gewu_core::types::ToolCall {
                id: "call-1".into(),
                r#type: "function".into(),
                function: gewu_core::types::FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }]),
        };
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        let wire = serde_json::to_value(provider.convert_message(&restored).unwrap()).unwrap();
        assert_eq!(wire[0]["reasoning_text"], "consider tools");
        let request = provider
            .build(
                &[
                    restored.clone(),
                    Message::Tool {
                        tool_call_id: "call-1".into(),
                        tool_name: Some("read".into()),
                        content: "result".into(),
                        media: vec![],
                    },
                    Message::User {
                        content: vec![ContentPart::Text {
                            text: "continue".into(),
                        }],
                    },
                ],
                &[],
            )
            .unwrap();
        assert_eq!(request["messages"][0]["reasoning_text"], "consider tools");
        assert_eq!(request["messages"][1]["role"], "tool");
        assert_eq!(request["messages"][2]["role"], "user");
        let other = OpenAIProvider::new("key", "other-model");
        let wire = serde_json::to_value(other.convert_message(&restored).unwrap()).unwrap();
        assert!(wire[0].get("reasoning_text").is_none());
        let required = crate::GlmProvider::new("key", "other-model");
        let wire = required
            .build(std::slice::from_ref(&restored), &[])
            .unwrap();
        assert!(wire["messages"][0].get("reasoning").is_none());
        drop(stream);
        let custom =
            crate::DeepSeekProvider::new_with_endpoint("key", &provider.base_url, "reasoner");
        let wire = custom.build(std::slice::from_ref(&restored), &[]).unwrap();
        assert_eq!(wire["messages"][0]["reasoning_content"], "consider tools");
        let plain = Message::Assistant {
            content: Some("plain".into()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        };
        let wire = custom.build(&[plain], &[]).unwrap();
        assert!(wire["messages"][0].get("reasoning_content").is_none());
    }

    #[tokio::test]
    async fn anthropic_replays_ordered_signed_and_redacted_blocks() {
        let events = [
            serde_json::json!({"type":"content_block_start", "index":0,"content_block":{"type":"thinking","thinking":"consider"}}),
            serde_json::json!({"type":"content_block_delta", "index":0,"delta":{"type":"signature_delta","signature":"sig-"}}),
            serde_json::json!({"type":"content_block_delta", "index":0,"delta":{"type":"signature_delta","signature":"123"}}),
            serde_json::json!({"type":"content_block_start", "index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
            serde_json::json!({"type":"content_block_start", "index":2,"content_block":{"type":"text","text":"checking"}}),
            serde_json::json!({"type":"content_block_start", "index":3,"content_block":{"type":"tool_use","id":"call-1","name":"read","input":{}}}),
            serde_json::json!({"type":"content_block_delta", "index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}),
            serde_json::json!({"type":"content_block_start", "index":4,"content_block":{"type":"thinking","thinking":"again", "signature":"second-signature"}}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
        ];
        let body = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect::<String>();
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 200 OK",
            &[("Content-Type", "text/event-stream")],
            &body,
        )])
        .await;
        let provider =
            AnthropicProvider::new_with_endpoint("key", &format!("http://{addr}"), "claude-test");
        let mut stream = provider.stream_response(&[], &[], None).await.unwrap();
        let mut replay = None;
        while let Some(event) = stream.next().await {
            if let ProviderEvent::ReasoningReplay { replay: state } = event.unwrap() {
                replay = Some(state);
            }
        }
        let message = Message::Assistant {
            content: Some("checking".into()),
            reasoning: Some("consider".into()),
            replay,
            tool_calls: None,
        };
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        let wire = serde_json::to_value(provider.convert_message(&restored).unwrap()).unwrap();
        let blocks = &wire[0]["content"];
        assert_eq!(blocks[0]["signature"], "sig-123");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "opaque");
        assert_eq!(blocks[2]["text"], "checking");
        assert_eq!(blocks[3]["input"]["path"], "a");
        assert_eq!(blocks[4]["thinking"], "again");
        assert_eq!(blocks[4]["signature"], "second-signature");
        let request = provider
            .build(
                &[
                    restored.clone(),
                    Message::Tool {
                        tool_call_id: "call-1".into(),
                        tool_name: Some("read".into()),
                        content: "result".into(),
                        media: vec![],
                    },
                ],
                &[],
            )
            .unwrap();
        assert_eq!(request["messages"][0]["content"][0]["signature"], "sig-123");
        assert_eq!(
            request["messages"][1]["content"][0]["tool_use_id"],
            "call-1"
        );
        let mut incomplete = restored.clone();
        if let Message::Assistant {
            replay: Some(state),
            ..
        } = &mut incomplete
        {
            state.blocks[0]["signature"] = serde_json::json!("");
        }
        assert!(provider.convert_message(&incomplete).is_err());
        let other = AnthropicProvider::new("key", "other");
        let wire = serde_json::to_value(other.convert_message(&restored).unwrap()).unwrap();
        assert_eq!(wire[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(wire[0]["content"][0]["type"], "text");
        assert_eq!(wire[0]["content"][0]["text"], "consider");
    }
    use super::*;
    use crate::openai::FunctionCallStream;
    use async_trait::async_trait;
    use gewu_core::types::{FunctionCall as CoreFunctionCall, ToolCall as CoreToolCall};
    use gewu_core::{Tool, ToolContext, ToolResult};
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

    /// Reasoning models think from the `max_tokens` budget, so the default
    /// must be large enough for them to emit text after thinking; legacy
    /// models keep the conservative 8192.
    #[test]
    fn anthropic_default_max_tokens_is_model_aware() {
        let reasoning = AnthropicProvider::new("test-key", "MiniMax-M3.1-flash");
        assert_eq!(reasoning.max_tokens, 32768);

        let legacy = AnthropicProvider::new("test-key", "claude-3-5-sonnet");
        assert_eq!(legacy.max_tokens, 8192);
    }

    #[test]
    fn anthropic_max_tokens_is_configurable() {
        let provider =
            AnthropicProvider::new("test-key", "MiniMax-M3.1-flash").with_max_tokens(65_536);
        let request = provider
            .build_typed(
                &[Message::User {
                    content: vec![ContentPart::Text {
                        text: "hi".to_string(),
                    }],
                }],
                &[],
            )
            .expect("request builds");
        assert_eq!(request.max_tokens, Some(65_536));
    }

    /// Reminder parts are model-facing scaffolding: both providers must
    /// carry them on the wire as plain text so the model still sees them.
    #[test]
    fn reminder_parts_reach_providers_as_text() {
        let reminder_text = "<system-reminder>Plan mode is active.</system-reminder>";
        let messages = [Message::User {
            content: vec![
                ContentPart::Text {
                    text: "prompt".to_string(),
                },
                ContentPart::Reminder {
                    text: reminder_text.to_string(),
                },
            ],
        }];

        let openai = OpenAIProvider::new("test-key", "gpt-4")
            .build_typed(&messages, &[])
            .expect("openai request builds");
        let wire = serde_json::to_string(&openai).unwrap();
        assert!(
            wire.contains(reminder_text),
            "OpenAI wire lost the reminder"
        );

        let anthropic = AnthropicProvider::new("test-key", "claude-3-5-sonnet")
            .build_typed(&messages, &[])
            .expect("anthropic request builds");
        let AnthropicMessage::User(UserMessage { content }) = &anthropic.messages[0] else {
            panic!("expected a user message");
        };
        assert!(
            content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == reminder_text))
        );
    }

    #[test]
    fn convert_message_defers_media_followup() {
        // Regression: a user message between two tool messages breaks the
        // `assistant(tool_calls) → tool` contiguity strict backends require,
        // so convert_message must emit ONLY the tool message; build_typed
        // flushes deferred media after the run of tool messages ends.
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
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            ChatCompletionRequestMessage::Tool(tool_msg) => {
                assert_eq!(tool_msg.tool_call_id.as_str(), "call_1");
            }
            _ => panic!("only message should be tool"),
        }

        // Through build_typed, two adjacent tool messages with media must
        // keep their tool messages contiguous, with media flushed after.
        let tool2 = Message::Tool {
            tool_call_id: "call_2".to_string(),
            tool_name: Some("read_file".to_string()),
            content: "second".to_string(),
            media: vec![ContentPart::Image {
                data: "Wg==".to_string(),
                mime_type: "image/png".to_string(),
            }],
        };
        let assistant = Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: None,
        };
        let request = provider
            .build_typed(&[msg, tool2, assistant], &[])
            .expect("build success");
        let roles: Vec<String> = request
            .messages
            .iter()
            .map(|m| match m {
                ChatCompletionRequestMessage::Tool(_) => "tool".to_string(),
                ChatCompletionRequestMessage::User(_) => "user".to_string(),
                ChatCompletionRequestMessage::Assistant(_) => "assistant".to_string(),
                ChatCompletionRequestMessage::System(_) => "system".to_string(),
            })
            .collect();
        assert_eq!(roles, vec!["tool", "tool", "user", "user", "assistant"]);
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
            reasoning: None,
            replay: None,
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
            reasoning: None,
            replay: None,
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
            reasoning: None,
            replay: None,
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
            AnthropicMessage::Assistant(message) => match message.content.as_ref().unwrap().first()
            {
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
                Some(ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                }) => {
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
        assert!(
            converted.is_empty(),
            "System messages should return empty for Anthropic"
        );
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

    #[derive(schemars::JsonSchema)]
    struct AllOptionalArgs {
        #[allow(dead_code)]
        path: Option<String>,
    }

    struct OptionalArgsTool;

    #[async_trait]
    impl Tool for OptionalArgsTool {
        fn name(&self) -> &str {
            "list"
        }
        fn description(&self) -> &str {
            "List directory entries"
        }
        fn schema(&self) -> RootSchema {
            schemars::schema_for!(AllOptionalArgs)
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
            Ok(ToolResult::new("list", "ok"))
        }
    }

    /// Asserts the invariants Anthropic's draft 2020-12 validation enforces.
    fn assert_wire_compatible(schema: &Value) {
        assert!(
            schema.get("$schema").is_none(),
            "tool schemas must not declare a dialect: {schema}"
        );
        assert!(schema.get("definitions").is_none());
        if schema.get("properties").is_some() {
            assert!(
                schema["required"].is_array(),
                "properties without required is rejected: {schema}"
            );
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
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MockTool::new(
            "get_weather",
            "Get weather for a city",
        ))];
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
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(MockTool::new("get_weather", "Get weather")),
            Arc::new(OptionalArgsTool),
        ];

        let request = provider.build(&messages, &tools).expect("build success");

        assert_eq!(request["model"], "claude-3-5-sonnet");
        assert_eq!(request["stream"], true);
        let wire_tools = request["tools"].as_array().unwrap();
        assert_eq!(wire_tools.len(), 2);
        assert_eq!(wire_tools[0]["name"], "get_weather");
        assert!(wire_tools[0]["input_schema"].is_object());
        for tool in wire_tools {
            assert_wire_compatible(&tool["input_schema"]);
        }
        assert_eq!(
            wire_tools[1]["input_schema"]["required"],
            serde_json::json!([])
        );
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
        // The request must be logged before it goes over the wire; a
        // successful canned response proves it was sent and the log filled.
        let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 200 OK",
            &[("Content-Type", "text/event-stream")],
            sse,
        )])
        .await;
        let provider =
            OpenAIProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "gpt-4o");
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
        assert!(
            result.is_ok(),
            "send should succeed against the canned server"
        );

        let entries = parse_log_entries(&buffer.contents());
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["timestamp"].is_string());
        assert_eq!(entries[0]["request"]["model"], "gpt-4o");
        assert_eq!(
            entries[0]["request"]["messages"].as_array().unwrap().len(),
            1
        );
        assert_eq!(entries[0]["request"]["tools"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stream_response_without_log_sends_normally() {
        let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 200 OK",
            &[("Content-Type", "text/event-stream")],
            sse,
        )])
        .await;
        let provider =
            OpenAIProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "gpt-4o");
        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];

        let result = provider.stream_response(&messages, &[], None).await;
        assert!(
            result.is_ok(),
            "send should succeed against the canned server"
        );
    }

    #[test]
    fn openai_assistant_tool_calls_serialize_with_function_type() {
        // Regression: the OpenAI API (strict servers such as vLLM) requires
        // `"type": "function"` on each entry of assistant message tool_calls.
        let provider = OpenAIProvider::new("test-key", "gpt-4o");
        let messages = vec![Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
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

    // -- 429 rate-limit handling -------------------------------------------

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Builds a complete HTTP/1.1 response with `Connection: close` so the
    /// client never reuses the connection across canned replies.
    fn http_response(status_line: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut response = format!("{status_line}\r\n");
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str(&format!("Content-Length: {}\r\n", body.len()));
        response.push_str("Connection: close\r\n\r\n");
        response.push_str(body);
        response
    }

    /// Serves one canned response per connection, in order, then stops
    /// accepting. Requests are drained before each reply.
    async fn serve_canned(responses: Vec<String>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                // Drain the request head and body so the reply is not reset.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    let mut data = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let head_end = data.windows(4).position(|w| w == b"\r\n\r\n");
                        if let Some(head_end) = head_end {
                            let head = String::from_utf8_lossy(&data[..head_end]).to_string();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let lowered = line.to_ascii_lowercase();
                                    let value = lowered.strip_prefix("content-length:")?;
                                    value.trim().parse::<usize>().ok()
                                })
                                .unwrap_or(0);
                            if data.len() >= head_end + 4 + length {
                                break;
                            }
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => data.extend_from_slice(&chunk[..n]),
                        }
                    }
                })
                .await;
                if socket.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
                let _ = socket.shutdown().await;
            }
        });
        addr
    }

    const NEW_API_429_BODY: &str = r#"{"error":{"code":"user_rate_limit_exceeded","message":"You have reached the request limit: at most 1 requests per 60 minutes","type":"new_api_error"}}"#;

    #[tokio::test]
    async fn openai_rate_limit_with_short_hint_is_retried() {
        let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let addr = serve_canned(vec![
            http_response(
                "HTTP/1.1 429 Too Many Requests",
                &[("Retry-After", "0")],
                r#"{"error":{"message":"slow down","type":"new_api_error"}}"#,
            ),
            http_response(
                "HTTP/1.1 200 OK",
                &[("Content-Type", "text/event-stream")],
                sse,
            ),
        ])
        .await;
        let provider =
            OpenAIProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "gpt-4o")
                .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let events: Vec<_> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("retry after the short hint should succeed")
            .map(|event| event.expect("event should parse"))
            .collect()
            .await;
        let content: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ContentDelta { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(content, "hi");
    }

    #[tokio::test]
    async fn openai_rate_limit_with_long_hint_is_clamped_and_retried() {
        let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let addr = serve_canned(vec![
            http_response(
                "HTTP/1.1 429 Too Many Requests",
                &[("Retry-After", "3600")],
                NEW_API_429_BODY,
            ),
            http_response(
                "HTTP/1.1 200 OK",
                &[("Content-Type", "text/event-stream")],
                sse,
            ),
        ])
        .await;
        // Tiny injected cap: the hour-long hint clamps to it instead of
        // stalling the test for minutes.
        let provider =
            OpenAIProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "gpt-4o")
                .with_retry_backoff(std::time::Duration::from_millis(1))
                .with_retry_max_wait(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let events: Vec<_> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("clamped retry should succeed despite the hour-long hint")
            .map(|event| event.expect("event should parse"))
            .collect()
            .await;
        let content: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ContentDelta { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(content, "hi");
    }

    #[tokio::test]
    async fn anthropic_rate_limit_with_long_hint_is_clamped_and_retried() {
        let sse = "event: message_stop\ndata: {}\n\n";
        let addr = serve_canned(vec![
            http_response(
                "HTTP/1.1 429 Too Many Requests",
                &[("retry-after", "3600")],
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of requests too high"}}"#,
            ),
            http_response("HTTP/1.1 200 OK", &[("Content-Type", "text/event-stream")], sse),
        ])
        .await;
        let provider =
            AnthropicProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "claude")
                .with_retry_backoff(std::time::Duration::from_millis(1))
                .with_retry_max_wait(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let events: Vec<ProviderEvent> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("clamped retry should succeed despite the hour-long hint")
            .map(|event| event.expect("event should parse"))
            .collect()
            .await;
        // The retry delivered a clean (empty but valid) stream, not an error.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Error { .. }))
        );
    }

    // -- Retryable-by-default failures (gateway outages, transport drops) ---

    #[tokio::test]
    async fn openai_gateway_error_is_retried_and_recovers() {
        let sse = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let addr = serve_canned(vec![
            http_response("HTTP/1.1 502 Bad Gateway", &[], "upstream exploded"),
            http_response(
                "HTTP/1.1 200 OK",
                &[("Content-Type", "text/event-stream")],
                sse,
            ),
        ])
        .await;
        let provider =
            OpenAIProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "gpt-4o")
                .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let events: Vec<_> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("a 502 before any output should be retried")
            .map(|event| event.expect("event should parse"))
            .collect()
            .await;
        let content: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ContentDelta { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(content, "hi");
    }

    #[tokio::test]
    async fn openai_client_error_is_not_retried() {
        // A single canned reply: a retry would find the server gone and end
        // with a transport error, so surfacing the typed 400 proves exactly
        // one attempt was made.
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 400 Bad Request",
            &[],
            r#"{"error":{"message":"bad","type":"invalid_request_error"}}"#,
        )])
        .await;
        let provider =
            OpenAIProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "gpt-4o")
                .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let error = match provider.stream_response(&messages, &[], None).await {
            Ok(_) => panic!("a 400 is permanent"),
            Err(error) => error,
        };
        let OpenAIError::Client { status, .. } = error
            .downcast_ref::<OpenAIError>()
            .expect("typed provider error")
        else {
            panic!("expected a typed client error");
        };
        assert_eq!(*status, reqwest::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn responses_gateway_error_is_retried_and_recovers() {
        let done = "event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\r\n\r\n";
        let addr = serve_canned(vec![
            http_response("HTTP/1.1 502 Bad Gateway", &[], "upstream exploded"),
            http_response(
                "HTTP/1.1 200 OK",
                &[("Content-Type", "text/event-stream")],
                done,
            ),
        ])
        .await;
        let provider = crate::responses::OpenAIProvider::new_with_endpoint(
            "test-key",
            &format!("http://{addr}/v1"),
            "reasoner",
        )
        .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let events: Vec<_> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("stream should start")
            .map(|event| event.expect("a retried 502 should recover"))
            .collect()
            .await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Usage { .. }))
        );
    }

    #[tokio::test]
    async fn responses_client_error_is_not_retried() {
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 400 Bad Request",
            &[],
            r#"{"error":{"message":"bad","type":"invalid_request_error"}}"#,
        )])
        .await;
        let provider = crate::responses::OpenAIProvider::new_with_endpoint(
            "test-key",
            &format!("http://{addr}/v1"),
            "reasoner",
        )
        .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let results: Vec<_> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("stream should start")
            .collect()
            .await;
        let error = results
            .into_iter()
            .last()
            .expect("a terminal failure")
            .expect_err("a 400 is permanent");
        let crate::openai::OpenAIError::Client { status, .. } = error
            .downcast_ref::<crate::openai::OpenAIError>()
            .expect("typed provider error")
        else {
            panic!("expected a typed client error");
        };
        assert_eq!(*status, reqwest::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn anthropic_gateway_error_is_retried_and_recovers() {
        let sse = "event: message_stop\ndata: {}\n\n";
        let addr = serve_canned(vec![
            http_response("HTTP/1.1 502 Bad Gateway", &[], "upstream exploded"),
            http_response(
                "HTTP/1.1 200 OK",
                &[("Content-Type", "text/event-stream")],
                sse,
            ),
        ])
        .await;
        let provider =
            AnthropicProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "claude")
                .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let events: Vec<ProviderEvent> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("stream should start")
            .map(|event| event.expect("a retried 502 should recover"))
            .collect()
            .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Error { .. }))
        );
    }

    #[tokio::test]
    async fn anthropic_client_error_is_not_retried() {
        let addr = serve_canned(vec![http_response(
            "HTTP/1.1 400 Bad Request",
            &[],
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
        )])
        .await;
        let provider =
            AnthropicProvider::new_with_endpoint("test-key", &format!("http://{addr}"), "claude")
                .with_retry_backoff(std::time::Duration::from_millis(1));

        let messages = vec![Message::User {
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
        }];
        let results: Vec<_> = provider
            .stream_response(&messages, &[], None)
            .await
            .expect("stream should start")
            .collect()
            .await;
        let error = results
            .into_iter()
            .last()
            .expect("a terminal failure")
            .expect_err("a 400 is permanent");
        let AnthropicError::Client { status, .. } = error
            .downcast_ref::<AnthropicError>()
            .expect("typed provider error")
        else {
            panic!("expected a typed client error");
        };
        assert_eq!(*status, reqwest::StatusCode::BAD_REQUEST);
    }

    // -- Provider-reported usage (cache accounting) ------------------------

    /// Collects the parsed events from a raw OpenAI SSE body.
    async fn openai_parse_sse(sse: &'static str) -> Vec<ProviderEvent> {
        let body: std::collections::VecDeque<Result<bytes::Bytes, reqwest::Error>> =
            std::collections::VecDeque::from(vec![Ok(bytes::Bytes::from_static(sse.as_bytes()))]);
        let wire = futures::stream::iter(body);
        OpenAIProvider::new("test-key", "gpt-4o")
            .parse_stream(ChatCompletionStream::new(wire))
            .map(|e| e.expect("event should parse"))
            .collect()
            .await
    }

    /// Collects the parsed events from a raw Anthropic SSE body.
    async fn anthropic_parse_sse(sse: &'static str) -> Vec<ProviderEvent> {
        let body: std::collections::VecDeque<Result<bytes::Bytes, reqwest::Error>> =
            std::collections::VecDeque::from(vec![Ok(bytes::Bytes::from_static(sse.as_bytes()))]);
        let wire = futures::stream::iter(body);
        AnthropicProvider::new("test-key", "claude-3-5-sonnet")
            .parse_stream(AnthropicStream::new(wire))
            .map(|e| e.expect("event should parse"))
            .collect()
            .await
    }

    #[tokio::test]
    async fn anthropic_in_stream_errors_surface_as_typed_failures() {
        let sse = concat!(
            "event: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\n",
        );
        let body: std::collections::VecDeque<Result<bytes::Bytes, reqwest::Error>> =
            std::collections::VecDeque::from(vec![Ok(bytes::Bytes::from_static(sse.as_bytes()))]);
        let wire = futures::stream::iter(body);
        let events: Vec<_> = AnthropicProvider::new("test-key", "claude-3-5-sonnet")
            .parse_stream(AnthropicStream::new(wire))
            .collect()
            .await;
        let error = events
            .into_iter()
            .last()
            .expect("the error event yields a failure")
            .expect_err("an in-stream error event is a stream failure");
        let AnthropicError::Stream {
            transient, message, ..
        } = error
            .downcast_ref::<AnthropicError>()
            .expect("the failure carries the typed provider error")
        else {
            panic!("expected a typed stream error");
        };
        assert!(transient);
        assert!(message.contains("overloaded_error"), "{message}");
    }

    #[test]
    fn anthropic_classifies_failures_by_retryability() {
        let failure: Box<dyn Error + Send + Sync> = Box::new(AnthropicError::Stream {
            transient: true,
            message: "overloaded_error: Overloaded".into(),
            retry_after: None,
        });
        assert!(matches!(
            classify_anthropic_failure(&failure),
            crate::http::RetryDecision::Yes { after: None }
        ));
        let permanent: Box<dyn Error + Send + Sync> = Box::new(AnthropicError::Stream {
            transient: false,
            message: "invalid_request_error: nope".into(),
            retry_after: None,
        });
        assert!(matches!(
            classify_anthropic_failure(&permanent),
            crate::http::RetryDecision::No
        ));
        // Server outages retry, honoring the provider's wait hint.
        let server: Box<dyn Error + Send + Sync> = Box::new(AnthropicError::Server {
            status: reqwest::StatusCode::BAD_GATEWAY,
            retry_after: Some(std::time::Duration::from_secs(2)),
            message: "upstream exploded".into(),
        });
        assert!(matches!(
            classify_anthropic_failure(&server),
            crate::http::RetryDecision::Yes { after: Some(_) }
        ));
        // A 408 is typed as Server and retries; a 400 is permanent.
        let timeout: Box<dyn Error + Send + Sync> = Box::new(AnthropicError::status_error(
            reqwest::StatusCode::REQUEST_TIMEOUT,
            None,
            "slow",
        ));
        assert!(matches!(
            classify_anthropic_failure(&timeout),
            crate::http::RetryDecision::Yes { after: None }
        ));
        let client: Box<dyn Error + Send + Sync> = Box::new(AnthropicError::status_error(
            reqwest::StatusCode::BAD_REQUEST,
            None,
            "bad",
        ));
        assert!(matches!(
            classify_anthropic_failure(&client),
            crate::http::RetryDecision::No
        ));
        let build: Box<dyn Error + Send + Sync> = Box::new(AnthropicError::Build("nope".into()));
        assert!(matches!(
            classify_anthropic_failure(&build),
            crate::http::RetryDecision::No
        ));
        // Untyped failures (raw transport errors, parse failures, unknown
        // shapes) default to retryable.
        let untyped: Box<dyn Error + Send + Sync> = Box::new(std::io::Error::other("boom"));
        assert!(matches!(
            classify_anthropic_failure(&untyped),
            crate::http::RetryDecision::Yes { after: None }
        ));
    }

    #[test]
    fn openai_classifies_failures_by_retryability() {
        // Server outages retry, honoring the provider's wait hint.
        assert!(matches!(
            classify_openai_failure(&OpenAIError::Server {
                status: reqwest::StatusCode::BAD_GATEWAY,
                retry_after: Some(std::time::Duration::from_secs(2)),
                message: "upstream exploded".into(),
            }),
            crate::http::RetryDecision::Yes { after: Some(_) }
        ));
        // A 408 is typed as Server and retries; a 400 is permanent.
        assert!(matches!(
            classify_openai_failure(&OpenAIError::status_error(
                reqwest::StatusCode::REQUEST_TIMEOUT,
                None,
                "slow",
            )),
            crate::http::RetryDecision::Yes { after: None }
        ));
        assert!(matches!(
            classify_openai_failure(&OpenAIError::status_error(
                reqwest::StatusCode::BAD_REQUEST,
                None,
                "bad",
            )),
            crate::http::RetryDecision::No
        ));
        assert!(matches!(
            classify_openai_failure(&OpenAIError::Build("nope".into())),
            crate::http::RetryDecision::No
        ));
        // Transport errors and SSE parse failures retry.
        assert!(matches!(
            classify_openai_failure(&OpenAIError::SseParse("garbage".into())),
            crate::http::RetryDecision::Yes { after: None }
        ));
    }

    fn usage_events(events: &[ProviderEvent]) -> Vec<gewu_core::types::TokenUsage> {
        events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::Usage { usage } => Some(*usage),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn anthropic_usage_merges_start_and_delta_cache_figures() {
        // Proxies (LiteLLM/OpenRouter) zero the cache fields on
        // `message_start` and report real numbers in the final
        // `message_delta`; each field must keep its latest non-zero value.
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"m1","role":"assistant","content":[],"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42,"cache_read_input_tokens":700,"cache_creation_input_tokens":50}}"#,
            "\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
        );
        let events = anthropic_parse_sse(sse).await;
        let usages = usage_events(&events);
        assert_eq!(usages.len(), 1, "usage emitted once: {events:?}");
        let usage = usages[0];
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cached_tokens, 700);
        assert_eq!(usage.cache_write_tokens, 50);
        assert_eq!(usage.output_tokens, 42);
        // 700 of 850 prompt tokens were served from cache.
        assert_eq!(usage.cache_hit_rate_percent(), Some(82));
    }

    #[tokio::test]
    async fn anthropic_usage_without_stop_reason_is_not_emitted() {
        // A cumulative `message_delta` without a stop reason is not the
        // terminal one; emitting per delta would double-count.
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"m1","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{},"usage":{"output_tokens":2}}"#,
            "\n\n",
        );
        let events = anthropic_parse_sse(sse).await;
        assert!(
            usage_events(&events).is_empty(),
            "no Usage before Finish: {events:?}"
        );
    }

    #[tokio::test]
    async fn openai_usage_chunk_maps_cached_tokens() {
        // The final chunk (empty `choices`) carries the usage; cached tokens
        // are a subset of prompt tokens and the rest was billed uncached.
        let sse = concat!(
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Answer"}}]}"#,
            "\n\n",
            r#"data: {"id":"c1","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,"total_tokens":1020,"prompt_tokens_details":{"cached_tokens":800},"completion_tokens_details":{"reasoning_tokens":7}}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let events = openai_parse_sse(sse).await;
        let usages = usage_events(&events);
        assert_eq!(usages.len(), 1, "usage emitted once: {events:?}");
        let usage = usages[0];
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.cached_tokens, 800);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.reasoning_tokens, 7);
        assert_eq!(usage.cache_hit_rate_percent(), Some(80));
    }

    #[tokio::test]
    async fn openai_usage_without_reasoning_details_reports_zero() {
        // Gateways may omit `completion_tokens_details`; thinking tokens
        // are unknown, not zero-billed.
        let sse = concat!(
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Answer"}}]}"#,
            "\n\n",
            r#"data: {"id":"c1","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let events = openai_parse_sse(sse).await;
        let usages = usage_events(&events);
        assert_eq!(usages[0].reasoning_tokens, 0);
    }

    #[tokio::test]
    async fn openai_usage_without_details_reports_no_cache_hits() {
        // Gateways (DeepSeek/GLM) may omit `prompt_tokens_details`; the
        // request must still parse, with every prompt token uncached.
        let sse = concat!(
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Answer"}}]}"#,
            "\n\n",
            r#"data: {"id":"c1","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,"total_tokens":1020}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let events = openai_parse_sse(sse).await;
        let usages = usage_events(&events);
        assert_eq!(usages.len(), 1, "{events:?}");
        assert_eq!(usages[0].input_tokens, 1000);
        assert_eq!(usages[0].cached_tokens, 0);
        assert_eq!(usages[0].cache_hit_rate_percent(), Some(0));
    }
}
