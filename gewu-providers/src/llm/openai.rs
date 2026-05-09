use crate::llm::{RequestBuilder, ResponseParser};
use crate::openai::{
    AssistantMessage as OpenAIAssistantMessage, ChatCompletionMessageToolCall,
    ChatCompletionMessageToolCallChunk, ChatCompletionMessageToolCalls,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartFile,
    ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionStream, ChatCompletionTool, ChatCompletionTools, OpenAIClient,
    CreateChatCompletionRequest, FileData, FunctionCall as OpenAIFunctionCall, FunctionObjectArgs,
    ImageUrl, OpenAIConfig,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Capabilities, ContentPart, Message, ProviderEvent};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::Arc;

pub fn parse_fixture_events(sse: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for block in sse.split("\n\n") {
        let data = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .collect::<Vec<_>>()
            .join("");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        if let Some(choice) = value.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) {
            if let Some(content) = choice.get("delta").and_then(|d| d.get("content")).and_then(|v| v.as_str()) && !content.is_empty() {
                out.push(format!("content:{content}"));
            }
            if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                out.push(format!("finish:{reason}"));
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Default, Clone)]
struct ToolCallState {
    id: Option<String>,
    name: Option<String>,
    argument_buffer: String,
    sent_len: usize,
    started: bool,
    finalized: bool,
}


pub struct OpenAIProvider {
    client: OpenAIClient,
    model: String,
    capabilities: Capabilities,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
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
            max_output_tokens: None,
            temperature: None,
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
            client: OpenAIClient::with_config(config),
            model: model.to_string(),
            capabilities,
            max_output_tokens: None,
            temperature: None,
        }
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub fn with_temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
    }

    fn openai_safe_tool_name(name: &str) -> String {
        let mut safe = String::new();
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                safe.push(ch);
            } else {
                safe.push_str("_x");
                safe.push_str(&format!("{:x}", ch as u32));
                safe.push('_');
            }
        }
        if safe.is_empty() {
            "tool".to_string()
        } else {
            safe
        }
    }

    fn tool_name_aliases(
        tools: &[Arc<dyn Tool>],
    ) -> (HashMap<String, String>, HashMap<String, String>) {
        let mut original_to_safe = HashMap::new();
        let mut safe_to_original = HashMap::new();
        let mut used = HashSet::new();

        for (index, tool) in tools.iter().enumerate() {
            let original = tool.name().to_string();
            let base = Self::openai_safe_tool_name(&original);
            let mut safe = base.clone();
            if used.contains(&safe) {
                safe = format!("{base}_{index}");
            }
            used.insert(safe.clone());
            original_to_safe.insert(original.clone(), safe.clone());
            safe_to_original.insert(safe, original);
        }

        (original_to_safe, safe_to_original)
    }

    fn restore_tool_event_name(
        event: ProviderEvent,
        safe_to_original: &HashMap<String, String>,
    ) -> ProviderEvent {
        match event {
            ProviderEvent::ToolCallStart { id, name } => ProviderEvent::ToolCallStart {
                id,
                name: safe_to_original.get(&name).cloned().unwrap_or(name),
            },
            other => other,
        }
    }

    /// Determines capabilities based on model name.
    ///
    /// This is a heuristic based on common model naming patterns.
    /// For accurate capability settings, use a custom provider implementation.
    #[allow(clippy::branches_sharing_code, clippy::if_same_then_else)]
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
                let url = format!("data:{mime_type};base64,{data}");
                Some(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                    ChatCompletionRequestMessageContentPartImage {
                        content_type: "image_url".to_string(),
                        image_url: ImageUrl { url, detail: None },
                    },
                ))
            }
            ContentPart::Audio { data, mime_type } => {
                let url = format!("data:{mime_type};base64,{data}");
                Some(ChatCompletionRequestUserMessageContentPart::Text(
                    ChatCompletionRequestMessageContentPartText {
                        content_type: "text".to_string(),
                        text: format!("[Audio file: {mime_type}]({url})"),
                    },
                ))
            }
            ContentPart::Video { data, mime_type } => {
                let url = format!("data:{mime_type};base64,{data}");
                Some(ChatCompletionRequestUserMessageContentPart::Text(
                    ChatCompletionRequestMessageContentPartText {
                        content_type: "text".to_string(),
                        text: format!("[Video file: {mime_type}]({url})"),
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
                    let url = format!("data:{mime_type};base64,{data}");
                    Some(ChatCompletionRequestUserMessageContentPart::Text(
                        ChatCompletionRequestMessageContentPartText {
                            content_type: "text".to_string(),
                            text: format!("[File: {mime_type}]({url})"),
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
    fn convert_message_with_aliases(
        &self,
        msg: &Message,
        original_to_safe: &HashMap<String, String>,
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
                                        name: original_to_safe
                                            .get(&tc.function.name)
                                            .cloned()
                                            .unwrap_or_else(|| tc.function.name.clone()),
                                        arguments: tc.function.arguments.clone(),
                                    },
                                },
                            )
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
                        Some(name) => format!("Tool '{name}' returned additional media:"),
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
                                "[Unsupported media type: {mt} - this model does not support {mt} input]",
                                mt = part.mime_type()
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
        original_to_safe: &HashMap<String, String>,
    ) -> Result<Vec<ChatCompletionTools>, Box<dyn Error + Send + Sync>> {
        let mut openai_tools: Vec<ChatCompletionTools> = Vec::new();
        for tool in tools {
            let schema = tool.schema();
            let mut parameters = serde_json::to_value(&schema)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
            if let Some(obj) = parameters.as_object_mut() {
                let is_object = obj
                    .get("type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == "object");
                if is_object {
                    if !obj.contains_key("properties") {
                        obj.insert("properties".into(), serde_json::json!({}));
                    }
                    if !obj.contains_key("required") {
                        obj.insert("required".into(), serde_json::json!([]));
                    }
                }
            }
            let function = FunctionObjectArgs::default()
                .name(
                    original_to_safe
                        .get(tool.name())
                        .map(String::as_str)
                        .unwrap_or_else(|| tool.name()),
                )
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
        let (original_to_safe, _) = Self::tool_name_aliases(tools);
        let mut openai_messages: Vec<ChatCompletionRequestMessage> = Vec::new();
        for msg in messages {
            let mut converted = self.convert_message_with_aliases(msg, &original_to_safe)?;
            openai_messages.append(&mut converted);
        }

        let openai_tools = self.convert_tools(tools, &original_to_safe)?;

        Ok(CreateChatCompletionRequest {
            model: self.model.clone(),
            messages: openai_messages,
            tools: if openai_tools.is_empty() {
                None
            } else {
                Some(openai_tools)
            },
            stream: None,
            max_tokens: self.max_output_tokens,
            temperature: self.temperature,
        })
    }
}

/// Internal state for tracking tool call argument streaming.
///
/// The OpenAI API may stream tool call arguments in multiple chunks.
/// This struct tracks the state of each in-progress tool call to properly
/// reassemble the arguments and emit events when both the ID and
/// arguments are available.
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
        self.parse_stream_with_tool_aliases(stream, HashMap::new())
    }
}

impl OpenAIProvider {
    fn parse_stream_with_tool_aliases(
        &self,
        stream: ChatCompletionStream,
        safe_to_original: HashMap<String, String>,
    ) -> BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>> {
        let s = async_stream::try_stream! {
            let mut tool_aggregator = ToolCallAggregator::default();

            let mut stream = stream;
            while let Some(result) = stream.next().await {
                let response = result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                if let Some(choice) = response.choices.first() {
                    let delta = &choice.delta;

                    if let Some(reasoning) = &delta.reasoning_content
                        && !reasoning.is_empty() {
                            yield ProviderEvent::ReasoningDelta { delta: reasoning.clone() };
                        }

                    if let Some(content) = &delta.content
                        && !content.is_empty() {
                            yield ProviderEvent::ContentDelta { delta: content.clone() };
                        }

                    if let Some(tool_calls) = &delta.tool_calls {
                        for tool_call in tool_calls {
                            for event in tool_aggregator.process_chunk(tool_call) {
                                yield Self::restore_tool_event_name(event, &safe_to_original);
                            }
                        }
                    }

                    if let Some(reason) = &choice.finish_reason {
                        yield ProviderEvent::Finish {
                            reason: Some(format!("{reason:?}")),
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
        let (_, safe_to_original) = Self::tool_name_aliases(tools);
        let request = self.build(messages, tools)?;

        let stream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        let parsed_stream = self.parse_stream_with_tool_aliases(stream, safe_to_original);

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
