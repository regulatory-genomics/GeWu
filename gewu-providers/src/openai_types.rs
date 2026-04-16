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
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<ChatCompletionChunk, OpenAIError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].to_string();
                self.buffer = self.buffer[pos + 1..].to_string();

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
                        Err(e) => {
                            return Poll::Ready(Some(Err(OpenAIError::SseParse(e.to_string()))));
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
