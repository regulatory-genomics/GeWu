use bytes::Bytes;
use futures::Stream;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AnthropicError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("SSE parse error: {0}")]
    SseParse(String),
    #[error("API error: {0}")]
    Api(String),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Build error: {0}")]
    Build(String),
}

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    api_key: String,
    base_url: String,
    pub beta: Option<String>,
}

impl AnthropicConfig {
    pub fn new(api_key: &str, base_url: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            beta: None,
        }
    }

    pub fn with_beta(mut self, beta: &str) -> Self {
        self.beta = Some(beta.to_string());
        self
    }
}

#[derive(Debug)]
pub struct Client {
    pub config: AnthropicConfig,
    http_client: HttpClient,
}

impl Client {
    pub fn with_config(config: AnthropicConfig) -> Self {
        Self {
            config,
            http_client: HttpClient::new(),
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

#[derive(Debug, Serialize)]
pub struct CreateMessageRequest {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Source { source: SourceBlock },
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
pub struct ToolUse {
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub tool_type: String,
    pub name: String,
    pub input: Value,
}

fn default_tool_type() -> String {
    "tool_use".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "input_schema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub content: Vec<ResponseContentBlock>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
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
            .header("x-api-key", &self.client.config.api_key)
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
            let error_text = response.text().await.unwrap_or_default();
            return Err(AnthropicError::Api(format!("{}: {}", status, error_text)));
        }

        let stream = response.bytes_stream();
        Ok(AnthropicStream::new(stream))
    }
}

pub struct AnthropicStream {
    inner: Pin<Box<dyn Stream<Item = Result<AnthropicChunk, AnthropicError>> + Send>>,
}

impl AnthropicStream {
    fn new<S>(stream: S) -> Self
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
    buffer: String,
}

impl<S> AnthropicSseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    fn new(stream: S) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: String::new(),
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
            if let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].to_string();
                self.buffer = self.buffer[pos + 1..].to_string();

                let line = line.trim();
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(event_data) = line.strip_prefix("event: ") {
                    let event_type = event_data.trim().to_string();

                    if let Some(pos2) = self.buffer.find('\n') {
                        let data_line = self.buffer[..pos2].to_string();
                        self.buffer = self.buffer[pos2 + 1..].to_string();

                        if let Some(data) = data_line.strip_prefix("data: ") {
                            let data = data.trim();
                            if data.is_empty() {
                                continue;
                            }

                            match event_type.as_str() {
                                "message_start" => {
                                    if let Ok(chunk) =
                                        serde_json::from_str::<MessageStartChunk>(data)
                                    {
                                        return Poll::Ready(Some(Ok(AnthropicChunk::MessageStart(
                                            chunk,
                                        ))));
                                    }
                                }
                                "content_block_start" => {
                                    if let Ok(chunk) =
                                        serde_json::from_str::<ContentBlockStartChunk>(data)
                                    {
                                        return Poll::Ready(Some(Ok(AnthropicChunk::ContentBlockStart(
                                            chunk,
                                        ))));
                                    }
                                }
                                "content_block_delta" => {
                                    if let Ok(chunk) =
                                        serde_json::from_str::<ContentBlockDeltaChunk>(data)
                                    {
                                        return Poll::Ready(Some(Ok(AnthropicChunk::ContentBlockDelta(
                                            chunk,
                                        ))));
                                    }
                                }
                                "content_block_stop" => {
                                    if let Ok(chunk) =
                                        serde_json::from_str::<ContentBlockStopChunk>(data)
                                    {
                                        return Poll::Ready(Some(Ok(AnthropicChunk::ContentBlockStop(
                                            chunk,
                                        ))));
                                    }
                                }
                                "message_delta" => {
                                    if let Ok(chunk) =
                                        serde_json::from_str::<MessageDeltaChunk>(data)
                                    {
                                        return Poll::Ready(Some(Ok(AnthropicChunk::MessageDelta(
                                            chunk,
                                        ))));
                                    }
                                }
                                "message_stop" => {
                                    return Poll::Ready(Some(Ok(AnthropicChunk::MessageStop)));
                                }
                                "error" => {
                                    if let Ok(error_chunk) =
                                        serde_json::from_str::<ErrorChunk>(data)
                                    {
                                        return Poll::Ready(Some(Ok(AnthropicChunk::Error(
                                            error_chunk,
                                        ))));
                                    }
                                }
                                _ => continue,
                            }
                        }
                    }
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

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicChunk {
    MessageStart(MessageStartChunk),
    ContentBlockStart(ContentBlockStartChunk),
    ContentBlockDelta(ContentBlockDeltaChunk),
    ContentBlockStop(ContentBlockStopChunk),
    MessageDelta(MessageDeltaChunk),
    MessageStop,
    Error(ErrorChunk),
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageStartChunk {
    pub message: Message,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub content: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlockStartChunk {
    pub index: u32,
    pub content_block: ContentBlockStart,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStart {
    Text { text: String },
    ToolUse { id: String, name: String },
    Thinking { thinking: String, signature: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlockDeltaChunk {
    pub index: u32,
    pub delta: ContentDelta,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlockStopChunk {
    pub index: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDeltaChunk {
    pub delta: MessageDelta,
    pub usage: MessageUsage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDelta {
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageUsage {
    pub output_tokens: u32,
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
            .header("x-api-key", &self.client.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AnthropicError::Api(format!("{}: {}", status, error_text)));
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
    pub model_type: String,
    pub display_name: Option<String>,
}