//! UI-agnostic conversation state shared by every GeWu front-end.
//!
//! [`SessionModel`] owns the transcript ([`Block`] list) and the idle/busy
//! phase. The terminal UI feeds it live [`AgentEvent`]s from an ask stream;
//! a graphical client feeds it the same events plus historical [`Message`]s
//! loaded from storage. Keeping this logic here means both front-ends render
//! and replay conversations identically, and it stays unit-testable without
//! any I/O.

use gewu_core::agent::extract_media_parts;
use gewu_core::question::Question;
use gewu_protocol::{AgentEvent, ContentPart, Message, summarize_tool_input};

/// A structured question attached to an open `question` tool block.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionState {
    /// Waiting for the user's answer.
    Pending {
        request_id: String,
        questions: Vec<Question>,
    },
    /// The user answered; `answers` holds one label list per question.
    Answered {
        request_id: String,
        questions: Vec<Question>,
        answers: Vec<Vec<String>>,
    },
    /// The user dismissed the form.
    Dismissed {
        request_id: String,
        questions: Vec<Question>,
    },
}

impl QuestionState {
    pub fn request_id(&self) -> &str {
        match self {
            QuestionState::Pending { request_id, .. }
            | QuestionState::Answered { request_id, .. }
            | QuestionState::Dismissed { request_id, .. } => request_id,
        }
    }

    pub fn questions(&self) -> &[Question] {
        match self {
            QuestionState::Pending { questions, .. }
            | QuestionState::Answered { questions, .. }
            | QuestionState::Dismissed { questions, .. } => questions,
        }
    }
}

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
        /// Images the tool returned (`read_file` media). Kept beside the
        /// output so base64 never renders as text; front-ends that cannot
        /// draw them simply show the output's caption.
        images: Vec<ContentPart>,
        /// Live status from the running tool (`ToolProgress` events);
        /// cleared when the tool finishes. Not part of stored history.
        status: Option<String>,
        /// Pending or settled form for the `question` tool. Live-only: it
        /// is rebuilt from the turn's event frames on reconnect.
        question: Option<QuestionState>,
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
        // any other event supersedes it. Status/Retrying do not end an open
        // thought — they can arrive mid-setup, before reasoning starts.
        if !matches!(
            event,
            AgentEvent::Status { .. } | AgentEvent::Retrying { .. }
        ) {
            self.status = None;
        }
        match event {
            AgentEvent::Compacted { summary } => {
                self.end_thought();
                self.blocks.push(Block::Compaction {
                    summary: summary.clone(),
                });
                Some(self.blocks.len() - 1)
            }
            AgentEvent::Status { message } => {
                self.status = Some(message.clone());
                None
            }
            AgentEvent::Retrying {
                message,
                attempt,
                delay_secs,
                ..
            } => {
                // The busy indicator carries the retry display; frontends
                // that surface retries elsewhere ignore the transient text.
                self.status = Some(AgentEvent::retry_message(message, *attempt, *delay_secs));
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
                    images: Vec::new(),
                    status: None,
                    question: None,
                });
                Some(self.blocks.len() - 1)
            }
            AgentEvent::ToolProgress { tool, message } => {
                self.set_tool_status(Some(tool), Some(message.clone()))
            }
            AgentEvent::QuestionAsked {
                request_id,
                questions,
            } => self.attach_question(request_id, questions.clone()),
            AgentEvent::QuestionResolved {
                request_id,
                answers,
            } => self.resolve_question(request_id, answers.clone()),
            AgentEvent::ToolFinish { tool, output, .. } => {
                self.set_tool_status(Some(tool), None);
                let (text, images) = tool_media(output);
                let images = match self.fill_open_tool(Some(tool), &text, images) {
                    Ok(index) => return Some(index),
                    Err(images) => images,
                };
                self.blocks.push(Block::Tool {
                    tool: tool.clone(),
                    input: String::new(),
                    output: Some(text),
                    images,
                    status: None,
                    question: None,
                });
                Some(self.blocks.len() - 1)
            }
            AgentEvent::Finish { response: _ } => {
                self.end_thought();
                None
            }
            AgentEvent::Cancelled { .. } => {
                self.end_thought();
                None
            }
            AgentEvent::Usage { .. } => {
                // Terminal-chunk accounting: also a reasoning boundary (a
                // turn that ends with thinking but no visible content).
                self.end_thought();
                None
            }
            AgentEvent::Error { error } => {
                self.end_thought();
                self.blocks.push(Block::Error(error.clone()));
                Some(self.blocks.len() - 1)
            }
            AgentEvent::Steered { text } => {
                // A user message injected mid-run renders like any other
                // user turn, at the point of the transcript where it landed.
                self.end_thought();
                self.blocks.push(Block::User(text.clone()));
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
    /// last [`SessionModel::apply_event`], if any. Reset per event.
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
            Message::Compaction { summary, .. } => self.blocks.push(Block::Compaction {
                summary: summary.clone(),
            }),
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
                        images: Vec::new(),
                        status: None,
                        question: None,
                    });
                }
            }
            Message::Tool {
                tool_name,
                content,
                media,
                ..
            } => {
                // Media may be stored beside the text (`media`) or, for
                // sessions recorded before images were split out, still
                // embedded in `content` as a payload.
                let (output, images) = tool_media(content);
                let images = if images.is_empty() {
                    image_parts(media)
                } else {
                    images
                };
                if let Err(images) = self.fill_open_tool(tool_name.as_deref(), &output, images) {
                    self.blocks.push(Block::Tool {
                        tool: tool_name.clone().unwrap_or_else(|| "tool".to_string()),
                        input: String::new(),
                        output: Some(output),
                        images,
                        status: None,
                        question: None,
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

    /// Fill the first still-open tool block matching the name (FIFO, with
    /// an any-name fallback) so tool results line up with their calls.
    ///
    /// The search spans the whole transcript, not just the trailing run of
    /// tool blocks: a steered user message can be inserted while a tool is
    /// still running, splitting the run — the finish must still land in the
    /// original block instead of duplicating it below the message. Safe
    /// because completed tools are skipped and results arrive in start
    /// order. Images are handed back to the caller when no block is open,
    /// so the result can be pushed without cloning the payload.
    fn fill_open_tool(
        &mut self,
        tool_name: Option<&str>,
        output: &str,
        images: Vec<ContentPart>,
    ) -> Result<usize, Vec<ContentPart>> {
        let mut pending = Some(images);
        let mut fallback = None;
        for i in 0..self.blocks.len() {
            let Block::Tool {
                tool,
                output: out,
                images: imgs,
                ..
            } = &mut self.blocks[i]
            else {
                continue;
            };
            if out.is_some() {
                continue;
            }
            if fallback.is_none() {
                fallback = Some(i);
            }
            if tool_name.is_none_or(|name| name == tool) {
                *out = Some(tool_output(output));
                *imgs = pending.take().unwrap_or_default();
                return Ok(i);
            }
        }
        match fallback {
            Some(i) => {
                if let Block::Tool {
                    output: out,
                    images: imgs,
                    ..
                } = &mut self.blocks[i]
                {
                    *out = Some(tool_output(output));
                    *imgs = pending.take().unwrap_or_default();
                    Ok(i)
                } else {
                    Err(pending.unwrap_or_default())
                }
            }
            None => Err(pending.unwrap_or_default()),
        }
    }

    /// Set the live status on the first still-open tool block matching the
    /// name (FIFO, same rule as [`SessionModel::fill_open_tool`]). Used for
    /// `ToolProgress` events; `None` clears the status (a finished tool's
    /// output takes over). Returns the affected block's index.
    fn set_tool_status(
        &mut self,
        tool_name: Option<&str>,
        status: Option<String>,
    ) -> Option<usize> {
        for i in 0..self.blocks.len() {
            if let Block::Tool {
                tool,
                output: out,
                status: current,
                ..
            } = &mut self.blocks[i]
            {
                if out.is_some() || *current == status {
                    continue;
                }
                if tool_name.is_none_or(|name| name == tool) {
                    *current = status;
                    return Some(i);
                }
            }
        }
        None
    }

    /// Attach a newly asked question to its open `question` tool block.
    ///
    /// The tool-input summary for `question` is the first question's header,
    /// so a card is matched by that label first; its fallback is the first
    /// open question card. Exact matching keeps parallel question calls
    /// aligned with their form even when the ask events arrive out of call
    /// order. Each block carries one request id, so resolution is exact.
    fn attach_question(&mut self, request_id: &str, questions: Vec<Question>) -> Option<usize> {
        let label = questions
            .first()
            .map(|question| question.header.trim())
            .filter(|header| !header.is_empty());
        let mut fallback = None;
        let mut matched = None;
        for (index, block) in self.blocks.iter().enumerate() {
            let Block::Tool {
                tool,
                input,
                output: None,
                question: None,
                ..
            } = block
            else {
                continue;
            };
            if tool != "question" {
                continue;
            }
            fallback.get_or_insert(index);
            if label.is_none_or(|label| input.as_str() == label) {
                matched = Some(index);
                break;
            }
        }
        let index = matched.or(fallback)?;
        let Block::Tool { question: slot, .. } = &mut self.blocks[index] else {
            return None;
        };
        *slot = Some(QuestionState::Pending {
            request_id: request_id.to_string(),
            questions,
        });
        Some(index)
    }

    /// Replace the pending state of the block owning `request_id`.
    fn resolve_question(
        &mut self,
        request_id: &str,
        answers: Option<Vec<Vec<String>>>,
    ) -> Option<usize> {
        for i in 0..self.blocks.len() {
            let Block::Tool {
                question: Some(state),
                ..
            } = &mut self.blocks[i]
            else {
                continue;
            };
            if state.request_id() != request_id {
                continue;
            }
            let questions = state.questions().to_vec();
            *state = match answers.clone() {
                Some(answers) => QuestionState::Answered {
                    request_id: request_id.to_string(),
                    questions,
                    answers,
                },
                None => QuestionState::Dismissed {
                    request_id: request_id.to_string(),
                    questions,
                },
            };
            return Some(i);
        }
        None
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
        // A question still pending when the turn ends can never be answered:
        // the request is unregistered server-side (interrupt) or lost with
        // the turn. Settle the card instead of leaving a dead form. A
        // `question` block announced without a form (interrupted before the
        // ask event arrived) is closed too, so a later turn cannot attach its
        // form to the stale card.
        for block in &mut self.blocks {
            let Block::Tool {
                tool,
                output,
                question,
                ..
            } = block
            else {
                continue;
            };
            if tool != "question" {
                continue;
            }
            match question {
                Some(QuestionState::Pending {
                    request_id,
                    questions,
                }) => {
                    *question = Some(QuestionState::Dismissed {
                        request_id: std::mem::take(request_id),
                        questions: std::mem::take(questions),
                    });
                }
                None if output.is_none() => {
                    *output = Some("Question not asked (the turn ended).".to_string());
                }
                _ => {}
            }
        }
    }

    /// Whether the running turn is blocked on a user-facing form.
    ///
    /// Live-only: question forms are never replayed from stored history, and
    /// [`finish_ask`](Self::finish_ask) settles a still-pending form when the
    /// turn ends, so an idle or resumed session never reports waiting.
    pub fn has_pending_question(&self) -> bool {
        self.blocks.iter().any(|block| {
            matches!(
                block,
                Block::Tool {
                    tool,
                    question: Some(QuestionState::Pending { .. }),
                    ..
                } if tool == "question"
            )
        })
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

/// Tool output as received, keeping line breaks so front-ends can collapse and
/// expand it. The server already bounds results (2000 lines / 50 KB).
fn tool_output(text: &str) -> String {
    text.to_string()
}

/// Split a tool result into display text and the image parts it carried.
///
/// A media payload (`read_file` on an image) is one JSON string whose base64
/// must never reach the transcript as text, so the payload's message becomes
/// the block's output and its images travel separately.
fn tool_media(output: &str) -> (String, Vec<ContentPart>) {
    let parts = extract_media_parts(output);
    if parts.is_empty() {
        return (tool_output(output), Vec::new());
    }
    let text = parts
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    (tool_output(text), image_parts(&parts))
}

/// The image parts of a message payload.
fn image_parts(parts: &[ContentPart]) -> Vec<ContentPart> {
    parts
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn compaction_live_and_reloaded_summaries_match_without_recent_context() {
        let mut live = SessionModel::default();
        live.apply_event(&AgentEvent::Status {
            message: "Compacting context…".into(),
        });
        assert!(live.status.is_some());
        assert_eq!(
            live.apply_event(&AgentEvent::Compacted {
                summary: "summary".into()
            }),
            Some(0)
        );
        assert!(live.status.is_none());
        let mut loaded = SessionModel::default();
        loaded.apply_message(&Message::Compaction {
            summary: "summary".into(),
            recent: "serialized context".into(),
        });
        assert_eq!(live.blocks, loaded.blocks);
        assert_eq!(
            live.blocks,
            vec![Block::Compaction {
                summary: "summary".into()
            }]
        );
        assert_eq!(live.blocks[0].text(), "summary");
    }

    #[test]
    fn failed_compaction_clears_status_without_success_marker() {
        let mut model = SessionModel::default();
        model.apply_event(&AgentEvent::Status {
            message: "Compacting context…".into(),
        });
        model.apply_event(&AgentEvent::Error {
            error: "summary failed".into(),
        });
        assert!(model.status.is_none());
        assert!(
            !model
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Compaction { .. }))
        );
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
            call_id: String::new(),
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
            tool: "bash".into(),
            title: String::new(),
            output: "file.rs".into(),
            metadata: Default::default(),
        });
        assert_eq!(
            m.blocks,
            vec![Block::Tool {
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("file.rs".into()),
                images: Vec::new(),
                status: None,
            }]
        );
    }

    fn question() -> Question {
        Question {
            question: "Which database?".into(),
            header: "Database".into(),
            options: vec![gewu_core::question::QuestionOption {
                label: "SQLite".into(),
                description: "Embedded".into(),
            }],
            multiple: false,
            custom: true,
        }
    }

    #[test]
    fn question_events_attach_to_the_question_card_and_resolve() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "question".into(),
            input: r#"{"questions":[{"question":"Which database?","header":"Database"}]}"#.into(),
        });
        assert_eq!(
            m.apply_event(&AgentEvent::QuestionAsked {
                request_id: "que_1".into(),
                questions: vec![question()],
            }),
            Some(0)
        );
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: Some(QuestionState::Pending { request_id, .. }),
                ..
            } if request_id == "que_1"
        ));
        assert!(m.has_pending_question());

        assert_eq!(
            m.apply_event(&AgentEvent::QuestionResolved {
                request_id: "que_1".into(),
                answers: Some(vec![vec!["SQLite".into()]]),
            }),
            Some(0)
        );
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: Some(QuestionState::Answered { answers, .. }),
                ..
            } if answers == &vec![vec!["SQLite".to_string()]]
        ));
        assert!(!m.has_pending_question());

        // The tool result still lands in the same card.
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
            tool: "question".into(),
            title: "question".into(),
            output: "answered".into(),
            metadata: Default::default(),
        });
        assert_eq!(m.blocks.len(), 1);
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                output: Some(_),
                question: Some(QuestionState::Answered { .. }),
                ..
            }
        ));
    }

    #[test]
    fn parallel_questions_resolve_by_request_id() {
        let mut m = model();
        for _ in 0..2 {
            let _ = m.apply_event(&AgentEvent::ToolStart {
                call_id: String::new(),
                tool: "question".into(),
                input: "{}".into(),
            });
        }
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_a".into(),
            questions: vec![question()],
        });
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_b".into(),
            questions: vec![question()],
        });
        // Resolve the second question first: the first card stays pending.
        let _ = m.apply_event(&AgentEvent::QuestionResolved {
            request_id: "que_b".into(),
            answers: Some(vec![vec!["Postgres".into()]]),
        });
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: Some(QuestionState::Pending { request_id, .. }),
                ..
            } if request_id == "que_a"
        ));
        assert!(matches!(
            &m.blocks[1],
            Block::Tool {
                question: Some(QuestionState::Answered { request_id, .. }),
                ..
            } if request_id == "que_b"
        ));
        // The first question is still open, so the turn is still blocked.
        assert!(m.has_pending_question());
        let _ = m.apply_event(&AgentEvent::QuestionResolved {
            request_id: "que_a".into(),
            answers: None,
        });
        assert!(!m.has_pending_question());
    }

    #[test]
    fn finishing_an_ask_settles_a_still_pending_question() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "question".into(),
            input: "{}".into(),
        });
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_1".into(),
            questions: vec![question()],
        });
        assert!(m.has_pending_question());
        m.finish_ask();
        assert!(!m.has_pending_question());
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: Some(QuestionState::Dismissed { request_id, questions }),
                ..
            } if request_id == "que_1" && questions == &vec![question()]
        ));
    }

    #[test]
    fn out_of_order_ask_events_match_cards_by_header() {
        let mut m = model();
        for header in ["First", "Second"] {
            let _ = m.apply_event(&AgentEvent::ToolStart {
                call_id: String::new(),
                tool: "question".into(),
                input: format!(r#"{{"questions":[{{"question":"?","header":"{header}"}}]}}"#),
            });
        }
        let named = |header: &str| Question {
            header: header.into(),
            ..question()
        };
        // The second call's event arrives first.
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_b".into(),
            questions: vec![named("Second")],
        });
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_a".into(),
            questions: vec![named("First")],
        });
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: Some(QuestionState::Pending { request_id, .. }),
                ..
            } if request_id == "que_a"
        ));
        assert!(matches!(
            &m.blocks[1],
            Block::Tool {
                question: Some(QuestionState::Pending { request_id, .. }),
                ..
            } if request_id == "que_b"
        ));
    }

    #[test]
    fn finishing_an_ask_closes_a_question_announced_without_a_form() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "question".into(),
            input: r#"{"questions":[{"question":"?","header":"Stale"}]}"#.into(),
        });
        m.finish_ask();
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: None,
                output: Some(output),
                ..
            } if output.contains("turn ended")
        ));

        // A later turn's form attaches to its own card, not the stale one.
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "question".into(),
            input: r#"{"questions":[{"question":"?","header":"Fresh"}]}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_fresh".into(),
            questions: vec![Question {
                header: "Fresh".into(),
                ..question()
            }],
        });
        assert!(matches!(&m.blocks[0], Block::Tool { question: None, .. }));
        assert!(matches!(
            &m.blocks[1],
            Block::Tool {
                question: Some(QuestionState::Pending { request_id, .. }),
                ..
            } if request_id == "que_fresh"
        ));
    }

    #[test]
    fn dismissed_questions_keep_their_questions_for_the_summary() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "question".into(),
            input: "{}".into(),
        });
        let _ = m.apply_event(&AgentEvent::QuestionAsked {
            request_id: "que_1".into(),
            questions: vec![question()],
        });
        let _ = m.apply_event(&AgentEvent::QuestionResolved {
            request_id: "que_1".into(),
            answers: None,
        });
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                question: Some(QuestionState::Dismissed { questions, .. }),
                ..
            } if questions == &vec![question()]
        ));
    }

    #[test]
    fn tool_finish_fills_across_a_steered_user_block() {
        // The steer inserts a user block after the still-running tool, so
        // the tool being finished is no longer in the trailing run. Its
        // output must land in the original block, not in a duplicate.
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::Steered {
            text: "focus on tests".into(),
        });
        let affected = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
            tool: "bash".into(),
            title: String::new(),
            output: "file.rs".into(),
            metadata: Default::default(),
        });
        // The filled block is the original tool block, right below the
        // steered message; nothing else was appended.
        assert_eq!(affected, Some(0));
        assert_eq!(m.blocks.len(), 2);
        assert_eq!(
            m.blocks[0],
            Block::Tool {
                question: None,
                tool: "bash".into(),
                input: "ls".into(),
                output: Some("file.rs".into()),
                images: Vec::new(),
                status: None,
            }
        );
        assert!(matches!(m.blocks[1], Block::User(_)));
    }

    #[test]
    fn parallel_tool_finishes_fill_in_order_across_a_steer() {
        let mut m = model();
        for _ in 0..2 {
            let _ = m.apply_event(&AgentEvent::ToolStart {
                call_id: String::new(),
                tool: "bash".into(),
                input: "{}".into(),
            });
        }
        let _ = m.apply_event(&AgentEvent::Steered { text: "go".into() });
        assert_eq!(
            m.apply_event(&AgentEvent::ToolFinish {
                call_id: String::new(),
                tool: "bash".into(),
                title: String::new(),
                output: "first".into(),
                metadata: Default::default(),
            }),
            Some(0)
        );
        assert_eq!(
            m.apply_event(&AgentEvent::ToolFinish {
                call_id: String::new(),
                tool: "bash".into(),
                title: String::new(),
                output: "second".into(),
                metadata: Default::default(),
            }),
            Some(1)
        );
        // Exactly the two original tool blocks carry the outputs.
        let outputs: Vec<Option<String>> = m
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Tool { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].as_deref(), Some("first"));
        assert_eq!(outputs[1].as_deref(), Some("second"));
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
            call_id: String::new(),
            tool: "bash".into(),
            input: r#"{"command":"seq 60"}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
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
            call_id: String::new(),
            tool: "bash".into(),
            input: "{}".into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
            tool: "bash".into(),
            title: String::new(),
            output: "ok".into(),
            metadata: Default::default(),
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
            Block::Thought {
                started, reasoning, ..
            } => {
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
        let content_idx = m.apply_event(&AgentEvent::ContentDelta {
            delta: "answer".into(),
        });
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
            call_id: String::new(),
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
                call_id: String::new(),
                tool: "bash".into(),
                input: "{}".into(),
            }),
            Some(2)
        );
        assert_eq!(m.take_collapsed(), Some(1));
        // Taken once: the next event without a collapse reports nothing.
        m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
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

        m.apply_event(&AgentEvent::ContentDelta {
            delta: "answer".into(),
        });
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
        m.apply_event(&AgentEvent::ContentDelta {
            delta: "plain".into(),
        });
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
                call_id: String::new(),
                tool: "bash".into(),
                input: "{}".into(),
            }),
            Some(1)
        );
        assert_eq!(
            m.apply_event(&AgentEvent::ToolFinish {
                call_id: String::new(),
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
                call_id: String::new(),
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
        assert_eq!(summarize_tool_input("skill", r#"{"name":"demo"}"#), "demo");
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
                    question: None,
                    tool: "bash".into(),
                    input: "ls".into(),
                    output: Some("a.rs".into()),
                    images: Vec::new(),
                    status: None,
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
                    question: None,
                    tool: "read_file".into(),
                    input: "{}".into(),
                    output: Some("file".into()),
                    images: Vec::new(),
                    status: None,
                },
                Block::Tool {
                    question: None,
                    tool: "bash".into(),
                    input: "{}".into(),
                    output: Some("cmd".into()),
                    images: Vec::new(),
                    status: None,
                },
            ]
        );
    }

    const IMAGE_PAYLOAD: &str = r#"{"type":"media","mime":"image/png","data":"ZmFrZQ==","message":"Image read successfully"}"#;

    fn png_part() -> ContentPart {
        ContentPart::Image {
            data: "ZmFrZQ==".into(),
            mime_type: "image/png".into(),
        }
    }

    #[test]
    fn tool_finish_keeps_image_media_out_of_the_output_text() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "read_file".into(),
            input: r#"{"file_path":"shot.png"}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
            tool: "read_file".into(),
            title: "read_file".into(),
            output: IMAGE_PAYLOAD.into(),
            metadata: Default::default(),
        });

        assert_eq!(
            m.blocks,
            vec![Block::Tool {
                question: None,
                tool: "read_file".into(),
                input: "shot.png".into(),
                output: Some("Image read successfully".into()),
                images: vec![png_part()],
                status: None,
            }]
        );
        assert!(
            !m.blocks[0].text().contains("ZmFrZQ=="),
            "base64 must never reach the transcript text"
        );
    }

    #[test]
    fn replayed_tool_media_is_kept_for_rendering() {
        let mut m = model();
        m.apply_message(&Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: Some(vec![gewu_protocol::ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"shot.png"}"#.into(),
                },
            }]),
        });
        m.apply_message(&Message::Tool {
            tool_call_id: "c1".into(),
            tool_name: Some("read_file".into()),
            content: "Image read successfully".into(),
            media: vec![png_part()],
        });

        assert_eq!(
            m.blocks[0],
            Block::Tool {
                question: None,
                tool: "read_file".into(),
                input: "shot.png".into(),
                output: Some("Image read successfully".into()),
                images: vec![png_part()],
                status: None,
            }
        );
    }

    #[test]
    fn replayed_media_embedded_in_content_is_still_split() {
        let mut m = model();
        m.apply_message(&Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: Some(vec![gewu_protocol::ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"shot.png"}"#.into(),
                },
            }]),
        });
        // Sessions recorded before media was split out kept the payload here.
        m.apply_message(&Message::Tool {
            tool_call_id: "c1".into(),
            tool_name: Some("read_file".into()),
            content: IMAGE_PAYLOAD.into(),
            media: vec![],
        });

        let Block::Tool { output, images, .. } = &m.blocks[0] else {
            panic!("expected a tool block");
        };
        assert_eq!(output.as_deref(), Some("Image read successfully"));
        assert_eq!(images, &vec![png_part()]);
    }

    #[test]
    fn non_media_tool_output_is_unchanged() {
        let mut m = model();
        let _ = m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "bash".into(),
            input: r#"{"command":"ls"}"#.into(),
        });
        let _ = m.apply_event(&AgentEvent::ToolFinish {
            call_id: String::new(),
            tool: "bash".into(),
            title: String::new(),
            output: r#"{"type":"media-ish"}"#.into(),
            metadata: Default::default(),
        });
        let Block::Tool { output, images, .. } = &m.blocks[0] else {
            panic!("expected a tool block");
        };
        assert_eq!(output.as_deref(), Some(r#"{"type":"media-ish"}"#));
        assert!(images.is_empty());
    }

    #[test]
    fn tool_progress_updates_the_open_tool_block_in_place() {
        let mut m = model();
        m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "task".into(),
            input: r#"{"description":"explore","prompt":"p"}"#.into(),
        });
        assert_eq!(
            m.apply_event(&AgentEvent::ToolProgress {
                tool: "task".into(),
                message: "1s — bash: ls".into(),
            }),
            Some(0)
        );
        assert_eq!(
            m.blocks[0],
            Block::Tool {
                question: None,
                tool: "task".into(),
                input: "explore".into(),
                output: None,
                images: Vec::new(),
                status: Some("1s — bash: ls".into()),
            }
        );

        // A later update replaces the status; no new block appears.
        assert_eq!(
            m.apply_event(&AgentEvent::ToolProgress {
                tool: "task".into(),
                message: "2s — read src/main.rs".into(),
            }),
            Some(0)
        );
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(
            m.blocks[0],
            Block::Tool {
                question: None,
                tool: "task".into(),
                input: "explore".into(),
                output: None,
                images: Vec::new(),
                status: Some("2s — read src/main.rs".into()),
            }
        );
    }

    #[test]
    fn tool_finish_clears_the_status_and_progress_after_finish_is_ignored() {
        let mut m = model();
        m.apply_event(&AgentEvent::ToolStart {
            call_id: String::new(),
            tool: "task".into(),
            input: "{}".into(),
        });
        m.apply_event(&AgentEvent::ToolProgress {
            tool: "task".into(),
            message: "1s — bash: ls".into(),
        });
        assert_eq!(
            m.apply_event(&AgentEvent::ToolFinish {
                call_id: String::new(),
                tool: "task".into(),
                title: String::new(),
                output: "done".into(),
                metadata: Default::default(),
            }),
            Some(0)
        );
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                output: Some(_),
                status: None,
                ..
            }
        ));

        // A stale progress event after the finish has no effect.
        assert_eq!(
            m.apply_event(&AgentEvent::ToolProgress {
                tool: "task".into(),
                message: "late".into(),
            }),
            None
        );
    }

    #[test]
    fn tool_progress_matches_open_tools_fifo_by_name() {
        let mut m = model();
        for _ in 0..2 {
            m.apply_event(&AgentEvent::ToolStart {
                call_id: String::new(),
                tool: "task".into(),
                input: "{}".into(),
            });
        }
        // The first open task block gets the update.
        assert_eq!(
            m.apply_event(&AgentEvent::ToolProgress {
                tool: "task".into(),
                message: "bash: ls".into(),
            }),
            Some(0)
        );
        assert!(matches!(
            &m.blocks[0],
            Block::Tool {
                status: Some(_),
                ..
            }
        ));
        assert!(matches!(&m.blocks[1], Block::Tool { status: None, .. }));
    }
}

#[test]
fn workflow_tool_inputs_are_summarized_for_the_gui() {
    let spec = r#"{"name":"demo","nodes":[{"id":"a","agent":"read","prompt":"p"}]}"#;
    let start = summarize_tool_input("workflow_start", &format!(r#"{{"spec":{spec}}}"#));
    // Keys are re-ordered (BTreeMap) but the spec must survive verbatim.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&start).unwrap(),
        serde_json::from_str::<serde_json::Value>(spec).unwrap()
    );

    assert_eq!(
        summarize_tool_input("workflow_status", r#"{"run_id":"r-1"}"#),
        "r-1"
    );
    assert_eq!(
        summarize_tool_input("workflow_cancel", r#"{"run_id":"r-2"}"#),
        "r-2"
    );
}
