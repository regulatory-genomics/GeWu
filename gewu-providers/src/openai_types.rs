use bytes::Bytes;
use futures::Stream;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OpenAIError {
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
    #[error("SSE stream done")]
    Done,
}

#[derive(Debug, Clone)]
pub struct OpenAIConfig {
    api_key: String,
    base_url: String,
}

impl OpenAIConfig {
    pub fn new(api_key: &str, base_url: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Debug)]
pub struct Client {
    config: OpenAIConfig,
    http_client: HttpClient,
}

impl Client {
    pub fn with_config(config: OpenAIConfig) -> Self {
        Self {
            config,
            http_client: HttpClient::new(),
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
    client: &'a Client,
}

impl<'a> ChatCompletions<'a> {
    pub async fn create_stream(
        &self,
        mut request: CreateChatCompletionRequest,
    ) -> Result<ChatCompletionStream, OpenAIError> {
        request.stream = Some(true);
        let url = format!("{}/chat/completions", self.client.config.base_url);

        let response = self
            .client
            .http_client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.client.config.api_key),
            )
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

    pub async fn create(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, OpenAIError> {
        let url = format!("{}/chat/completions", self.client.config.base_url);

        let response = self
            .client
            .http_client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.client.config.api_key),
            )
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(OpenAIError::Api(format!("{}: {}", status, error_text)));
        }

        let chunk: ChatCompletionResponse = response.json().await?;
        Ok(chunk)
    }
}

pub struct ChatCompletionStream {
    inner: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, OpenAIError>> + Send>>,
}

impl ChatCompletionStream {
    fn new<S>(stream: S) -> Self
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
    buffer: String,
}

impl<S> SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    fn new(stream: S) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: String::new(),
        }
    }

    fn try_parse_event(event_block: &str) -> Option<Result<ChatCompletionChunk, OpenAIError>> {
        let mut data_parts = Vec::new();
        for line in event_block.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                data_parts.push(data);
            } else if let Some(data) = line.strip_prefix("data:") {
                data_parts.push(data.trim_start());
            }
        }
        if data_parts.is_empty() {
            return None;
        }
        let data = data_parts.join("");
        if data == "[DONE]" {
            return Some(Err(OpenAIError::Done));
        }
        match serde_json::from_str::<ChatCompletionChunk>(&data) {
            Ok(chunk) => Some(Ok(chunk)),
            Err(e) => {
                tracing::warn!(
                    "SSE: skipping malformed event (len={}, err={})",
                    data.len(),
                    e
                );
                None
            }
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
            if let Some(pos) = self.buffer.find("\n\n") {
                let event_block = self.buffer[..pos].to_string();
                self.buffer = self.buffer[pos + 2..].to_string();

                match Self::try_parse_event(&event_block) {
                    Some(Err(OpenAIError::Done)) => return Poll::Ready(None),
                    Some(result) => return Poll::Ready(Some(result)),
                    None => continue,
                }
            }

            match self.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        self.buffer.push_str(s);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(OpenAIError::Http(e))));
                }
                Poll::Ready(None) => {
                    let remaining = std::mem::take(&mut self.buffer);
                    if remaining.trim().is_empty() {
                        return Poll::Ready(None);
                    }
                    match Self::try_parse_event(&remaining) {
                        Some(Err(OpenAIError::Done)) | None => return Poll::Ready(None),
                        Some(result) => return Poll::Ready(Some(result)),
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub struct Models<'a> {
    client: &'a Client,
}

impl<'a> Models<'a> {
    pub async fn list(&self) -> Result<ModelsResponse, OpenAIError> {
        let url = format!("{}/models", self.client.config.base_url);

        let response = self
            .client
            .http_client
            .get(&url)
            .header(
                "Authorization",
                format!("Bearer {}", self.client.config.api_key),
            )
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
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionCall,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateChatCompletionRequestArgs {
    model: Option<String>,
    messages: Option<Vec<ChatCompletionRequestMessage>>,
    tools: Option<Vec<ChatCompletionTools>>,
    stream: Option<bool>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

impl Default for CreateChatCompletionRequestArgs {
    fn default() -> Self {
        Self {
            model: None,
            messages: None,
            tools: None,
            stream: None,
            max_tokens: None,
            temperature: None,
        }
    }
}

impl CreateChatCompletionRequestArgs {
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    pub fn messages(mut self, messages: Vec<ChatCompletionRequestMessage>) -> Self {
        self.messages = Some(messages);
        self
    }

    pub fn tools(mut self, tools: Vec<ChatCompletionTools>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn stream(mut self, stream: bool) -> Self {
        self.stream = Some(stream);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn build(self) -> Result<CreateChatCompletionRequest, OpenAIError> {
        Ok(CreateChatCompletionRequest {
            model: self
                .model
                .ok_or_else(|| OpenAIError::Build("model is required".to_string()))?,
            messages: self.messages.unwrap_or_default(),
            tools: self.tools,
            stream: self.stream,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
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
pub struct ChatCompletionRequestAssistantMessageArgs {
    content: Option<String>,
    tool_calls: Option<Vec<ChatCompletionMessageToolCalls>>,
}

impl Default for ChatCompletionRequestAssistantMessageArgs {
    fn default() -> Self {
        Self {
            content: None,
            tool_calls: None,
        }
    }
}

impl ChatCompletionRequestAssistantMessageArgs {
    pub fn content(mut self, content: String) -> Self {
        self.content = Some(content);
        self
    }

    pub fn tool_calls(mut self, tool_calls: Vec<ChatCompletionMessageToolCalls>) -> Self {
        self.tool_calls = Some(tool_calls);
        self
    }

    pub fn build(self) -> Result<AssistantMessage, OpenAIError> {
        Ok(AssistantMessage {
            content: self.content,
            tool_calls: self.tool_calls,
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
    #[serde(rename = "reasoning_content", skip_serializing_if = "Option::is_none")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub choices: Vec<ChatCompletionResponseChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponseChoice {
    pub index: u32,
    pub message: ChatCompletionResponseMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponseMessage {
    pub role: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn make_chunk_json(content: &str) -> String {
        format!(
            r#"{{"id":"c1","choices":[{{"index":0,"delta":{{"content":"{}"}}}}]}}"#,
            content
        )
    }

    fn bytes_stream(chunks: Vec<&str>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        futures::stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok(Bytes::from(s.to_string())))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn single_line_events() {
        let raw = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            make_chunk_json("hello"),
            make_chunk_json("world"),
        );
        let stream = SseStream::new(bytes_stream(vec![&raw]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(items[1].is_ok());
        let c0 = items[0].as_ref().unwrap();
        assert_eq!(c0.choices[0].delta.content.as_deref(), Some("hello"));
        let c1 = items[1].as_ref().unwrap();
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some("world"));
    }

    #[tokio::test]
    async fn multi_line_data_event() {
        let json = make_chunk_json("joined");
        let (first, second) = json.split_at(json.len() / 2);
        let raw = format!("data: {}\ndata: {}\n\ndata: [DONE]\n\n", first, second);
        let stream = SseStream::new(bytes_stream(vec![&raw]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("joined")
        );
    }

    #[tokio::test]
    async fn malformed_event_skipped() {
        let raw = format!(
            "data: {{bad json}}\n\ndata: {}\n\ndata: [DONE]\n\n",
            make_chunk_json("ok"),
        );
        let stream = SseStream::new(bytes_stream(vec![&raw]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("ok")
        );
    }

    #[tokio::test]
    async fn stream_end_without_trailing_newlines() {
        let raw = format!("data: {}", make_chunk_json("last"));
        let stream = SseStream::new(bytes_stream(vec![&raw]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("last")
        );
    }

    #[tokio::test]
    async fn chunked_tcp_delivery() {
        let json = make_chunk_json("split");
        let full = format!("data: {}\n\ndata: [DONE]\n\n", json);
        let mid = full.len() / 2;
        let (a, b) = full.split_at(mid);
        let stream = SseStream::new(bytes_stream(vec![a, b]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("split")
        );
    }

    #[tokio::test]
    async fn done_terminates_stream() {
        let raw = format!(
            "data: {}\n\ndata: [DONE]\n\ndata: {}\n\n",
            make_chunk_json("before"),
            make_chunk_json("after"),
        );
        let stream = SseStream::new(bytes_stream(vec![&raw]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("before")
        );
    }

    #[tokio::test]
    async fn comment_lines_ignored() {
        let raw = format!(
            ": keep-alive\n\ndata: {}\n\ndata: [DONE]\n\n",
            make_chunk_json("hi"),
        );
        let stream = SseStream::new(bytes_stream(vec![&raw]));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
    }
}
