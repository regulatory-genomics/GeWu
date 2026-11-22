//! Anthropic API types, HTTP client, and SSE parsing.
//!
//! This module provides types for interacting with Anthropic's Messages API,
//! including request/response structures, streaming support via Server-Sent Events,
//! and an HTTP client implementation.
//!
//! # Request Flow
//!
//! 1. Create a [`Client`] with [`AnthropicConfig`]
//! 2. Use [`Client::messages()`] to get a [`Messages`] handle
//! 3. Call [`Messages::create_stream()`] to get a streaming response
//!
//! # Extended Thinking
//!
//! Some models support extended thinking via beta headers.

/// Stream- and request-level failure type for the Anthropic provider.
pub(crate) type Failure = Box<dyn std::error::Error + Send + Sync>;

use bytes::Bytes;
use futures::Stream;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

/// Errors that can occur when interacting with the Anthropic API.
#[derive(Error, Debug)]
pub enum AnthropicError {
    /// HTTP request failed (network error, timeout, etc.)
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The provider rejected the request with a 429 rate limit. Carries the
    /// parsed human message and the `Retry-After` hint when one was sent, so
    /// the retry layer can decide whether waiting is worthwhile.
    #[error("provider rate limit (429): {message}")]
    RateLimit {
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
    /// Error building the request (e.g. the wire payload failed to parse).
    /// Treated as permanent: a re-send fails the same way.
    #[error("build error: {0}")]
    Build(String),
    /// An error event inside a streaming response. `transient` marks
    /// provider-side outages (`overloaded_error`, `api_error`,
    /// `rate_limit_error`, `timeout_error`) that a fresh request can
    /// plausibly survive; anything else is permanent.
    #[error("provider stream error: {message}")]
    Stream {
        transient: bool,
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// A known SSE event could not be parsed.
    #[error("SSE parse error: {0}")]
    SseParse(String),
}

/// Builds the rate-limit error for a 429 response body.
///
/// `retry_after` is the `Retry-After` hint extracted from the headers before
/// the body was consumed. The hint is advisory only: the retry layer clamps
/// it to its per-wait cap, so every rate limit is retried indefinitely.
pub(crate) fn rate_limit_error(
    retry_after: Option<std::time::Duration>,
    body: &str,
) -> AnthropicError {
    let message = crate::http::response_message(body);
    AnthropicError::RateLimit {
        message,
        retry_after,
    }
}

impl AnthropicError {
    /// Maps a non-429 failure response to its typed variant. Server-side
    /// statuses (5xx) and request timeouts (408) become
    /// [`AnthropicError::Server`] (retryable — a fresh request can plausibly
    /// succeed); every other client error becomes [`AnthropicError::Client`]
    /// (permanent).
    pub(crate) fn status_error(
        status: reqwest::StatusCode,
        retry_after: Option<std::time::Duration>,
        body: &str,
    ) -> AnthropicError {
        let message = crate::http::response_message(body);
        if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
            AnthropicError::Server {
                status,
                retry_after,
                message,
            }
        } else {
            AnthropicError::Client { status, message }
        }
    }
}

impl gewu_core::traits::RetryClassify for AnthropicError {
    fn retry_kind(&self) -> gewu_core::types::RetryKind {
        match self {
            AnthropicError::Server { status, .. } | AnthropicError::Client { status, .. } => {
                gewu_core::types::RetryKind::Status(status.as_u16())
            }
            AnthropicError::RateLimit { .. } => gewu_core::types::RetryKind::RateLimit,
            AnthropicError::Stream { .. } => gewu_core::types::RetryKind::Stream,
            AnthropicError::Http(_) => gewu_core::types::RetryKind::Transport,
            AnthropicError::SseParse(_) => gewu_core::types::RetryKind::Parse,
            // Permanent; never reaches a retry notice.
            AnthropicError::Build(_) => gewu_core::types::RetryKind::Other,
        }
    }
}

/// Configuration for the Anthropic API client.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub beta: Option<String>,
}

impl AnthropicConfig {
    /// Creates a new configuration.
    ///
    /// # Arguments
    ///
    /// * `api_key` - Your Anthropic API key.
    /// * `base_url` - The base URL of the API (e.g., <https://api.anthropic.com>).
    ///
    /// A trailing `/v1` is stripped so that endpoints configured either with
    /// (`https://host/v1`) or without (`https://host`) the version prefix both
    /// work, since this client appends `/v1/messages` itself.
    pub fn new(api_key: &str, base_url: &str) -> Self {
        let base_url = base_url.trim_end_matches('/');
        let base_url = base_url.strip_suffix("/v1").unwrap_or(base_url);
        Self {
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
            beta: None,
        }
    }
}

/// HTTP client for Anthropic API requests.
#[derive(Debug)]
pub struct Client {
    override_key: tokio::sync::watch::Sender<Option<String>>,
    /// The configuration used by this client.
    pub config: AnthropicConfig,
    http_client: HttpClient,
}

impl Client {
    pub(crate) fn update_api_key(&self, key: &str) {
        self.override_key.send_replace(Some(key.to_owned()));
    }
    fn api_key(&self) -> String {
        self.override_key
            .borrow()
            .as_ref()
            .unwrap_or(&self.config.api_key)
            .clone()
    }
    pub fn with_config(config: AnthropicConfig) -> Self {
        Self {
            override_key: tokio::sync::watch::channel(None).0,
            config,
            http_client: crate::http::client(),
        }
    }

    pub fn messages(&self) -> Messages<'_> {
        Messages { client: self }
    }

    pub fn models(&self) -> Models<'_> {
        Models { client: self }
    }
}

pub struct Messages<'a> {
    client: &'a Client,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateMessageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<serde_json::Value>,
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum AnthropicMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    Text {
        text: String,
    },
    /// Inline base64 image (`type: "image"` on the wire).
    #[serde(rename = "image")]
    Image {
        source: SourceBlock,
    },
    /// Inline base64 document, e.g. PDF (`type: "document"` on the wire).
    #[serde(rename = "document")]
    Document {
        source: SourceBlock,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBlock {
    #[serde(rename = "type", default = "default_source_type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

fn default_source_type() -> String {
    "base64".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "input_schema")]
    pub input_schema: Value,
}

impl<'a> Messages<'a> {
    pub async fn create_stream(
        &self,
        request: CreateMessageRequest,
    ) -> Result<AnthropicStream, AnthropicError> {
        let url = format!("{}/v1/messages", self.client.config.base_url);

        let mut req_builder = self
            .client
            .http_client
            .post(&url)
            .header("x-api-key", self.client.api_key())
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("anthropic-version", "2023-06-01");

        if let Some(ref beta) = self.client.config.beta {
            req_builder = req_builder.header("anthropic-beta", beta);
        }

        req_builder = req_builder.json(&request);

        let response = req_builder.send().await?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::http::retry_after(&response);
            let error_text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limit_error(retry_after, &error_text));
            }
            return Err(AnthropicError::status_error(
                status,
                retry_after,
                &error_text,
            ));
        }

        let stream = response.bytes_stream();
        Ok(AnthropicStream::new(stream))
    }
}

pub struct AnthropicStream {
    inner: Pin<Box<dyn Stream<Item = Result<AnthropicChunk, AnthropicError>> + Send>>,
}

impl AnthropicStream {
    pub(crate) fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        let inner = AnthropicSseStream::new(stream);
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl Stream for AnthropicStream {
    type Item = Result<AnthropicChunk, AnthropicError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

struct AnthropicSseStream<S> {
    stream: Pin<Box<S>>,
    /// Unparsed bytes from the wire.
    buffer: String,
    /// The most recent `event:` field, pending until its payload arrives.
    current_event: Option<String>,
    /// Accumulated `data:` payload for the current event.
    data_buffer: String,
}

impl<S> AnthropicSseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    fn new(stream: S) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: String::new(),
            current_event: None,
            data_buffer: String::new(),
        }
    }

    /// Processes one SSE line, returning a chunk when an event completes.
    ///
    /// Line-based (not pair-based): the `event:` field is remembered across
    /// transport chunk boundaries so an event split between its `event:` and
    /// `data:` lines is never dropped.
    fn process_line(&mut self, line: &str) -> Option<Result<AnthropicChunk, AnthropicError>> {
        let line = line.trim();
        if line.is_empty() {
            // Blank line terminates the event per the SSE spec.
            return self.take_event();
        }
        if line.starts_with(':') {
            // Comment / keep-alive.
            return None;
        }
        if let Some(value) = line.strip_prefix("event:") {
            // Flush a previous event that was not terminated by a blank line,
            // then record the new event type.
            let flushed = self.take_event();
            self.current_event = Some(value.trim().to_string());
            return flushed;
        }
        if let Some(value) = line.strip_prefix("data:") {
            // The single space after the colon is optional per the SSE spec.
            let value = value.strip_prefix(' ').unwrap_or(value);
            if !self.data_buffer.is_empty() {
                self.data_buffer.push('\n');
            }
            self.data_buffer.push_str(value);
            return None;
        }
        // Other fields (`id:`, `retry:`) are ignored.
        None
    }

    /// Dispatches the pending event, consuming any buffered state.
    fn take_event(&mut self) -> Option<Result<AnthropicChunk, AnthropicError>> {
        let event_type = self.current_event.take()?;
        let data = std::mem::take(&mut self.data_buffer);

        match event_type.as_str() {
            // Terminal marker events carry no data payload.
            "content_block_stop" => Some(Ok(AnthropicChunk::ContentBlockStop(()))),
            "message_stop" => Some(Ok(AnthropicChunk::MessageStop)),
            "message_start"
            | "content_block_start"
            | "content_block_delta"
            | "message_delta"
            | "error" => {
                if data.is_empty() {
                    return None;
                }
                let parsed = match event_type.as_str() {
                    "message_start" => serde_json::from_str::<MessageStartChunk>(&data)
                        .map(AnthropicChunk::MessageStart),
                    "content_block_start" => serde_json::from_str::<ContentBlockStartChunk>(&data)
                        .map(AnthropicChunk::ContentBlockStart),
                    "content_block_delta" => serde_json::from_str::<ContentBlockDeltaChunk>(&data)
                        .map(AnthropicChunk::ContentBlockDelta),
                    "message_delta" => serde_json::from_str::<MessageDeltaChunk>(&data)
                        .map(AnthropicChunk::MessageDelta),
                    "error" => serde_json::from_str::<ErrorChunk>(&data).map(AnthropicChunk::Error),
                    _ => unreachable!("event type filtered above"),
                };
                match parsed {
                    Ok(chunk) => Some(Ok(chunk)),
                    Err(e) => Some(Err(AnthropicError::SseParse(format!(
                        "failed to parse {} event: {}",
                        event_type, e
                    )))),
                }
            }
            // Unknown event types (e.g. `ping`) are ignored.
            _ => None,
        }
    }
}

impl<S> Stream for AnthropicSseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<AnthropicChunk, AnthropicError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Process every complete line already buffered so events are
            // never lost at transport chunk boundaries.
            while let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].trim_end_matches('\r').to_string();
                self.buffer.drain(..=pos);
                if let Some(event) = self.process_line(&line) {
                    return Poll::Ready(Some(event));
                }
            }

            match self.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        self.buffer.push_str(s);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(AnthropicError::Http(e))));
                }
                Poll::Ready(None) => {
                    // A final line without a trailing newline is still a
                    // complete SSE field; process it before ending.
                    if !self.buffer.is_empty() {
                        let line = std::mem::take(&mut self.buffer);
                        if let Some(event) = self.process_line(line.trim_end_matches('\r')) {
                            return Poll::Ready(Some(event));
                        }
                    }
                    // Flush a trailing event that lacked its blank-line terminator.
                    if let Some(event) = self.take_event() {
                        return Poll::Ready(Some(event));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicChunk {
    MessageStart(MessageStartChunk),
    ContentBlockStart(ContentBlockStartChunk),
    ContentBlockDelta(ContentBlockDeltaChunk),
    #[serde(skip)]
    ContentBlockStop(()),
    MessageDelta(MessageDeltaChunk),
    MessageStop,
    Error(ErrorChunk),
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageStartChunk {
    #[serde(rename = "message")]
    pub _message: Message,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(rename = "id")]
    pub _id: String,
    #[serde(rename = "role")]
    pub _role: String,
    #[serde(rename = "content")]
    pub _content: Vec<Value>,
    /// Input/output token accounting for the request. Present on the
    /// `message_start` payload; some proxies zero the cache fields here and
    /// only report real numbers on the final `message_delta`.
    #[serde(default)]
    pub usage: Option<MessageUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlockStartChunk {
    pub index: u32,
    pub content_block: ContentBlockStart,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStart {
    Text {
        #[serde(rename = "text")]
        _text: String,
    },
    ToolUse {
        id: String,
        name: String,
        /// The complete `input` object when Anthropic sends it on the start
        /// block instead of (or before) `input_json_delta` fragments.
        #[serde(default)]
        input: Option<Value>,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        // Anthropic sends the signature as `signature_delta` events; the
        // start block may omit it entirely. Proxies may also include it.
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlockDeltaChunk {
    pub index: u32,
    pub delta: ContentDelta,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDeltaChunk {
    pub delta: MessageDelta,
    #[serde(rename = "usage")]
    pub _usage: MessageUsage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDelta {
    pub stop_reason: Option<String>,
}

/// Token accounting from `message_start.message.usage` and
/// `message_delta.usage`.
///
/// Cache fields are defaulted (not optional) so payloads from proxies that
/// omit them still parse; a missing field reads as 0, matching providers
/// that report no caching.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Prompt tokens served from the prompt cache (cache hits).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Prompt tokens written to the prompt cache by this request.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorChunk {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

/// Classifies an in-stream error event's payload.
///
/// The default is transient: provider-side outages (`overloaded_error` —
/// Anthropic's 529, `api_error`, in-stream rate limits, timeouts) and
/// unknown error kinds retry, so future error modes self-heal instead of
/// killing the ask. Only known request-level failures a re-send cannot fix
/// (invalid request, auth, permissions) are permanent and surface to the
/// user.
pub(crate) fn stream_error(detail: &ErrorDetail) -> AnthropicError {
    // Only known request-level failures are permanent; everything else —
    // including unknown kinds — is transient so a fresh request can
    // plausibly survive it.
    let transient = !matches!(
        detail.error_type.as_str(),
        "invalid_request_error"
            | "authentication_error"
            | "permission_error"
            | "not_found_error"
            | "request_too_large"
    );
    AnthropicError::Stream {
        transient,
        message: format!("{}: {}", detail.error_type, detail.message),
        retry_after: None,
    }
}

pub struct Models<'a> {
    client: &'a Client,
}

impl<'a> Models<'a> {
    pub async fn list(&self) -> Result<ModelsResponse, AnthropicError> {
        let url = format!("{}/v1/models", self.client.config.base_url);

        let response = self
            .client
            .http_client
            .get(&url)
            .header("x-api-key", self.client.api_key())
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::http::retry_after(&response);
            let error_text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_limit_error(retry_after, &error_text));
            }
            return Err(AnthropicError::status_error(
                status,
                retry_after,
                &error_text,
            ));
        }

        let models: ModelsResponse = response.json().await?;
        Ok(models)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(rename = "type")]
    pub _model_type: String,
    #[serde(rename = "display_name")]
    pub _display_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// A stream of canned body chunks, typed as the SSE source expects.
    fn body_stream(chunks: Vec<Bytes>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        futures::stream::iter(chunks.into_iter().map(Ok).collect::<Vec<_>>())
    }

    async fn collect(chunks: Vec<Bytes>) -> Vec<AnthropicChunk> {
        let stream = AnthropicSseStream::new(body_stream(chunks));
        stream
            .filter_map(|r| async { r.ok() })
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn message_delta_parses_real_wire_payload() {
        // Regression: the usage fields are named `usage`/`output_tokens` on
        // the wire, not `_usage`/`_output_tokens`. A real payload must parse
        // and surface its stop reason, or Finish never fires.
        let sse = concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":42}}"#,
            "\n\n",
        );
        let events = collect(vec![Bytes::from_static(sse.as_bytes())]).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicChunk::MessageDelta(chunk) => {
                assert_eq!(chunk.delta.stop_reason.as_deref(), Some("max_tokens"));
            }
            other => panic!("expected MessageDelta, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn thinking_block_start_without_signature_parses() {
        // Regression: real Anthropic streams deliver the signature through
        // `signature_delta`; requiring it on `content_block_start` failed
        // every extended-thinking request with an SSE parse error.
        let sse = concat!(
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            "\n\n",
        );
        let events = collect(vec![Bytes::from_static(sse.as_bytes())]).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicChunk::ContentBlockStart(chunk) => match &chunk.content_block {
                ContentBlockStart::Thinking {
                    thinking,
                    signature,
                } => {
                    assert!(thinking.is_empty());
                    assert!(signature.is_empty());
                }
                other => panic!("expected Thinking, got {other:?}"),
            },
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_split_across_chunk_boundaries_is_not_dropped() {
        // Regression: the transport chunk ends between the `event:` and
        // `data:` lines. The parser must remember the pending event instead
        // of dropping it.
        let chunk1 = Bytes::from_static(b"event: content_block_delta\n");
        let chunk2 = Bytes::from_static(
            br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        );
        let chunk3 = Bytes::from_static(b"\n\n");
        let events = collect(vec![chunk1, chunk2, chunk3]).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicChunk::ContentBlockDelta(chunk) => match &chunk.delta {
                ContentDelta::Text { text } => assert_eq!(text, "hello"),
                other => panic!("expected TextDelta, got {:?}", other),
            },
            other => panic!("expected ContentBlockDelta, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn keep_alive_lines_between_event_and_data_are_skipped() {
        // A keep-alive comment (or id/retry field) between the event and its
        // data must not consume the event.
        let sse = concat!(
            "event: message_delta\n",
            ": keep-alive\n",
            "id: 7\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            "\n\n",
        );
        let events = collect(vec![Bytes::from_static(sse.as_bytes())]).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AnthropicChunk::MessageDelta(_)));
    }

    #[tokio::test]
    async fn data_field_without_space_is_accepted() {
        let sse = concat!("event:content_block_stop\n", "event: message_stop\n",);
        let events = collect(vec![Bytes::from_static(sse.as_bytes())]).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AnthropicChunk::ContentBlockStop(())));
        assert!(matches!(events[1], AnthropicChunk::MessageStop));
    }

    #[tokio::test]
    async fn final_event_without_trailing_blank_line_is_delivered() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"m1","role":"assistant","content":[]}}"#,
            "\n\n",
            "event: message_stop\n",
        );
        let events = collect(vec![Bytes::from_static(sse.as_bytes())]).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AnthropicChunk::MessageStart(_)));
        assert!(matches!(events[1], AnthropicChunk::MessageStop));
    }
}
