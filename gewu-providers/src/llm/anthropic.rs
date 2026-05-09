use crate::anthropic::{AnthropicChunk, AnthropicConfig, AnthropicMessage, AnthropicStream, AnthropicTool, AssistantMessage as AnthropicAssistantMessage, Client as AnthropicClient, ContentBlock, ContentBlockDeltaChunk, ContentBlockStart, ContentBlockStartChunk, ContentDelta, CreateMessageRequest, MessageStartChunk, ToolCall, UserMessage};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Capabilities, ContentPart, Message, ProviderEvent};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

pub fn parse_fixture_events(sse: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut current_event = None::<String>;
    for line in sse.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some(event) = line.strip_prefix("event:") { current_event = Some(event.trim().to_string()); continue; }
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            match current_event.as_deref() {
                Some("content_block_delta") => {
                    let value: serde_json::Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
                    if let Some(text) = value.get("delta").and_then(|d| d.get("text")).and_then(|v| v.as_str()) && !text.is_empty() { out.push(format!("content:{text}")); }
                }
                Some("message_delta") => {
                    let value: serde_json::Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
                    if let Some(reason) = value.get("delta").and_then(|d| d.get("stop_reason")).and_then(|v| v.as_str()) { out.push(format!("finish:{reason}")); }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Default, Clone)]
struct ToolCallState { id: Option<String>, name: Option<String>, argument_buffer: String, sent_len: usize, started: bool }

pub struct AnthropicProvider { client: AnthropicClient, model: String, #[allow(dead_code)] capabilities: Capabilities, max_output_tokens: Option<u32> }

impl AnthropicProvider {
    pub fn new(api_key: &str, model: &str) -> Self { let config = AnthropicConfig::new(api_key, "https://api.anthropic.com"); let capabilities = Self::default_capabilities_for_model(model); Self { client: AnthropicClient::with_config(config), model: model.to_string(), capabilities, max_output_tokens: Some(8192) } }
    pub fn new_with_endpoint(api_key: &str, base_url: &str, model: &str) -> Self { let config = AnthropicConfig::new(api_key, base_url); let capabilities = Self::default_capabilities_for_model(model); Self { client: AnthropicClient::with_config(config), model: model.to_string(), capabilities, max_output_tokens: Some(8192) } }
    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self { self.max_output_tokens = max_output_tokens; self }
    pub fn with_extended_thinking(mut self) -> Self { self.client.config.beta = Some("thinking-2025-05-14".to_string()); self }
    fn default_capabilities_for_model(model: &str) -> Capabilities { let model_lower = model.to_lowercase(); if model_lower.contains("claude-3-5-sonnet") || model_lower.contains("claude-3-opus") { Capabilities { input: gewu_core::types::InputCapabilities { text: true, image: true, audio: false, video: false, pdf: true }, output: gewu_core::types::OutputCapabilities { text: true, image: false, audio: false, video: false, pdf: false } } } else if model_lower.contains("claude-3-haiku") { Capabilities { input: gewu_core::types::InputCapabilities { text: true, image: false, audio: false, video: false, pdf: false }, output: gewu_core::types::OutputCapabilities { text: true, image: false, audio: false, video: false, pdf: false } } } else { Capabilities { input: gewu_core::types::InputCapabilities { text: true, image: true, audio: false, video: false, pdf: true }, output: gewu_core::types::OutputCapabilities { text: true, image: false, audio: false, video: false, pdf: false } } } }
    fn convert_content_part(&self, part: &ContentPart) -> Option<ContentBlock> { match part { ContentPart::Text { text } => Some(ContentBlock::Text { text: text.clone() }), ContentPart::Image { data, mime_type } => Some(ContentBlock::Text { text: format!("[Image: {mime_type}](data:{mime_type};base64,{data})") }), ContentPart::Audio { data, mime_type } => Some(ContentBlock::Text { text: format!("[Audio: {mime_type}](data:{mime_type};base64,{data})") }), ContentPart::Video { data, mime_type } => Some(ContentBlock::Text { text: format!("[Video: {mime_type}](data:{mime_type};base64,{data})") }), ContentPart::File { data, mime_type } => Some(ContentBlock::Text { text: format!("[File: {mime_type}](data:{mime_type};base64,{data})") }) } }
    fn convert_message(&self, msg: &Message) -> Result<Vec<AnthropicMessage>, Box<dyn Error + Send + Sync>> { match msg { Message::System { .. } => Ok(vec![]), Message::User { content } => Ok(vec![AnthropicMessage::User(UserMessage { content: content.iter().filter_map(|p| self.convert_content_part(p)).collect() })]), Message::Assistant { content, tool_calls } => { let mut blocks = Vec::new(); if let Some(text) = content && !text.is_empty() { blocks.push(ContentBlock::Text { text: text.clone() }); } let anthropic_tool_calls = tool_calls.as_ref().and_then(|calls| if calls.is_empty() { None } else { Some(calls.iter().map(|tc| ToolCall { id: tc.id.clone(), tool_type: "tool_use".to_string(), name: tc.function.name.clone(), input: serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null) }).collect()) }); if blocks.is_empty() && anthropic_tool_calls.is_none() { Ok(vec![]) } else { Ok(vec![AnthropicMessage::Assistant(AnthropicAssistantMessage { content: if blocks.is_empty() { None } else { Some(blocks) }, tool_calls: anthropic_tool_calls })]) } }, Message::Tool { content, media, .. } => { let mut blocks = vec![ContentBlock::Text { text: content.clone() }]; for part in media { if let Some(block) = self.convert_content_part(part) { blocks.push(block); } } Ok(vec![AnthropicMessage::User(UserMessage { content: blocks })]) } } }
    fn convert_tools(&self, tools: &[Arc<dyn Tool>]) -> Result<Vec<AnthropicTool>, Box<dyn Error + Send + Sync>> {
        let mut out = Vec::new();
        for tool in tools {
            let input_schema = serde_json::to_value(tool.schema())
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
            out.push(AnthropicTool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema,
            });
        }
        Ok(out)
    }
    fn build(&self, messages: &[Message], tools: &[Arc<dyn Tool>]) -> Result<CreateMessageRequest, Box<dyn Error + Send + Sync>> { let mut anthropic_messages = Vec::new(); let mut system_prompt = String::new(); for msg in messages { if let Message::System { content } = msg { if !system_prompt.is_empty() { system_prompt.push('\n'); } system_prompt.push_str(content); } else { anthropic_messages.extend(self.convert_message(msg)?); } } let anthropic_tools = self.convert_tools(tools)?; Ok(CreateMessageRequest { model: self.model.clone(), messages: anthropic_messages, system: if system_prompt.is_empty() { None } else { Some(system_prompt) }, tools: if anthropic_tools.is_empty() { None } else { Some(anthropic_tools) }, stream: true, max_tokens: self.max_output_tokens }) }
    fn parse_stream(&self, stream: AnthropicStream) -> BoxStream<'static, Result<ProviderEvent, Box<dyn Error + Send + Sync>>> { let s = try_stream! { let mut tool_aggregator = AnthropicToolCallAggregator::default(); let mut stream = stream; while let Some(result) = stream.next().await { let chunk = result.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?; match chunk { AnthropicChunk::MessageStart(MessageStartChunk { .. }) => {}, AnthropicChunk::ContentBlockStart(chunk) => for event in tool_aggregator.process_chunk(&chunk, chunk.index) { yield event; }, AnthropicChunk::ContentBlockDelta(chunk) => { for event in tool_aggregator.process_delta(&chunk, chunk.index) { yield event; } match &chunk.delta { ContentDelta::TextDelta { text } if !text.is_empty() => yield ProviderEvent::ContentDelta { delta: text.clone() }, ContentDelta::ThinkingDelta { thinking } if !thinking.is_empty() => yield ProviderEvent::ReasoningDelta { delta: thinking.clone() }, ContentDelta::ThinkingDelta { .. } => {}, ContentDelta::InputJsonDelta { .. } => {}, _ => {} } }, AnthropicChunk::ContentBlockStop(_) => {}, AnthropicChunk::MessageDelta(chunk) => { if let Some(reason) = chunk.delta.stop_reason { yield ProviderEvent::Finish { reason: Some(reason) }; } }, AnthropicChunk::MessageStop => {}, AnthropicChunk::Error(e) => yield ProviderEvent::Error { error: e.error.message }, } } }; Box::pin(s) }
}

#[derive(Default)]
struct AnthropicToolCallAggregator { states: Vec<ToolCallState> }

impl AnthropicToolCallAggregator { fn process_chunk(&mut self, chunk: &ContentBlockStartChunk, index: u32) -> Vec<ProviderEvent> { let mut events = Vec::new(); let idx = index as usize; if idx >= self.states.len() { self.states.resize_with(idx + 1, ToolCallState::default); } match &chunk.content_block { ContentBlockStart::ToolUse { id, name } => { let state = &mut self.states[idx]; state.id = Some(id.clone()); state.name = Some(name.clone()); state.started = true; events.push(ProviderEvent::ToolCallStart { id: id.clone(), name: name.clone() }); }, ContentBlockStart::Thinking { thinking, .. } => if !thinking.is_empty() { events.push(ProviderEvent::ReasoningDelta { delta: thinking.clone() }); }, ContentBlockStart::Text { .. } => {} } events } fn process_delta(&mut self, chunk: &ContentBlockDeltaChunk, index: u32) -> Vec<ProviderEvent> { let mut events = Vec::new(); let idx = index as usize; if idx >= self.states.len() { return events; } let state = &mut self.states[idx]; match &chunk.delta { ContentDelta::InputJsonDelta { partial_json } => { if !state.started { return events; } state.argument_buffer.push_str(partial_json); if let Some(id) = state.id.clone() { let new_len = state.argument_buffer.len(); if new_len > state.sent_len { let delta = state.argument_buffer[state.sent_len..new_len].to_string(); state.sent_len = new_len; if !delta.is_empty() { events.push(ProviderEvent::ToolCallDelta { id, delta }); } } } }, ContentDelta::ThinkingDelta { thinking } if !thinking.is_empty() => events.push(ProviderEvent::ReasoningDelta { delta: thinking.clone() }), ContentDelta::ThinkingDelta { .. } => {}, ContentDelta::TextDelta { .. } => {} } events } }

#[async_trait]
impl LlmProvider for AnthropicProvider { async fn stream_response<'a>(&'a self, messages: &[Message], tools: &[Arc<dyn Tool>]) -> Result<BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>, Box<dyn Error + Send + Sync>> { let request = self.build(messages, tools)?; let stream = self.client.messages().create_stream(request).await.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?; Ok(self.parse_stream(stream)) } async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> { Ok(self.client.models().list().await.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?.data.into_iter().map(|m| m.id).collect()) } }
