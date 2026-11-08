//! OpenAI API types, HTTP client, and SSE parsing.
//!
//! This module provides types for interacting with OpenAI's Chat Completions API,
//! including request/response structures, streaming support via Server-Sent Events,
//! and an HTTP client implementation.
//!
//! # Request Flow
//!
//! 1. Create a [`OpenAIClient`] with [`OpenAIConfig`]
//! 2. Use [`OpenAIClient::chat()`] to get a [`ChatCompletions`] handle
//! 3. Call [`ChatCompletions::create_stream()`] to get a streaming response
//!
//! # Example
//!
//! ```ignore
//! use gewu_providers::openai::{OpenAIClient, OpenAIConfig};
//!
//! let client = OpenAIClient::with_config(OpenAIConfig::new("api-key", "https://api.openai.com/v1"));
//! // Use client.chat().create_stream(request) to make requests
//! ```

use bytes::Bytes;
use futures::Stream;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

/// Errors that can occur when interacting with the OpenAI API.
#[derive(Error, Debug)]
pub enum OpenAIError {
    /// HTTP request failed (network error, timeout, etc.)
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Error parsing Server-Sent Events response.
    #[error("SSE parse error: {0}")]
    SseParse(String),
    /// API returned an error response.
    #[error("API error: {0}")]
    Api(String),
    /// Error building request (missing required fields, etc.).
    #[error("Build error: {0}")]
    Build(String),
}

/// Configuration for the OpenAI API client.
#[derive(Debug, Clone)]
pub struct OpenAIConfig {
    api_key: String,
    base_url: String,
}

impl OpenAIConfig {
    /// Creates a new configuration.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your OpenAI API key.
    /// * `base_url` - The base URL of the API (e.g., <https://api.openai.com/v1>).
    pub fn new(api_key: &str, base_url: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

/// HTTP client for OpenAI API requests.
#[derive(Debug)]
pub struct OpenAIClient {
    config: OpenAIConfig,
    http_client: HttpClient,
}

impl OpenAIClient {
    pub fn with_config(config: OpenAIConfig) -> Self {
        Self {
            config,
            http_client: HttpClient::new(),
        }
    }

    pub fn chat(&self) -> ChatCompletions<'_> {
        ChatCompletions {
            client: self,
        }
    }

    pub fn models(&self) -> Models<'_> {
        Models { client: self }
    }
}

pub struct ChatCompletions<'a> {
    client: &'a OpenAIClient,
}

impl<'a> ChatCompletions<'a> {
    pub async fn create_stream(
        &self,
        mut request: CreateChatCompletionRequest,
    ) -> Result<ChatCompletionStream, OpenAIError> {
        request.stream = Some(true);
        let url = format!("{}/chat/completions", self.client.config.base_url);
        
        let response = self.client.http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.client.config.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(OpenAIError::Api(format!("{}: {}", status, error_text)));
        }

        let stream = response.bytes_stream();
        Ok(ChatCompletionStream::new(stream))
    }
}

pub struct ChatCompletionStream {
    inner: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, OpenAIError>> + Send>>,
}

impl ChatCompletionStream {
    pub(crate) fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        let inner = SseStream::new(stream);
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl Stream for ChatCompletionStream {
    type Item = Result<ChatCompletionChunk, OpenAIError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

struct SseStream<S> {
    stream: Pin<Box<S>>,
    /// Unparsed bytes from the wire. Kept as raw bytes (not a `String`) so a
    /// chunk boundary splitting a multi-byte UTF-8 character never loses data.
    buffer: Vec<u8>,
}

impl<S> SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    fn new(stream: S) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: Vec::new(),
        }
    }
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<ChatCompletionChunk, OpenAIError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                // Decode only complete lines, so a multi-byte character split
                // across chunk boundaries is decoded once the line is whole.
                let line = String::from_utf8_lossy(&self.buffer[..pos]).into_owned();
                self.buffer.drain(..=pos);

                let line = line.trim();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        return Poll::Ready(None);
                    }
                    match serde_json::from_str::<ChatCompletionChunk>(data) {
                        Ok(chunk) => return Poll::Ready(Some(Ok(chunk))),
                        Err(e) => return Poll::Ready(Some(Err(OpenAIError::SseParse(e.to_string())))),
                    }
                }
            }

            match self.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.extend_from_slice(&bytes);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(OpenAIError::Http(e))));
                }
                Poll::Ready(None) => {
                    if self.buffer.is_empty() {
                        return Poll::Ready(None);
                    }
                    self.buffer.clear();
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub struct Models<'a> {
    client: &'a OpenAIClient,
}

impl<'a> Models<'a> {
    pub async fn list(&self) -> Result<ModelsResponse, OpenAIError> {
        let url = format!("{}/models", self.client.config.base_url);
        
        let response = self.client.http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.client.config.api_key))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(OpenAIError::Api(format!("{}: {}", status, error_text)));
        }

        let models: ModelsResponse = response.json().await?;
        Ok(models)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<Model>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub object: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub owned_by: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatCompletionRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatCompletionTools>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Ask reasoning-capable models to think less (`low`) or more (`high`).
    /// Omitted for models and gateways that do not understand it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatCompletionRequestMessage {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    Tool(ToolMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ChatCompletionRequestUserMessageContentPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatCompletionMessageToolCalls>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMessage {
    pub tool_call_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatCompletionRequestUserMessageContentPart {
    Text(ChatCompletionRequestMessageContentPartText),
    ImageUrl(ChatCompletionRequestMessageContentPartImage),
    File(ChatCompletionRequestMessageContentPartFile),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequestMessageContentPartText {
    #[serde(rename = "type", default = "default_text_type")]
    pub content_type: String,
    pub text: String,
}

fn default_text_type() -> String {
    "text".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequestMessageContentPartImage {
    #[serde(rename = "type", default = "default_image_type")]
    pub content_type: String,
    pub image_url: ImageUrl,
}

fn default_image_type() -> String {
    "image_url".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequestMessageContentPartFile {
    #[serde(rename = "type", default = "default_file_type")]
    pub content_type: String,
    pub file: FileData,
}

fn default_file_type() -> String {
    "file".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionTool {
    #[serde(rename = "type", default)]
    pub tool_type: String,
    pub function: FunctionObject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionObject {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatCompletionTools {
    Function(ChatCompletionTool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatCompletionMessageToolCalls {
    Function(ChatCompletionMessageToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionMessageToolCall {
    pub id: String,
    /// The OpenAI API requires `"type": "function"` on every tool call.
    #[serde(rename = "type", default = "default_tool_call_type")]
    pub tool_type: String,
    pub function: FunctionCall,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionObjectArgs {
    name: String,
    description: Option<String>,
    parameters: Value,
}

impl FunctionObjectArgs {
    pub fn default() -> Self {
        Self {
            name: String::new(),
            description: None,
            parameters: Value::Object(serde_json::Map::new()),
        }
    }
    
    pub fn name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }
    
    pub fn description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }
    
    pub fn parameters(mut self, parameters: Value) -> Self {
        self.parameters = parameters;
        self
    }
    
    pub fn build(self) -> Result<FunctionObject, OpenAIError> {
        if self.name.is_empty() {
            return Err(OpenAIError::Build("name is required".to_string()));
        }
        Ok(FunctionObject {
            name: self.name,
            description: self.description.unwrap_or_default(),
            parameters: self.parameters,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequestSystemMessageArgs {
    content: Option<String>,
}

impl Default for ChatCompletionRequestSystemMessageArgs {
    fn default() -> Self {
        Self { content: None }
    }
}

impl ChatCompletionRequestSystemMessageArgs {
    pub fn content(mut self, content: String) -> Self {
        self.content = Some(content);
        self
    }
    
    pub fn build(self) -> Result<SystemMessage, OpenAIError> {
        Ok(SystemMessage {
            content: self.content.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequestUserMessageArgs {
    content: Option<Vec<ChatCompletionRequestUserMessageContentPart>>,
}

impl Default for ChatCompletionRequestUserMessageArgs {
    fn default() -> Self {
        Self { content: None }
    }
}

impl ChatCompletionRequestUserMessageArgs {
    pub fn content(mut self, content: Vec<ChatCompletionRequestUserMessageContentPart>) -> Self {
        self.content = Some(content);
        self
    }
    
    pub fn build(self) -> Result<UserMessage, OpenAIError> {
        Ok(UserMessage {
            content: self.content.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequestToolMessageArgs {
    tool_call_id: Option<String>,
    content: Option<String>,
}

impl Default for ChatCompletionRequestToolMessageArgs {
    fn default() -> Self {
        Self {
            tool_call_id: None,
            content: None,
        }
    }
}

impl ChatCompletionRequestToolMessageArgs {
    pub fn tool_call_id(mut self, tool_call_id: String) -> Self {
        self.tool_call_id = Some(tool_call_id);
        self
    }
    
    pub fn content(mut self, content: String) -> Self {
        self.content = Some(content);
        self
    }
    
    pub fn build(self) -> Result<ToolMessage, OpenAIError> {
        Ok(ToolMessage {
            tool_call_id: self.tool_call_id.ok_or_else(|| OpenAIError::Build("tool_call_id is required".to_string()))?,
            content: self.content.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: u32,
    pub delta: ChatDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    // Reasoning models stream their thinking under different field names:
    // GLM (and some other gateways) use "reasoning", DeepSeek-style APIs use
    // "reasoning_content". Accept both on deserialize; serialize as
    // "reasoning_content" to preserve the existing wire behavior.
    #[serde(
        rename = "reasoning_content",
        alias = "reasoning",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatCompletionMessageToolCallChunk>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionMessageToolCallChunk {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub function_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCallStream>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallStream {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// A stream of canned body chunks, typed as the SSE source expects.
    fn body_stream(chunks: Vec<Bytes>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        futures::stream::iter(chunks.into_iter().map(Ok).collect::<Vec<_>>())
    }

    #[test]
    fn chat_delta_parses_glm_reasoning_field() {
        // GLM-style gateways stream thinking under "reasoning".
        let delta: ChatDelta = serde_json::from_str(r#"{"reasoning":"thinking..."}"#).unwrap();
        assert_eq!(delta.reasoning_content.as_deref(), Some("thinking..."));
    }

    #[test]
    fn chat_delta_parses_deepseek_reasoning_content_field() {
        // DeepSeek-style APIs use "reasoning_content"; must keep working.
        let delta: ChatDelta =
            serde_json::from_str(r#"{"reasoning_content":"thinking..."}"#).unwrap();
        assert_eq!(delta.reasoning_content.as_deref(), Some("thinking..."));
    }

    #[tokio::test]
    async fn sse_stream_reassembles_utf8_split_across_chunks() {
        // A multi-byte character ("好" = 3 bytes) split across two network
        // chunks must not be dropped.
        let full = format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "1",
                "choices": [{"index": 0, "delta": {"content": "你好"}}]
            })
        );
        let split_at = full.find("好").unwrap() + 1; // mid-character cut
        let bytes = full.as_bytes();
        let chunks = vec![
            Bytes::copy_from_slice(&bytes[..split_at]),
            Bytes::copy_from_slice(&bytes[split_at..]),
        ];

        let mut stream = ChatCompletionStream::new(body_stream(chunks));
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev.expect("chunk should parse"));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].choices[0].delta.content.as_deref(),
            Some("你好")
        );
    }

    #[tokio::test]
    async fn sse_stream_parses_glm_reasoning_chunks() {
        // Regression: GLM streams thinking as "delta":{"reasoning":...};
        // chunks must parse (not be silently ignored).
        let sse = concat!(
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"The\"}}]}\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\" user\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut stream = ChatCompletionStream::new(body_stream(vec![Bytes::from_static(
            sse.as_bytes(),
        )]));
        let mut reasoning = String::new();
        while let Some(ev) = stream.next().await {
            let chunk = ev.expect("chunk should parse");
            if let Some(r) = &chunk.choices[0].delta.reasoning_content {
                reasoning.push_str(r);
            }
        }
        assert_eq!(reasoning, "The user");
    }
}
