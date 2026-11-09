//! OpenAI Responses API with stateless encrypted-reasoning continuity.
use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use gewu_core::{traits::{LlmProvider, StatusSink, Tool}, types::{ContentPart, Message, ProviderEvent, ReasoningReplay, TokenUsage}};
use serde_json::{json, Value};
use std::{collections::BTreeSet, error::Error, sync::{Arc, Mutex}, time::Duration};

type Failure = Box<dyn Error + Send + Sync>;

pub struct OpenAIProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_output_tokens: Option<u32>,
    effort: Option<String>,
    reasoning_summary: bool,
    backoff: Duration,
    max_wait: Duration,
    status: Mutex<Option<StatusSink>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fixture() -> Vec<Value> {
        vec![
            json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"a\"}"}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Inspect the file"}),
            json!({"type":"response.completed","response":{
                "output":[
                    {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Inspect the file"}],"encrypted_content":"encrypted-state"},
                    {"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}","status":"completed"}
                ],
                "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40},"output_tokens":20,"output_tokens_details":{"reasoning_tokens":12}}
            }}),
        ]
    }

    fn sse(events: &[Value]) -> String {
        events.iter().map(|event| format!("event: {}\r\ndata: {event}\r\n\r\n", event["type"].as_str().unwrap())).collect()
    }

    #[tokio::test]
    async fn responses_http_round_trip_replays_encrypted_reasoning_and_tool_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let server = tokio::spawn(async move {
            let responses = [sse(&fixture()), sse(&[json!({"type":"response.completed","response":{"output":[
                {"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Done","annotations":[]}]}
            ]}})])];
            for body in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let header = std::str::from_utf8(&bytes[..end]).unwrap();
                        let length = header.lines().find_map(|line| line.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap();
                        if bytes.len() >= end + 4 + length {
                            assert!(header.starts_with("POST /v1/responses "));
                            tx.send(serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length]).unwrap()).await.unwrap();
                            break;
                        }
                    }
                }
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let provider = OpenAIProvider::new_with_endpoint("key", &format!("http://{address}/v1"), "reasoner")
            .with_reasoning_effort("high").unwrap().with_max_output_tokens(1000).unwrap();
        let mut stream = provider.stream_response(&[Message::User { content: vec![ContentPart::Text { text:"inspect".into() }] }], &[], None).await.unwrap();
        let mut replay = None;
        let mut argument_text = String::new();
        let mut reasoning = String::new();
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                ProviderEvent::ReasoningReplay { replay: state } => replay = Some(state),
                ProviderEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
                ProviderEvent::ToolCallStart { id, name } => { assert_eq!(id,"call_1"); assert_eq!(name,"read"); }
                ProviderEvent::ToolCallDelta { id, delta } => { assert_eq!(id,"call_1"); argument_text.push_str(&delta); }
                ProviderEvent::Usage { usage } => { assert_eq!(usage.input_tokens,60); assert_eq!(usage.cached_tokens,40); assert_eq!(usage.reasoning_tokens,12); }
                _ => {}
            }
        }
        assert_eq!(reasoning,"Inspect the file");
        assert_eq!(argument_text,"{\"path\":\"a\"}");
        let message = Message::Assistant { content: None, reasoning: Some(reasoning), tool_calls: None, replay };
        let restored: Message = serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        let history = [restored.clone(), Message::Tool { tool_call_id:"call_1".into(), tool_name:Some("read".into()), content:"file contents".into(), media:vec![] }];
        let events = provider.stream_response(&history, &[], None).await.unwrap().collect::<Vec<_>>().await;
        assert!(events.iter().any(|event| matches!(event,Ok(ProviderEvent::ContentDelta { delta }) if delta == "Done")));
        let first = rx.recv().await.unwrap();
        assert_eq!(first["store"],false);
        assert_eq!(first["include"][0],"reasoning.encrypted_content");
        assert_eq!(first["max_output_tokens"],1000);
        assert_eq!(first["reasoning"]["effort"],"high");
        let second = rx.recv().await.unwrap();
        assert_eq!(second["input"][0]["encrypted_content"],"encrypted-state");
        assert_eq!(second["input"][1]["call_id"],"call_1");
        assert_eq!(second["input"][2]["type"],"function_call_output");
        assert_eq!(second["input"][2]["output"],"file contents");
        let other = OpenAIProvider::new("key","other").build(&[restored],&[]).unwrap();
        assert_eq!(other["input"][0]["role"], "assistant");
        assert!(!other.to_string().contains("encrypted-state"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handles_utf8_boundaries_crlf_and_final_frame_without_newline() {
        let events = [json!({"type":"response.output_text.delta","item_id":"m","delta":"你好"}),
            json!({"type":"response.completed","response":{"output":[{"type":"message","id":"m","role":"assistant","content":[{"type":"output_text","text":"你好","annotations":[]}]}]}})];
        let body = sse(&events).trim_end().to_string();
        let source = futures::stream::iter(body.into_bytes().into_iter().map(|b| Ok(bytes::Bytes::from(vec![b]))));
        let events = parse_stream(source,"origin".into(),"model".into()).collect::<Vec<_>>().await;
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(events.iter().filter(|e| matches!(e,Ok(ProviderEvent::ContentDelta { .. }))).count(),1);
    }

    #[test]
    fn incomplete_errors_and_attachment_validation() {
        let mut state = ResponseState::default();
        let events = state.process(&json!({"type":"response.incomplete","response":{"output":[],"incomplete_details":{"reason":"max_output_tokens"}}}).to_string(),"origin","model").unwrap();
        assert!(events.iter().any(|e| matches!(e,ProviderEvent::Finish { reason: Some(reason) } if reason == "max_tokens")));
        assert!(ResponseState::default().process(r#"{"type":"response.failed","response":{"error":{"message":"failed"}}}"#,"o","m").is_err());
        let arbitrary = OpenAIProvider::new("key","m").with_reasoning_effort("vendor-defined").unwrap().build(&[], &[]).unwrap();
        assert_eq!(arbitrary["reasoning"]["effort"], "vendor-defined");
        assert!(OpenAIProvider::new("key","m").with_max_output_tokens(0).is_err());
        let provider = OpenAIProvider::new("key","m");
        let payload = provider.build(&[Message::User { content:vec![ContentPart::Image { data:"a".into(),mime_type:"image/png".into() },ContentPart::File { data:"b".into(),mime_type:"application/pdf".into() }] }],&[]).unwrap();
        assert_eq!(payload["input"][0]["content"][0]["type"],"input_image");
        assert_eq!(payload["input"][0]["content"][1]["type"],"input_file");
        assert!(provider.build(&[Message::User { content:vec![ContentPart::Audio { data:"a".into(),mime_type:"audio/wav".into() }] }],&[]).is_err());
    }

    #[tokio::test]
    async fn truncated_stream_is_an_error() {
        let source = futures::stream::iter(vec![Ok(bytes::Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n"))]);
        let events = parse_stream(source,"origin".into(),"model".into()).collect::<Vec<_>>().await;
        assert!(events.last().unwrap().is_err());
    }

    #[test]
    fn summary_defaults_and_effort_are_independent() {
        let default = OpenAIProvider::new("key", "gpt").build(&[], &[]).unwrap();
        assert_eq!(default["reasoning"], json!({"summary":"auto"}));
        let disabled = OpenAIProvider::new("key", "gpt").with_reasoning_summary(false).build(&[], &[]).unwrap();
        assert!(disabled.get("reasoning").is_none());
        assert_eq!(disabled["include"], json!(["reasoning.encrypted_content"]));
        let effort_only = OpenAIProvider::new("key", "gpt").with_reasoning_summary(false)
            .with_reasoning_effort("high").unwrap().build(&[], &[]).unwrap();
        assert_eq!(effort_only["reasoning"], json!({"effort":"high"}));
        let both = OpenAIProvider::new("key", "gpt").with_reasoning_effort("low").unwrap().build(&[], &[]).unwrap();
        assert_eq!(both["reasoning"], json!({"summary":"auto", "effort":"low"}));
    }
}

impl OpenAIProvider {
    pub fn new(api_key: &str, model: &str) -> Self {
        Self::new_with_endpoint(api_key, "https://api.openai.com/v1", model)
    }

    pub fn new_with_endpoint(api_key: &str, base_url: &str, model: &str) -> Self {
        Self {
            client: crate::http::client(), api_key: api_key.into(),
            base_url: base_url.trim_end_matches('/').into(), model: model.into(),
            max_output_tokens: None, effort: None, reasoning_summary: true, backoff: crate::http::RETRY_BACKOFF,
            max_wait: crate::http::MAX_RETRY_WAIT, status: Mutex::new(None),
        }
    }

    pub fn with_max_output_tokens(mut self, limit: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(limit > 0, "max_output_tokens must be positive");
        self.max_output_tokens = Some(limit);
        Ok(self)
    }

    pub fn with_reasoning_effort(mut self, effort: &str) -> anyhow::Result<Self> {
        self.effort = Some(effort.into());
        Ok(self)
    }

    /// Request a visible reasoning summary without overriding model effort.
    pub fn with_reasoning_summary(mut self, enabled: bool) -> Self {
        self.reasoning_summary = enabled;
        self
    }

    pub fn with_retry_backoff(mut self, delay: Duration) -> Self { self.backoff = delay; self }
    pub fn with_retry_max_wait(mut self, delay: Duration) -> Self { self.max_wait = delay; self }

    fn origin(&self) -> String { format!("responses:{}", self.base_url) }

    async fn request(&self, path: &str, body: Option<&Value>) -> Result<reqwest::Response, crate::openai::OpenAIError> {
        let request = match body {
            Some(body) => self.client.post(format!("{}{path}", self.base_url)).json(body).header("Accept", "text/event-stream"),
            None => self.client.get(format!("{}{path}", self.base_url)),
        };
        let response = request.bearer_auth(&self.api_key).send().await?;
        if response.status().is_success() { return Ok(response); }
        let status = response.status();
        let retry_after = crate::http::retry_after(&response);
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(crate::openai::OpenAIError::RateLimit { message: crate::http::rate_limit_message(&text), retry_after });
        }
        Err(crate::openai::OpenAIError::Api(format!("{status}: {text}")))
    }
}

fn content(parts: &[ContentPart]) -> Result<Vec<Value>, Failure> {
    parts.iter().map(|part| Ok(match part {
        ContentPart::Text { text } | ContentPart::Reminder { text } => json!({"type":"input_text", "text":text}),
        ContentPart::Image { data, mime_type } => json!({"type":"input_image", "image_url":format!("data:{mime_type};base64,{data}")}),
        ContentPart::File { data, mime_type } if mime_type == "application/pdf" => json!({"type":"input_file", "filename":"attachment.pdf", "file_data":format!("data:{mime_type};base64,{data}")}),
        _ => return Err(format!("Responses API input does not support {} attachments", part.mime_type()).into()),
    })).collect()
}

#[async_trait]
impl LlmProvider for OpenAIProvider {
    fn build(&self, messages: &[Message], tools: &[Arc<dyn Tool>]) -> Result<Value, Failure> {
        let mut input = Vec::new();
        let mut instructions = Vec::new();
        for message in messages {
            match message {
                Message::Compaction { summary, recent } => {
                    let checkpoint = gewu_core::compaction::build_checkpoint_message(summary, recent);
                    if let Message::User { content: parts } = checkpoint {
                        input.push(json!({"role":"user", "content":content(&parts)?}));
                    }
                }
                Message::System { content } => instructions.push(content.as_str()),
                Message::User { content: parts } => input.push(json!({"role":"user", "content":content(parts)?})),
                Message::Assistant { content, reasoning, tool_calls, replay } => {
                    if let Some(replay) = replay.as_ref().filter(|r| r.origin == self.origin() && r.model == self.model) {
                        for item in &replay.blocks {
                            if item["type"] == "reasoning" {
                                if item["encrypted_content"].as_str().is_some_and(|text| !text.is_empty()) {
                                    input.push(item.clone());
                                }
                            } else { input.push(item.clone()); }
                        }
                        continue;
                    }
                    let text = [reasoning.as_deref(), content.as_deref()].into_iter().flatten()
                        .filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n");
                    if !text.is_empty() {
                        input.push(json!({"role":"assistant", "content":[{"type":"output_text", "text":text, "annotations":[]}]}));
                    }
                    for call in tool_calls.iter().flatten() {
                        input.push(json!({"type":"function_call", "call_id":call.id, "name":call.function.name,
                            "arguments":if call.function.arguments.trim().is_empty() { "{}" } else { &call.function.arguments }}));
                    }
                }
                Message::Tool { tool_call_id, content: text, media, .. } => {
                    let output = if media.is_empty() { Value::String(text.clone()) } else {
                        let mut parts = vec![json!({"type":"input_text", "text":text})];
                        parts.extend(content(media)?);
                        Value::Array(parts)
                    };
                    input.push(json!({"type":"function_call_output", "call_id":tool_call_id, "output":output}));
                }
            }
        }
        let functions = tools.iter().map(|tool| json!({"type":"function", "name":tool.name(),
            "description":tool.description(), "parameters":tool.schema(), "strict":false})).collect::<Vec<_>>();
        let mut request = json!({"model":self.model, "input":input, "stream":true, "store":false,
            "include":["reasoning.encrypted_content"]});
        if !instructions.is_empty() { request["instructions"] = instructions.join("\n\n").into(); }
        if !functions.is_empty() { request["tools"] = functions.into(); }
        if let Some(limit) = self.max_output_tokens { request["max_output_tokens"] = limit.into(); }
        let mut reasoning = serde_json::Map::new();
        if self.reasoning_summary { reasoning.insert("summary".into(), json!("auto")); }
        if let Some(effort) = &self.effort { reasoning.insert("effort".into(), json!(effort)); }
        if !reasoning.is_empty() { request["reasoning"] = Value::Object(reasoning); }
        Ok(request)
    }

    async fn send<'a>(&'a self, request: Value) -> Result<BoxStream<'a, Result<ProviderEvent, Failure>>, Failure> {
        let status = self.status.lock().expect("status sink lock poisoned").clone();
        let response = crate::http::with_retries_capped(self.backoff, self.max_wait,
            || self.request("/responses", Some(&request)),
            |error| match error {
                crate::openai::OpenAIError::Http(_) => crate::http::RetryDecision::Yes { after: None },
                crate::openai::OpenAIError::RateLimit { retry_after, .. } => crate::http::RetryDecision::Yes { after: *retry_after },
                _ => crate::http::RetryDecision::No,
            }, status.as_ref()).await?;
        Ok(parse_stream(response.bytes_stream(), self.origin(), self.model.clone()))
    }

    fn set_status_sink(&self, sink: Option<StatusSink>) { *self.status.lock().expect("status sink lock poisoned") = sink; }

    async fn list_models(&self) -> Result<Vec<String>, Failure> {
        let response: Value = self.request("/models", None).await?.json().await?;
        Ok(response["data"].as_array().ok_or("missing models data")?.iter()
            .filter_map(|model| model["id"].as_str().map(str::to_string)).collect())
    }
}

/// Decode complete SSE frames as bytes, so arbitrary UTF-8 and network
/// boundaries cannot corrupt streamed content. Multi-line data is supported.
fn parse_stream<S>(source: S, origin: String, model: String) -> BoxStream<'static, Result<ProviderEvent, Failure>>
where S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static {
    Box::pin(async_stream::try_stream! {
        let mut source = Box::pin(source);
        let mut pending = Vec::new();
        let mut data = Vec::<String>::new();
        let mut state = ResponseState::default();
        while let Some(chunk) = source.next().await {
            pending.extend_from_slice(&chunk?);
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !data.is_empty() {
                        let event = data.join("\n"); data.clear();
                        for event in state.process(&event, &origin, &model)? { yield event; }
                    }
                } else if let Some(text) = line.strip_prefix("data:") {
                    data.push(text.strip_prefix(' ').unwrap_or(text).to_string());
                }
            }
        }
        if !pending.is_empty() {
            let line = std::str::from_utf8(&pending)?.trim_end_matches('\r');
            if let Some(text) = line.strip_prefix("data:") { data.push(text.trim_start_matches(' ').into()); }
        }
        if !data.is_empty() { for event in state.process(&data.join("\n"), &origin, &model)? { yield event; } }
        if !state.finished { Err::<(), Failure>("Responses API stream ended without a terminal response".into())?; }
    })
}

#[derive(Default)]
struct ResponseState {
    finished: bool,
    calls: std::collections::BTreeMap<String, (String, String)>,
    text_items: BTreeSet<String>,
    reasoning_items: BTreeSet<String>,
}

impl ResponseState {
    fn process(&mut self, data: &str, origin: &str, model: &str) -> Result<Vec<ProviderEvent>, Failure> {
        if self.finished { return Ok(vec![]); }
        if data == "[DONE]" { return Ok(vec![]); }
        let event: Value = serde_json::from_str(data)?;
        let mut events = Vec::new();
        match event["type"].as_str().unwrap_or("") {
            "response.output_item.added" => {
                let item = &event["item"];
                if item["type"] == "function_call" {
                    let id = item["id"].as_str().ok_or("function call missing item ID")?.to_string();
                    let call = item["call_id"].as_str().ok_or("function call missing call_id")?.to_string();
                    events.push(ProviderEvent::ToolCallStart { id: call.clone(), name: item["name"].as_str().ok_or("function call missing name")?.into() });
                    let arguments = item["arguments"].as_str().unwrap_or("").to_string();
                    if !arguments.is_empty() { events.push(ProviderEvent::ToolCallDelta { id: call.clone(), delta: arguments.clone() }); }
                    self.calls.insert(id, (call, arguments));
                }
            }
            "response.function_call_arguments.delta" => {
                let id = event["item_id"].as_str().ok_or("argument delta missing item ID")?;
                let (call, arguments) = self.calls.get_mut(id).ok_or("argument delta for unknown function call")?;
                let delta = event["delta"].as_str().ok_or("missing argument delta")?;
                arguments.push_str(delta);
                events.push(ProviderEvent::ToolCallDelta { id: call.clone(), delta: delta.into() });
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text_items.insert(event["item_id"].as_str().unwrap_or("").into());
                events.push(ProviderEvent::ContentDelta { delta: event["delta"].as_str().ok_or("missing text delta")?.into() });
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_items.insert(event["item_id"].as_str().unwrap_or("").into());
                events.push(ProviderEvent::ReasoningDelta { delta: event["delta"].as_str().ok_or("missing reasoning delta")?.into() });
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                let output = response["output"].as_array().ok_or("terminal response missing output")?;
                for item in output {
                    let id = item["id"].as_str().unwrap_or("");
                    match item["type"].as_str().unwrap_or("") {
                        "function_call" => {
                            let call = item["call_id"].as_str().ok_or("function call missing call_id")?;
                            let arguments = item["arguments"].as_str().unwrap_or("");
                            if let Some((_, buffered)) = self.calls.get(id) {
                                let suffix = arguments.strip_prefix(buffered.as_str()).ok_or("function arguments changed after streaming")?;
                                if !suffix.is_empty() { events.push(ProviderEvent::ToolCallDelta { id: call.into(), delta: suffix.into() }); }
                            } else {
                                events.push(ProviderEvent::ToolCallStart { id: call.into(), name: item["name"].as_str().ok_or("function call missing name")?.into() });
                                events.push(ProviderEvent::ToolCallDelta { id: call.into(), delta: arguments.into() });
                            }
                        }
                        "message" if !self.text_items.contains(id) => {
                            for part in item["content"].as_array().ok_or("message missing content")? {
                                if let Some(text) = part["text"].as_str().or_else(|| part["refusal"].as_str()) {
                                    events.push(ProviderEvent::ContentDelta { delta: text.into() });
                                }
                            }
                        }
                        "reasoning" if !self.reasoning_items.contains(id) => {
                            for part in item["summary"].as_array().into_iter().flatten() {
                                if let Some(text) = part["text"].as_str() { events.push(ProviderEvent::ReasoningDelta { delta: text.into() }); }
                            }
                        }
                        _ => {}
                    }
                }
                events.push(ProviderEvent::ReasoningReplay { replay: ReasoningReplay {
                    origin: origin.into(), model: model.into(), field: None, blocks: output.clone(),
                }});
                if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
                    let total = usage["input_tokens"].as_u64().unwrap_or(0);
                    let cached = usage["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                    events.push(ProviderEvent::Usage { usage: TokenUsage {
                        input_tokens: total.saturating_sub(cached), cached_tokens: cached, cache_write_tokens: 0,
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                    }});
                }
                let reason = if event["type"] == "response.incomplete" {
                    match response["incomplete_details"]["reason"].as_str() {
                        Some("max_output_tokens") => "max_tokens",
                        Some(reason) => return Err(format!("Responses API incomplete: {reason}").into()),
                        None => return Err("Responses API incomplete without a reason".into()),
                    }
                } else { "stop" };
                events.push(ProviderEvent::Finish { reason: Some(reason.into()) });
                self.finished = true;
            }
            "error" | "response.failed" => return Err(format!("Responses API error: {}", event.get("error").unwrap_or(&event["response"]["error"])).into()),
            _ => {}
        }
        Ok(events)
    }
}
