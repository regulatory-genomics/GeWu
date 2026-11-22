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
    /// The provider rejected the request with a 429 rate limit. Carries the
    /// parsed human message and the `Retry-After` hint when one was sent, so
    /// the retry layer can decide whether waiting is worthwhile.
    #[error("provider rate limit (429): {message}")]
    RateLimit {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// An error event inside a streaming response (e.g. the Responses API's
    /// `error` / `response.failed`). `transient` marks provider-side outages
    /// that a fresh request can plausibly survive; `retry_after` carries the
    /// wait hint from the event's embedded headers when one was sent.
    #[error("provider stream error: {message}")]
    Stream {
        transient: bool,
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// The server failed to serve the request (5xx, or a 408 request
    /// timeout): a fresh request can plausibly succeed. `retry_after`
    /// carries a provider-suggested wait when the response included one.
    #[error("API error: {status}: {message}")]
    Server {
        status: reqwest::StatusCode,
        retry_after: Option<std::time::Duration>,
        message: String,
    },
    /// The request itself was rejected (4xx other than 408/429): retrying
    /// cannot fix it, so it surfaces immediately.
    #[error("API error: {status}: {message}")]
    Client {
        status: reqwest::StatusCode,
        message: String,
    },
    /// Error building the request (missing required fields, etc.).
    #[error("Build error: {0}")]
    Build(String),
}

/// Builds the rate-limit error for a 429 response body.
///
/// `retry_after` is the `Retry-After` hint extracted from the headers before
/// the body was consumed. The hint is advisory only: the retry layer clamps
/// it to its per-wait cap, so every rate limit is retried indefinitely.
pub(crate) fn rate_limit_error(
    retry_after: Option<std::time::Duration>,
    body: &str,
) -> OpenAIError {
    let message = crate::http::response_message(body);
    OpenAIError::RateLimit {
        message,
        retry_after,
    }
}

impl OpenAIError {
    /// Maps a non-429 failure response to its typed variant. Server-side
    /// statuses (5xx) and request timeouts (408) become [`OpenAIError::Server`]
    /// (retryable — a fresh request can plausibly succeed); every other
    /// client error becomes [`OpenAIError::Client`] (permanent).
    pub(crate) fn status_error(
        status: reqwest::StatusCode,
        retry_after: Option<std::time::Duration>,
        body: &str,
    ) -> OpenAIError {
        let message = crate::http::response_message(body);
        if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
            OpenAIError::Server {
                status,
                retry_after,
                message,
            }
        } else {
            OpenAIError::Client { status, message }
        }
    }
}

impl gewu_core::traits::RetryClassify for OpenAIError {
    fn retry_kind(&self) -> gewu_core::types::RetryKind {
        match self {
            OpenAIError::Server { status, .. } | OpenAIError::Client { status, .. } => {
                gewu_core::types::RetryKind::Status(status.as_u16())
            }
            OpenAIError::RateLimit { .. } => gewu_core::types::RetryKind::RateLimit,
            OpenAIError::Stream { .. } => gewu_core::types::RetryKind::Stream,
            OpenAIError::Http(_) => gewu_core::types::RetryKind::Transport,
            OpenAIError::SseParse(_) => gewu_core::types::RetryKind::Parse,
            // Permanent; never reaches a retry notice.
            OpenAIError::Build(_) => gewu_core::types::RetryKind::Other,
        }
    }
}

/// Configuration for the OpenAI API client.
#[derive(Debug, Clone)]
pub struct OpenAIConfig {
    api_key: tokio::sync::watch::Sender<String>,
    base_url: String,
}

impl OpenAIConfig {
    fn authorization(&self) -> String {
        format!("Bearer {}", *self.api_key.borrow())
    }

    /// Creates a new configuration.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your OpenAI API key.
    /// * `base_url` - The base URL of the API (e.g., <https://api.openai.com/v1>).
    pub fn new(api_key: &str, base_url: &str) -> Self {
        Self {
            api_key: tokio::sync::watch::channel(api_key.to_owned()).0,
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
    pub(crate) fn update_api_key(&self, key: &str) {
        self.config.api_key.send_replace(key.to_owned());
    }
    pub fn with_config(config: OpenAIConfig) -> Self {
        Self {
            config,
            http_client: crate::http::client(),
        }
    }

    pub fn chat(&self) -> ChatCompletions<'_> {
        ChatCompletions { client: self }
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
        // Ask for the final usage chunk; without this the stream carries no
        // token accounting (and no cache-hit figures).
        request.stream_options = Some(StreamOptions {
            include_usage: Some(true),
        });
        let url = format!("{}/chat/completions", self.client.config.base_url);

        let response = self
            .client
            .http_client
            .post(&url)
            .header("Authorization", self.client.config.authorization())
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::http::retry_after(&response);
            let error_text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limit_error(retry_after, &error_text));
            }
            return Err(OpenAIError::status_error(status, retry_after, &error_text));
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
            while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                // Decode only complete lines, so a multi-byte character split
                // across chunk boundaries is decoded once the line is whole.
                let line = String::from_utf8_lossy(&self.buffer[..pos]).into_owned();
                self.buffer.drain(..=pos);

                match Self::process_line(&line) {
                    LineOutcome::Continue => {}
                    LineOutcome::Chunk(chunk) => return Poll::Ready(Some(Ok(chunk))),
                    LineOutcome::Error(e) => return Poll::Ready(Some(Err(e))),
                    LineOutcome::Done => return Poll::Ready(None),
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
                    // A final line without a trailing newline is still a
                    // complete SSE field; process it before ending.
                    if !self.buffer.is_empty() {
                        let line = String::from_utf8_lossy(&self.buffer).into_owned();
                        self.buffer.clear();
                        match Self::process_line(&line) {
                            LineOutcome::Chunk(chunk) => return Poll::Ready(Some(Ok(chunk))),
                            LineOutcome::Error(e) => return Poll::Ready(Some(Err(e))),
                            _ => {}
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Result of processing one SSE line.
enum LineOutcome {
    /// Keep scanning for more lines.
    Continue,
    /// A complete data event was parsed.
    Chunk(ChatCompletionChunk),
    /// The event failed to parse.
    Error(OpenAIError),
    /// The `[DONE]` terminator was reached.
    Done,
}

impl<S> SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    fn process_line(raw: &str) -> LineOutcome {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(':') {
            return LineOutcome::Continue;
        }
        // The single space after the colon is optional per the SSE spec;
        // servers and proxies may emit `data:{...}`.
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            if data == "[DONE]" {
                return LineOutcome::Done;
            }
            return match serde_json::from_str::<ChatCompletionChunk>(data) {
                Ok(chunk) => LineOutcome::Chunk(chunk),
                Err(e) => LineOutcome::Error(OpenAIError::SseParse(e.to_string())),
            };
        }
        LineOutcome::Continue
    }
}

pub struct Models<'a> {
    client: &'a OpenAIClient,
}

impl<'a> Models<'a> {
    pub async fn list(&self) -> Result<ModelsResponse, OpenAIError> {
        let url = format!("{}/models", self.client.config.base_url);

        let response = self
            .client
            .http_client
            .get(&url)
            .header("Authorization", self.client.config.authorization())
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::http::retry_after(&response);
            let error_text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limit_error(retry_after, &error_text));
            }
            return Err(OpenAIError::status_error(status, retry_after, &error_text));
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub model: String,
    pub messages: Vec<ChatCompletionRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatCompletionTools>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Set with `stream: true` to have the final chunk carry `usage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// `stream_options` for chat completion requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    /// Stream one extra final chunk carrying `usage` for the whole request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
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
    #[serde(flatten)]
    pub reasoning: std::collections::BTreeMap<String, String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatCompletionRequestSystemMessageArgs {
    content: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatCompletionRequestUserMessageArgs {
    content: Option<Vec<ChatCompletionRequestUserMessageContentPart>>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatCompletionRequestToolMessageArgs {
    tool_call_id: Option<String>,
    content: Option<String>,
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
            tool_call_id: self
                .tool_call_id
                .ok_or_else(|| OpenAIError::Build("tool_call_id is required".to_string()))?,
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
    // Keep the native field name: alternative/custom reasoning fields are
    // retained in `extra` and detected by the provider's stream parser.
    #[serde(rename = "reasoning_content", skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
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
    /// Cache/cost breakdown of the prompt tokens. OpenAI reports
    /// `cached_tokens` under this object; third-party gateways that omit
    /// the whole object must still parse (`#[serde(default)]`).
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Breakdown of the completion tokens. OpenAI reports thinking tokens
    /// as `reasoning_tokens` under this object; gateways that omit it must
    /// still parse.
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// Breakdown of completion tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    /// Output tokens the model spent thinking (not visible content).
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

/// Breakdown of prompt tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    /// Prompt tokens served from the provider's prompt cache (cache hits).
    #[serde(default)]
    pub cached_tokens: Option<u32>,
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
        assert_eq!(
            delta
                .extra
                .get("reasoning")
                .and_then(serde_json::Value::as_str),
            Some("thinking...")
        );
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
        assert_eq!(events[0].choices[0].delta.content.as_deref(), Some("你好"));
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
        let mut stream =
            ChatCompletionStream::new(body_stream(vec![Bytes::from_static(sse.as_bytes())]));
        let mut reasoning = String::new();
        while let Some(ev) = stream.next().await {
            let chunk = ev.expect("chunk should parse");
            if let Some(r) = chunk.choices[0]
                .delta
                .extra
                .get("reasoning")
                .and_then(serde_json::Value::as_str)
            {
                reasoning.push_str(r);
            }
        }
        assert_eq!(reasoning, "The user");
    }

    #[tokio::test]
    async fn sse_stream_accepts_data_field_without_space() {
        // The SSE spec makes the single space after the colon optional;
        // `data:{...}` must parse, not be silently skipped.
        let sse = concat!(
            "data:{\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data:[DONE]\n\n",
        );
        let mut stream =
            ChatCompletionStream::new(body_stream(vec![Bytes::from_static(sse.as_bytes())]));
        let mut count = 0;
        while let Some(ev) = stream.next().await {
            let chunk = ev.expect("chunk should parse");
            assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
            count += 1;
        }
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn sse_stream_processes_final_line_without_trailing_newline() {
        // A complete-but-unterminated last event must still be delivered.
        let sse = concat!(
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n",
        );
        let mut stream =
            ChatCompletionStream::new(body_stream(vec![Bytes::from_static(sse.as_bytes())]));
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            let chunk = ev.expect("chunk should parse");
            if let Some(c) = &chunk.choices[0].delta.content {
                text.push_str(c);
            }
        }
        assert_eq!(text, "ab");
    }
}
