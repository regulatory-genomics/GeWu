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
    /// A collapsed thinking indicator, e.g. "Thought 5.6s". While reasoning
    /// streams (`started` set), the text is the live "Thinking… Ns" label.
    Thought {
        text: String,
        /// Reasoning (chain-of-thought) accumulated from `ReasoningDelta`
        /// events, or replayed from stored history.
        reasoning: String,
        /// When the reasoning segment started; `None` once it has ended.
        started: Option<std::time::Instant>,
    },
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
    /// Durable boundary between archived transcript and current model context.
    Compaction { summary: String },
}

impl Block {
    /// Speaker label rendered in the transcript.
    pub fn label(&self) -> &'static str {
        match self {
            Block::User(_) => "you",
            Block::Assistant(_) => "gewu",
            Block::Thought { .. } => "thought",
            Block::Tool { .. } => "tool",
            Block::Error(_) => "error",
            Block::Info(_) => "info",
            Block::Compaction { .. } => "compaction",
        }
    }

    /// The block's body text.
    pub fn text(&self) -> &str {
        match self {
            Block::User(text)
            | Block::Assistant(text)
            | Block::Error(text)
            | Block::Info(text)
            | Block::Thought { text, .. } => text,
            Block::Tool { output, .. } => output.as_deref().unwrap_or("running…"),
            Block::Compaction { summary } => summary,
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
    /// Transient status message (e.g. LLM retry backoff) shown in the busy
    /// indicator; cleared by the next non-status event.
    pub status: Option<String>,
    /// Block index of a thought collapsed during the last [`apply_event`];
    /// front-ends re-render it so the live "Thinking…" label settles even
    /// when the triggering event appends a different block.
    collapsed: Option<usize>,
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
            status: None,
            collapsed: None,
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
    ///
    /// Returns the index of the block the event affected, if any — callers
    /// can re-render just that block instead of the whole transcript.
    pub fn apply_event(&mut self, event: &AgentEvent) -> Option<usize> {
        self.collapsed = None;
        // A status message is transient UI state, not transcript content:
        // any other event supersedes it. Status does not end an open
        // thought — it can arrive mid-setup, before reasoning starts.
        if !matches!(event, AgentEvent::Status { .. }) {
            self.status = None;
        }
        match event {
            AgentEvent::Compacted { summary } => {
                self.end_thought();
                self.blocks.push(Block::Compaction { summary: summary.clone() });
                Some(self.blocks.len() - 1)
            }
            AgentEvent::Status { message } => {
                self.status = Some(message.clone());
                None
            }
            AgentEvent::ReasoningDelta { delta } => self.extend_thought(delta),
            AgentEvent::ContentDelta { delta } => {
                self.end_thought();
                let idx = self.open_assistant();
                if let Block::Assistant(text) = &mut self.blocks[idx] {
                    text.push_str(delta);
                }
                Some(idx)
            }
            AgentEvent::ToolStart { tool, input, .. } => {
                self.end_thought();
                self.blocks.push(Block::Tool {
                    tool: tool.clone(),
                    input: summarize_tool_input(tool, input),
                    output: None,
                });
                Some(self.blocks.len() - 1)
            }
            AgentEvent::ToolFinish { tool, output, .. } => {
                if !self.fill_open_tool(Some(tool), output) {
                    self.blocks.push(Block::Tool {
                        tool: tool.clone(),
                        input: String::new(),
                        output: Some(tool_output(output)),
                    });
                    return Some(self.blocks.len() - 1);
                }
                // `fill_open_tool` filled a block inside the trailing run; a
                // run renders as one card, so any index inside it identifies
                // the same card.
                let start = self
                    .blocks
                    .iter()
                    .rposition(|b| !matches!(b, Block::Tool { .. }))
                    .map_or(0, |i| i + 1);
                (start..self.blocks.len())
                    .find(|&i| {
                        matches!(
                            &self.blocks[i],
                            Block::Tool {
                                output: Some(_),
                                ..
                            }
                        )
                    })
                    .or(Some(self.blocks.len() - 1))
            }
            AgentEvent::Finish { response: _ } => {
                self.end_thought();
                None
            }
            AgentEvent::Usage { .. } => {
                // Terminal-chunk accounting: also a reasoning boundary (a
                // turn that ends with thinking but no visible content).
                self.end_thought();
                None
            }
            AgentEvent::Cancelled { reason } => {
                self.blocks.push(Block::Info(format!("cancelled: {reason}")));
                None
            }
            AgentEvent::Error { error } => {
                self.end_thought();
                self.blocks.push(Block::Error(error.clone()));
                Some(self.blocks.len() - 1)
            }
        }
    }

    /// Update the open reasoning indicator with the elapsed time, or start a
    /// new one. The delta accumulates into the block so the GUI can show the
    /// reasoning content on expansion. Returns the affected block index.
    fn extend_thought(&mut self, delta: &str) -> Option<usize> {
        if let Some(Block::Thought {
            text,
            reasoning,
            started,
        }) = self.blocks.last_mut()
            && let Some(started_at) = started.as_ref()
        {
            *text = format!("Thinking… {:.1}s", started_at.elapsed().as_secs_f32());
            reasoning.push_str(delta);
            return Some(self.blocks.len() - 1);
        }
        self.blocks.push(Block::Thought {
            text: "Thinking… 0.0s".to_string(),
            reasoning: delta.to_string(),
            started: Some(std::time::Instant::now()),
        });
        Some(self.blocks.len() - 1)
    }

    /// Collapse an open reasoning indicator to its final "Thought Ns" label.
    /// The collapsed block's index is reported via [`take_collapsed`].
    fn end_thought(&mut self) {
        if let Some(Block::Thought { text, started, .. }) = self.blocks.last_mut()
            && started.is_some()
        {
            let elapsed = started.unwrap().elapsed().as_secs_f32();
            *text = format!("Thought {elapsed:.1}s");
            *started = None;
            self.collapsed = Some(self.blocks.len() - 1);
        }
    }

    /// The block index whose live "Thinking…" label collapsed during the
    /// last [`apply_event`], if any. Reset per event.
    pub fn take_collapsed(&mut self) -> Option<usize> {
        self.collapsed.take()
    }

    /// Append a stored conversation [`Message`] (used when loading history).
    ///
    /// System messages are skipped: they are prompt configuration, not
    /// user-visible conversation.
    pub fn apply_message(&mut self, message: &Message) {
        match message {
            Message::System { .. } => {}
            Message::Compaction { summary, .. } => self.blocks.push(Block::Compaction { summary: summary.clone() }),
            Message::User { content } => {
                let text = parts_text(content);
                if !text.is_empty() {
                    self.blocks.push(Block::User(text));
                }
            }
            Message::Assistant {
                content,
                reasoning,
                replay: _,
                tool_calls,
            } => {
                // Reasoning replays as a finalized thought block (duration is
                // unknown for stored history, so the label has no time).
                if let Some(reasoning) = reasoning.as_deref().filter(|r| !r.is_empty()) {
                    self.blocks.push(Block::Thought {
                        text: "Thought".to_string(),
                        reasoning: reasoning.to_string(),
                        started: None,
                    });
                }
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
                        output: Some(tool_output(&output)),
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
                    *out = Some(tool_output(output));
                    return true;
                }
            }
        }
        if let Some(i) = fallback
            && let Block::Tool { output: out, .. } = &mut self.blocks[i]
        {
            *out = Some(tool_output(output));
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
            // Reminders are model-facing scaffolding appended to user
            // messages; they must never render as user content.
            ContentPart::Reminder { .. } => None,
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

/// Tool output as received, keeping line breaks so front-ends can collapse and
/// expand it. The server already bounds results (2000 lines / 50 KB).
fn tool_output(text: &str) -> String {
    text.to_string()
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
    #[test]
    fn compaction_live_and_reloaded_summaries_match_without_recent_context() {
        let mut live = SessionModel::default();
        live.apply_event(&AgentEvent::Status { message: "Compacting context…".into() });
        assert!(live.status.is_some());
        assert_eq!(live.apply_event(&AgentEvent::Compacted { summary: "summary".into() }), Some(0));
        assert!(live.status.is_none());
        let mut loaded = SessionModel::default();
        loaded.apply_message(&Message::Compaction { summary: "summary".into(), recent: "serialized context".into() });
        assert_eq!(live.blocks, loaded.blocks);
        assert_eq!(live.blocks, vec![Block::Compaction { summary: "summary".into() }]);
        assert_eq!(live.blocks[0].text(), "summary");
    }

    #[test]
    fn failed_compaction_clears_status_without_success_marker() {
        let mut model = SessionModel::default();
        model.apply_event(&AgentEvent::Status { message: "Compacting context…".into() });
        model.apply_event(&AgentEvent::Error { error: "summary failed".into() });
        assert!(model.status.is_none());
        assert!(!model.blocks.iter().any(|block| matches!(block, Block::Compaction { .. })));
    }
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

    /// Reminder parts appended to user messages are model-facing
    /// scaffolding; history replay must not render them as user content.
    #[test]
    fn reminders_in_history_are_not_rendered_as_user_content() {
        let mut m = model();
        m.apply_message(&Message::User {
            content: vec![
                ContentPart::Text {
                    text: "Update README.md".into(),
                },
                ContentPart::Reminder {
                    text: "<system-reminder>Plan mode is active.</system-reminder>".into(),
                },
            ],
        });
        assert_eq!(m.blocks, vec![Block::User("Update README.md".into())]);
    }

    #[test]
    fn content_deltas_accumulate_in_one_block() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ContentDelta {
            delta: "Hel".into(),
        });
        let _ = m.apply_event(&AgentEvent::ContentDelta { delta: "lo".into() });
        assert_eq!(m.blocks, vec![Block::Assistant("Hello".into())]);
    }

    #[test]
    fn tool_start_then_finish_fills_output() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
            call_id: "call-1".into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
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
    fn tool_output_keeps_line_breaks_and_full_length() {
        let mut m = model();
        let output = (1..=60)
            .map(|n| format!("line {n} with some padding text"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(output.len() > 400);
        let _ = m.apply_event(&AgentEvent::ToolStart {
            tool: "bash".into(),
            input: r#"{"command":"seq 60"}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            tool: "bash".into(),
            title: String::new(),
            output: output.clone(),
            metadata: Default::default(),
        });
        let Some(Block::Tool {
            output: Some(stored),
            ..
        }) = m.blocks.last()
        else {
            panic!("expected a tool block with output");
        };
        assert_eq!(stored, &output, "front-ends need the real lines to expand");
        assert!(stored.lines().count() > 6);
    }

    #[test]
    fn tool_start_after_content_starts_new_block() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ContentDelta {
            delta: "let me check".into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolStart {
            tool: "bash".into(),
            input: "{}".into(),
            call_id: "call-2".into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            tool: "bash".into(),
            title: String::new(),
            output: "ok".into(),
            metadata: Default::default(),
            call_id: "call-2".into(),
        });
        let _ = m.apply_event(&AgentEvent::ContentDelta {
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
    fn status_event_sets_a_transient_cleared_by_the_next_event() {
        let mut m = model();
        m.submit("hi".into());

        // The status is stored for the busy indicator, not the transcript.
        assert_eq!(
            m.apply_event(&AgentEvent::Status {
                message: "LLM request failed: rate limited; retrying in 4s".into(),
            }),
            None
        );
        assert_eq!(
            m.status.as_deref(),
            Some("LLM request failed: rate limited; retrying in 4s")
        );
        assert_eq!(m.blocks.len(), 1); // only the user prompt

        // Any other event supersedes it.
        m.apply_event(&AgentEvent::ContentDelta { delta: "hi".into() });
        assert_eq!(m.status, None);
    }

    #[test]
    fn reasoning_starts_a_live_thought_block() {
        let mut m = model();
        m.submit("hi".into());

        let first = m.apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });
        assert_eq!(first, Some(1));
        match &m.blocks[1] {
            Block::Thought { started, reasoning, .. } => {
                assert!(started.is_some());
                assert_eq!(reasoning, "hm");
            }
            other => panic!("expected Thought block, got {other:?}"),
        }

        // Later deltas update the same block instead of stacking new ones.
        assert_eq!(
            m.apply_event(&AgentEvent::ReasoningDelta { delta: "mm".into() }),
            first
        );
        assert_eq!(m.blocks.len(), 2);
        match &m.blocks[1] {
            Block::Thought { reasoning, .. } => assert_eq!(reasoning, "hmmm"),
            other => panic!("expected Thought block, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_ends_at_the_first_visible_output() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });

        // Content ends the thought and opens the assistant block.
        let content_idx = m.apply_event(&AgentEvent::ContentDelta { delta: "answer".into() });
        assert_eq!(content_idx, Some(2));
        match &m.blocks[1] {
            Block::Thought { text, started, .. } => {
                assert!(text.starts_with("Thought "), "{text}");
                assert!(started.is_none());
            }
            other => panic!("expected Thought block, got {other:?}"),
        }
        assert_eq!(m.blocks[2], Block::Assistant("answer".into()));
    }

    #[test]
    fn tool_call_also_ends_the_thought() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });

        m.apply_event(&AgentEvent::ToolStart {
            tool: "bash".into(),
            input: "{}".into(),
        });
        match &m.blocks[1] {
            Block::Thought { text, started, .. } => {
                assert!(text.starts_with("Thought "), "{text}");
                assert!(started.is_none());
            }
            other => panic!("expected Thought block, got {other:?}"),
        }
    }

    /// A collapse caused by an event that appends a different block (here a
    /// ToolStart) is still reported, so front-ends can re-render the settled
    /// label; the report is per event and clears once taken.
    #[test]
    fn tool_start_collapsing_a_thought_reports_the_collapsed_index() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });

        assert_eq!(m.take_collapsed(), None);
        assert_eq!(
            m.apply_event(&AgentEvent::ToolStart {
                tool: "bash".into(),
                input: "{}".into(),
            }),
            Some(2)
        );
        assert_eq!(m.take_collapsed(), Some(1));
        // Taken once: the next event without a collapse reports nothing.
        m.apply_event(&AgentEvent::ToolFinish {
            tool: "bash".into(),
            title: String::new(),
            output: "ok".into(),
            metadata: Default::default(),
        });
        assert_eq!(m.take_collapsed(), None);
    }

    /// Content after reasoning also collapses the thought (the trigger
    /// appends an assistant block, not the thought it settled).
    #[test]
    fn content_delta_collapsing_a_thought_reports_the_collapsed_index() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ReasoningDelta { delta: "hm".into() });

        m.apply_event(&AgentEvent::ContentDelta { delta: "answer".into() });
        assert_eq!(m.take_collapsed(), Some(1));
    }

    #[test]
    fn a_second_reasoning_segment_gets_its_own_block() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ReasoningDelta { delta: "a".into() });
        m.apply_event(&AgentEvent::ContentDelta { delta: "x".into() });

        let second = m.apply_event(&AgentEvent::ReasoningDelta { delta: "b".into() });
        assert_eq!(second, Some(3));
        assert_eq!(m.blocks.len(), 4);
        assert!(matches!(m.blocks[3], Block::Thought { .. }));
    }

    #[test]
    fn no_reasoning_produces_no_thought_block() {
        let mut m = model();
        m.submit("hi".into());
        m.apply_event(&AgentEvent::ContentDelta { delta: "plain".into() });
        assert_eq!(m.blocks.len(), 2);
        assert!(m.blocks.iter().all(|b| !matches!(b, Block::Thought { .. })));
    }

    #[test]
    fn apply_event_reports_the_affected_block() {
        let mut m = model();

        // First delta creates block 0; later deltas touch the same block.
        assert_eq!(
            m.apply_event(&AgentEvent::ContentDelta { delta: "a".into() }),
            Some(0)
        );
        assert_eq!(
            m.apply_event(&AgentEvent::ContentDelta { delta: "b".into() }),
            Some(0)
        );

        // ToolStart appends block 1; its ToolFinish fills it.
        assert_eq!(
            m.apply_event(&AgentEvent::ToolStart {
                tool: "bash".into(),
                input: "{}".into(),
            }),
            Some(1)
        );
        assert_eq!(
            m.apply_event(&AgentEvent::ToolFinish {
                tool: "bash".into(),
                title: String::new(),
                output: "ok".into(),
                metadata: Default::default(),
            }),
            Some(1)
        );

        // Non-transcript events report nothing.
        assert_eq!(
            m.apply_event(&AgentEvent::Usage {
                used_tokens: 10,
                context_limit: 100,
                provider: None,
            }),
            None
        );

        // A second ToolStart appends into the same run (block 2).
        assert_eq!(
            m.apply_event(&AgentEvent::ToolStart {
                tool: "grep".into(),
                input: "{}".into(),
            }),
            Some(2)
        );
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
                reasoning: None,
                replay: None,
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
    fn history_reasoning_replays_as_a_finalized_thought_block() {
        let mut m = model();
        m.apply_message(&Message::Assistant {
            content: Some("done".into()),
            reasoning: Some("step by step…".into()),
            replay: None,
            tool_calls: None,
        });
        m.apply_message(&Message::Assistant {
            content: Some("no thought".into()),
            reasoning: None,
            replay: None,
            tool_calls: None,
        });
        assert_eq!(
            m.blocks,
            vec![
                Block::Thought {
                    text: "Thought".into(),
                    reasoning: "step by step…".into(),
                    started: None,
                },
                Block::Assistant("done".into()),
                Block::Assistant("no thought".into()),
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
            reasoning: None,
            replay: None,
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
