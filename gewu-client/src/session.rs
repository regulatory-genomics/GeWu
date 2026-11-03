//! UI-agnostic conversation state shared by every GeWu front-end.
//!
//! [`SessionModel`] owns the transcript ([`Block`] list) and the idle/busy
//! phase. The terminal UI feeds it live [`AgentEvent`]s from an ask stream;
//! a graphical client feeds it the same events plus historical [`Message`]s
//! loaded from storage. Keeping this logic here means both front-ends render
//! and replay conversations identically, and it stays unit-testable without
//! any I/O.

use gewu_protocol::{AgentEvent, ContentPart, Message};

/// One entry in the conversation transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// A message sent by the user.
    User(String),
    /// Assistant content accumulated from `content_delta` events (or loaded
    /// from conversation history).
    Assistant(String),
    /// A tool invocation (start; finished output appended when known).
    Tool {
        tool: String,
        /// Rendered input summary (e.g. the bash command).
        input: String,
        output: Option<String>,
    },
    /// A non-fatal agent error or a connection error.
    Error(String),
    /// A status line (e.g. "interrupted", session hints).
    Info(String),
}

impl Block {
    /// Speaker label rendered in the transcript.
    pub fn label(&self) -> &'static str {
        match self {
            Block::User(_) => "you",
            Block::Assistant(_) => "gewu",
            Block::Tool { .. } => "tool",
            Block::Error(_) => "error",
            Block::Info(_) => "info",
        }
    }

    /// The block's body text.
    pub fn text(&self) -> &str {
        match self {
            Block::User(text) | Block::Assistant(text) | Block::Error(text) | Block::Info(text) => {
                text
            }
            Block::Tool { output, .. } => output.as_deref().unwrap_or("running…"),
        }
    }
}

/// Whether the model holds an open ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No ask in flight; the composer is editable.
    Idle,
    /// An ask is streaming.
    Busy,
}

/// Conversation state independent of how it is displayed.
#[derive(Debug)]
pub struct SessionModel {
    /// The transcript, oldest first.
    pub blocks: Vec<Block>,
    /// Whether an ask is currently streaming.
    pub phase: Phase,
    /// Server session handling this conversation, once known.
    pub session_id: Option<String>,
}

impl Default for SessionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionModel {
    /// An empty, idle conversation.
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            phase: Phase::Idle,
            session_id: None,
        }
    }

    // -- transcript ---------------------------------------------------------

    /// Start a new assistant block; returns its index.
    fn start_assistant(&mut self) -> usize {
        self.blocks.push(Block::Assistant(String::new()));
        self.blocks.len() - 1
    }

    /// Current (possibly newly created) assistant block being streamed into.
    fn open_assistant(&mut self) -> usize {
        match self.blocks.last() {
            Some(Block::Assistant(_)) => self.blocks.len() - 1,
            _ => self.start_assistant(),
        }
    }

    /// Apply one decoded [`AgentEvent`] to the transcript.
    pub fn apply_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ContentDelta { delta } => {
                let idx = self.open_assistant();
                if let Block::Assistant(text) = &mut self.blocks[idx] {
                    text.push_str(delta);
                }
            }
            AgentEvent::ReasoningDelta { .. } => {}
            AgentEvent::ToolStart { tool, input, .. } => {
                self.blocks.push(Block::Tool {
                    tool: tool.clone(),
                    input: summarize_tool_input(tool, input),
                    output: None,
                });
            }
            AgentEvent::ToolFinish { tool, output, .. } => {
                if !self.fill_open_tool(Some(tool), output) {
                    self.blocks.push(Block::Tool {
                        tool: tool.clone(),
                        input: String::new(),
                        output: Some(truncate(output, 400)),
                    });
                }
            }
            AgentEvent::Finish { response: _ } => {}
            AgentEvent::Cancelled { reason } => {
                self.blocks.push(Block::Info(format!("cancelled: {reason}")));
            }
            AgentEvent::Error { error } => {
                self.blocks.push(Block::Error(error.clone()));
            }
        }
    }

    /// Append a stored conversation [`Message`] (used when loading history).
    ///
    /// System messages are skipped: they are prompt configuration, not
    /// user-visible conversation.
    pub fn apply_message(&mut self, message: &Message) {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                let text = parts_text(content);
                if !text.is_empty() {
                    self.blocks.push(Block::User(text));
                }
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content.as_deref().filter(|t| !t.is_empty()) {
                    self.blocks.push(Block::Assistant(text.to_string()));
                }
                for call in tool_calls.iter().flatten() {
                    self.blocks.push(Block::Tool {
                        tool: call.function.name.clone(),
                        input: summarize_tool_input(&call.function.name, &call.function.arguments),
                        output: None,
                    });
                }
            }
            Message::Tool {
                tool_name,
                content,
                media,
                ..
            } => {
                let mut output = content.clone();
                let media_text = parts_text(media);
                if !media_text.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&media_text);
                }
                if !self.fill_open_tool(tool_name.as_deref(), &output) {
                    self.blocks.push(Block::Tool {
                        tool: tool_name.clone().unwrap_or_else(|| "tool".to_string()),
                        input: String::new(),
                        output: Some(truncate(&output, 400)),
                    });
                }
            }
        }
    }

    /// Replace the transcript with stored history, leaving `phase` alone.
    pub fn load_history<'m, I>(&mut self, messages: I)
    where
        I: IntoIterator<Item = &'m Message>,
    {
        self.blocks.clear();
        for message in messages {
            self.apply_message(message);
        }
    }

    /// Fill the first still-open tool block (within the trailing run of tool
    /// blocks) so historical tool results line up with their calls.
    fn fill_open_tool(&mut self, tool_name: Option<&str>, output: &str) -> bool {
        let start = self
            .blocks
            .iter()
            .rposition(|b| !matches!(b, Block::Tool { .. }))
            .map_or(0, |i| i + 1);
        let mut fallback = None;
        for i in start..self.blocks.len() {
            if let Block::Tool {
                tool, output: out, ..
            } = &mut self.blocks[i]
            {
                if out.is_some() {
                    continue;
                }
                if fallback.is_none() {
                    fallback = Some(i);
                }
                if tool_name.is_none_or(|name| name == tool) {
                    *out = Some(truncate(output, 400));
                    return true;
                }
            }
        }
        if let Some(i) = fallback
            && let Block::Tool { output: out, .. } = &mut self.blocks[i]
        {
            *out = Some(truncate(output, 400));
            return true;
        }
        false
    }

    // -- lifecycle ----------------------------------------------------------

    /// Record a user prompt and enter the busy phase.
    pub fn submit(&mut self, prompt: String) {
        self.blocks.push(Block::User(prompt));
        self.phase = Phase::Busy;
    }

    /// An ask finished (success or terminal error).
    pub fn finish_ask(&mut self) {
        self.phase = Phase::Idle;
    }

    /// Record the session id; `None` is ignored so a known id is sticky.
    pub fn set_session(&mut self, session_id: Option<String>) {
        if session_id.is_some() {
            self.session_id = session_id;
        }
    }

    /// Append an informational status line.
    pub fn note(&mut self, message: impl Into<String>) {
        self.blocks.push(Block::Info(message.into()));
    }

    /// Append an error line and return to idle.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.blocks.push(Block::Error(message.into()));
        self.phase = Phase::Idle;
    }

    /// Clear the transcript and session identity (e.g. "new chat").
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.phase = Phase::Idle;
        self.session_id = None;
    }
}

/// Concatenate the text parts of a message payload.
fn parts_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse tool input JSON to a single short line for display.
pub fn summarize_tool_input(tool: &str, input: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return truncate(input, 80),
    };
    let summary = match tool {
        "bash" => value
            .get("command")
            .and_then(|c| c.as_str())
            .map(str::to_string),
        "read_file" | "write_file" | "edit_file" | "apply_patch" => value
            .get("file_path")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        "glob" | "list" | "grep" => value
            .get("pattern")
            .and_then(|p| p.as_str())
            .map(str::to_string),
        "webfetch" => value
            .get("url")
            .and_then(|u| u.as_str())
            .map(str::to_string),
        _ => None,
    };
    summary.unwrap_or_else(|| truncate(&value.to_string(), 80))
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_protocol::FunctionCall;

    fn model() -> SessionModel {
        SessionModel::new()
    }

    #[test]
    fn submit_adds_user_block_and_sets_busy() {
        let mut m = model();
        m.submit("hi".into());
        assert_eq!(m.blocks, vec![Block::User("hi".into())]);
        assert_eq!(m.phase, Phase::Busy);
    }

    #[test]
    fn content_deltas_accumulate_in_one_block() {
        let mut m = model();
        m.apply_event(&AgentEvent::ContentDelta {
            delta: "Hel".into(),
        });
        m.apply_event(&AgentEvent::ContentDelta { delta: "lo".into() });
        assert_eq!(m.blocks, vec![Block::Assistant("Hello".into())]);
    }

    #[test]
    fn tool_start_then_finish_fills_output() {
        let mut m = model();
        m.apply_event(&AgentEvent::ToolStart {
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
            call_id: "call-1".into(),
        });
        m.apply_event(&AgentEvent::ToolFinish {
            tool: "bash".into(),
            title: String::new(),
            output: "file.rs".into(),
            metadata: Default::default(),
            call_id: "call-1".into(),
        });
        assert_eq!(
            m.blocks,
            vec![Block::Tool {
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("file.rs".into()),
            }]
        );
    }

    #[test]
    fn tool_start_after_content_starts_new_block() {
        let mut m = model();
        m.apply_event(&AgentEvent::ContentDelta {
            delta: "let me check".into(),
        });
        m.apply_event(&AgentEvent::ToolStart {
            tool: "bash".into(),
            input: "{}".into(),
            call_id: "call-2".into(),
        });
        m.apply_event(&AgentEvent::ToolFinish {
            tool: "bash".into(),
            title: String::new(),
            output: "ok".into(),
            metadata: Default::default(),
            call_id: "call-2".into(),
        });
        m.apply_event(&AgentEvent::ContentDelta {
            delta: "done".into(),
        });
        assert_eq!(m.blocks.len(), 3);
        assert_eq!(m.blocks[0], Block::Assistant("let me check".into()));
        assert_eq!(m.blocks[2], Block::Assistant("done".into()));
    }

    #[test]
    fn events_during_busy_then_finish() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ContentDelta {
            delta: "hello".into(),
        });
        m.finish_ask();
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn session_only_set_when_present() {
        let mut m = model();
        m.set_session(None);
        assert_eq!(m.session_id, None);
        m.set_session(Some("abc".into()));
        assert_eq!(m.session_id.as_deref(), Some("abc"));
        m.set_session(None);
        assert_eq!(m.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn tool_input_summaries() {
        assert_eq!(
            summarize_tool_input("read_file", r#"{"file_path":"/a/b.rs"}"#),
            "/a/b.rs"
        );
        assert_eq!(summarize_tool_input("bash", "not json"), "not json");
    }

    #[test]
    fn history_replays_into_blocks() {
        let mut m = model();
        let history = vec![
            Message::System {
                content: "ignored".into(),
            },
            Message::User {
                content: vec![ContentPart::Text {
                    text: "list files".into(),
                }],
            },
            Message::Assistant {
                content: Some("checking".into()),
                tool_calls: Some(vec![gewu_protocol::ToolCall {
                    id: "c1".into(),
                    r#type: "function".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: r#"{"command":"ls"}"#.into(),
                    },
                }]),
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                tool_name: Some("bash".into()),
                content: "a.rs".into(),
                media: vec![],
            },
        ];
        m.load_history(&history);
        assert_eq!(
            m.blocks,
            vec![
                Block::User("list files".into()),
                Block::Assistant("checking".into()),
                Block::Tool {
                    tool: "bash".into(),
                    input: "ls".into(),
                    output: Some("a.rs".into()),
                },
            ]
        );
    }

    #[test]
    fn history_matches_parallel_tool_calls_in_order() {
        let mut m = model();
        let call = |id: &str, name: &str| gewu_protocol::ToolCall {
            id: id.into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        };
        m.apply_message(&Message::Assistant {
            content: None,
            tool_calls: Some(vec![call("1", "read_file"), call("2", "bash")]),
        });
        m.apply_message(&Message::Tool {
            tool_call_id: "1".into(),
            tool_name: Some("read_file".into()),
            content: "file".into(),
            media: vec![],
        });
        m.apply_message(&Message::Tool {
            tool_call_id: "2".into(),
            tool_name: Some("bash".into()),
            content: "cmd".into(),
            media: vec![],
        });
        assert_eq!(
            m.blocks,
            vec![
                Block::Tool {
                    tool: "read_file".into(),
                    input: "{}".into(),
                    output: Some("file".into()),
                },
                Block::Tool {
                    tool: "bash".into(),
                    input: "{}".into(),
                    output: Some("cmd".into()),
                },
            ]
        );
    }
}
