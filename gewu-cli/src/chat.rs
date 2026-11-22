use crate::{
    endpoint::Endpoint,
    view::{View, editor},
};
use anyhow::{Context, Result, bail, ensure};
use gewu_client::{AskStream, Block, Client, Phase, QuestionState, SessionModel};
use gewu_protocol::{Question, ServerMessage, SessionSummary};
use ratatui_textarea::TextArea;

enum Panel {
    Menu(menu::Menu),
    Chat,
    Legacy(SessionSummary, String),
    Question {
        id: String,
        questions: Vec<Question>,
        answers: Vec<Vec<String>>,
    },
}

/// Only the observer renders frames. The control connection can send an ask,
/// steer or answer without drawing a second copy of an event.
struct Connections {
    control: Client,
    observer: Client,
    stream: AskStream,
}

pub(crate) struct Chat {
    pub model: SessionModel,
    pub turn_started: Option<std::time::Instant>,
    pub directory: String,
    pub model_name: String,
    catalog: Vec<gewu_protocol::ModelSummary>,
    effort: Option<String>,
    pub agent: String,
    pub editor: TextArea<'static>,
    pub view: View,
    pub designer: Option<crate::workflow::Designer>,
    pub key_entry: Option<crate::key_entry::KeyEntry>,
    pub notice: String,
    pub command_index: usize,
    pub commands_hidden: bool,
    pub context: Option<(u64, u64)>,
    attachments: Vec<gewu_protocol::AskAttachment>,
    provider_usage: Option<gewu_protocol::TokenUsage>,
    panel: Panel,
    draft: Option<TextArea<'static>>,
    command_draft: Option<TextArea<'static>>,
    connections: Option<Connections>,
    initial_agent: String,
    initial_prompt: Option<String>,
}

// The observer sends a fresh history snapshot at the start of every turn.
// Reattach locally measured footers only to replies still present after replay.
fn restore_turn_footers(previous: &[Block], history: &mut Vec<Block>) {
    let mut turn = 0;
    let mut footers = std::collections::BTreeMap::new();
    for pair in previous.windows(2) {
        match (&pair[0], &pair[1]) {
            (Block::User(_) | Block::UserMedia { .. }, _) => turn += 1,
            (Block::Assistant(reply), Block::Info(note)) if note.starts_with("Worked for ") => {
                footers.insert(turn, (reply, note));
            }
            _ => {}
        }
    }
    if footers.is_empty() {
        return;
    }
    turn = 0;
    let blocks = std::mem::take(history);
    for block in blocks {
        if matches!(block, Block::User(_) | Block::UserMedia { .. }) {
            turn += 1;
        }
        let footer = match &block {
            Block::Assistant(reply) if footers.get(&turn).is_some_and(|(old, _)| *old == reply) => {
                footers
                    .remove(&turn)
                    .map(|(_, note)| Block::Info(note.clone()))
            }
            _ => None,
        };
        history.push(block);
        history.extend(footer);
    }
}

impl Chat {
    pub async fn new(endpoint: &Endpoint, agent: &str, prompt: Option<String>) -> Result<Self> {
        let admin = endpoint.connect().await?;
        let info = admin.info().await?;
        let catalog = admin.list_models().await.unwrap_or_default();
        admin.close().await;
        let mut chat = Self::start(info.working_directory, info.model, agent, prompt);
        chat.catalog = catalog;
        Ok(chat)
    }

    fn start(directory: String, model_name: String, agent: &str, prompt: Option<String>) -> Self {
        Self {
            model: SessionModel::new(),
            turn_started: None,
            directory,
            model_name,
            catalog: Vec::new(),
            effort: None,
            agent: agent.to_owned(),
            initial_agent: agent.to_owned(),
            editor: editor(),
            view: View::default(),
            designer: None,
            key_entry: None,
            panel: Panel::Chat,
            draft: None,
            command_draft: None,
            connections: None,
            initial_prompt: prompt,
            notice: String::new(),
            command_index: 0,
            commands_hidden: false,
            context: None,
            attachments: Vec::new(),
            provider_usage: None,
        }
    }

    async fn sessions(endpoint: &Endpoint) -> Result<Vec<SessionSummary>> {
        let client = endpoint.connect().await?;
        let list = client.list_sessions().await;
        client.close().await;
        Ok(list?)
    }

    pub async fn pick(&mut self, endpoint: &Endpoint, id: Option<&str>, last: bool) -> Result<()> {
        let list = Self::sessions(endpoint).await?;
        let selected = if last {
            let admin = endpoint.connect().await?;
            let root = admin.info().await?.working_directory;
            admin.close().await;
            Some(
                list.iter()
                    .find(|s| {
                        s.workspace_path
                            .as_deref()
                            .is_some_and(|p| p.is_empty() || p == root)
                    })
                    .context("no saved session in this workspace")?,
            )
        } else if let Some(id) = id {
            Some(
                list.iter()
                    .find(|s| s.session_id == id)
                    .context("unknown session id")?,
            )
        } else {
            None
        };
        if let Some(session) = selected {
            self.select(endpoint, session.clone()).await?;
        } else {
            self.show_sessions(list);
        }
        Ok(())
    }

    async fn select(&mut self, endpoint: &Endpoint, session: SessionSummary) -> Result<()> {
        if session.workspace_path.is_none() {
            let client = endpoint.connect().await?;
            let root = client
                .info_for_session(&session.session_id)
                .await?
                .working_directory;
            client.close().await;
            if root.is_empty() {
                return self.open(endpoint, Some(&session), None).await;
            }
            self.panel = Panel::Legacy(session, root);
            return Ok(());
        }
        self.open(endpoint, Some(&session), None).await
    }

    pub async fn open(
        &mut self,
        endpoint: &Endpoint,
        session: Option<&SessionSummary>,
        legacy_path: Option<&str>,
    ) -> Result<()> {
        let control = endpoint.connect().await?;
        let (id, agent, directory) = if let Some(s) = session {
            let info = control.info_for_session(&s.session_id).await?;
            let mut directory = info.working_directory;
            if let Some(path) = legacy_path {
                if control.usage(None).await?.busy {
                    ensure!(
                        path == directory,
                        "a busy session is working in {directory}"
                    );
                } else {
                    directory = control.add_workspace(path).await?.1.path;
                }
            } else {
                ensure!(
                    s.workspace_path.as_deref().unwrap_or_default() == directory,
                    "session workspace changed; refresh the session list"
                );
            }
            (s.session_id.clone(), s.agent.clone(), directory)
        } else {
            let (id, root) = control.switch_workspace("").await?;
            if self.initial_agent != "build" {
                control.switch_agent(&self.initial_agent).await?;
            }
            (id, self.initial_agent.clone(), root.path)
        };
        let observer = endpoint.connect().await?;
        let mut stream = observer.watch_session(&id).await?;
        let snapshot = stream
            .next_frame()
            .await
            .context("server closed before session snapshot")??;
        ensure!(
            matches!(snapshot, ServerMessage::SessionSnapshot { .. }),
            "expected session snapshot"
        );
        self.disconnect().await;
        self.connections = Some(Connections {
            control,
            observer,
            stream,
        });
        self.directory = directory;
        self.attachments.clear();
        self.provider_usage = None;
        self.agent = agent;
        self.panel = Panel::Chat;
        self.draft = None;
        self.command_draft = None;
        self.editor = editor();
        self.apply(snapshot)?;
        self.refresh_context().await;
        if let Some(text) = self.initial_prompt.take() {
            if self.model.has_pending_question() {
                self.draft.get_or_insert_with(editor).insert_str(text);
            } else if endpoint.keys.missing(&self.model_name) {
                self.editor.insert_str(text);
                self.notice = "Set the API key for this model in /settings.".into();
            } else {
                self.send_prompt(text).await?;
            }
        }
        Ok(())
    }

    pub async fn disconnect(&mut self) {
        if let Some(c) = self.connections.take() {
            c.observer.close().await;
            c.control.close().await;
        }
    }

    pub async fn frame(&mut self) -> Option<gewu_client::Result<ServerMessage>> {
        match self.connections.as_mut() {
            Some(c) => c.stream.next_frame().await,
            None => std::future::pending().await,
        }
    }

    pub fn apply(&mut self, frame: ServerMessage) -> Result<()> {
        match frame {
            ServerMessage::SessionSnapshot {
                messages,
                frames,
                session_id,
                model,
                reasoning_effort,
                busy,
                ..
            } => {
                if !busy || self.model.session_id.as_ref() != Some(&session_id) {
                    self.turn_started = None;
                }
                let previous = std::mem::replace(&mut self.model, SessionModel::new());
                self.context = None;
                self.effort = reasoning_effort;
                self.model.set_session(Some(session_id));
                self.model.load_history(&messages);
                if previous.session_id == self.model.session_id {
                    restore_turn_footers(&previous.blocks, &mut self.model.blocks);
                }
                for f in frames {
                    if let Some(event) = AskStream::decode_event(&f) {
                        self.apply_event(event?);
                    }
                }
                self.model.phase = if busy { Phase::Busy } else { Phase::Idle };
                if let Some(name) = model {
                    self.model_name = name;
                }
                self.view = View::default();
            }
            ServerMessage::Result { response, .. } => {
                if !response.is_empty() {
                    match self.model.blocks.last_mut() {
                        Some(Block::Assistant(text)) => *text = response,
                        _ => self.model.blocks.push(Block::Assistant(response)),
                    }
                }
                self.model.finish_ask();
                if let Some(started) = self.turn_started.take() {
                    let beijing = chrono::Utc::now()
                        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
                    self.model.note(format!(
                        "Worked for {}s · {}",
                        started.elapsed().as_secs(),
                        beijing.format("%H:%M")
                    ));
                }
                self.notice.clear();
            }
            ServerMessage::Error { error, .. } => {
                self.model.finish_ask();
                self.turn_started = None;
                self.model.fail(error);
            }
            ServerMessage::WorkflowProgress { run, .. } => {
                self.notice = format!(
                    "Workflow {}: {} ({} done)",
                    run.name.unwrap_or(run.run_id),
                    run.status,
                    run.tasks_done
                );
            }
            f => {
                if let Some(event) = AskStream::decode_event(&f) {
                    self.apply_event(event?);
                }
            }
        }
        self.sync_question();
        Ok(())
    }

    fn apply_event(&mut self, event: gewu_protocol::AgentEvent) {
        if let gewu_protocol::AgentEvent::Usage {
            used_tokens,
            context_limit,
            provider,
        } = &event
            && *context_limit > 0
        {
            self.context = Some((*used_tokens, *context_limit));
            if provider.is_some() {
                self.provider_usage = *provider;
            }
        }
        self.model.apply_event(&event);
    }

    async fn refresh_context(&mut self) {
        if let Ok(client) = self.control()
            && let Ok(usage) = client.usage(self.model.session_id.as_deref()).await
            && !usage.busy
            && usage.context_limit > 0
        {
            self.context = Some((usage.used_tokens, usage.context_limit));
            self.provider_usage = usage.provider_usage;
        }
    }

    fn sync_question(&mut self) {
        let pending = self.model.blocks.iter().find_map(|b| match b {
            Block::Tool {
                question:
                    Some(QuestionState::Pending {
                        request_id,
                        questions,
                    }),
                ..
            } => Some((request_id, questions)),
            _ => None,
        });
        match pending {
            Some((id, questions)) if !matches!(&self.panel, Panel::Question { id: current, .. } if current == id) =>
            {
                if self.draft.is_none() {
                    self.draft = Some(std::mem::replace(&mut self.editor, editor()));
                }
                self.panel = Panel::Question {
                    id: id.clone(),
                    questions: questions.clone(),
                    answers: vec![],
                };
                self.view.scroll = 0;
            }
            None if matches!(self.panel, Panel::Question { .. }) => self.end_form(),
            _ => {}
        }
    }

    fn end_form(&mut self) {
        self.panel = Panel::Chat;
        self.editor = self.draft.take().unwrap_or_else(editor);
        if let Some(draft) = self.command_draft.take() {
            self.editor = draft;
        }
        self.view.scroll = 0;
    }

    pub(crate) fn control(&self) -> Result<&Client> {
        Ok(&self
            .connections
            .as_ref()
            .context("select a session or use /new")?
            .control)
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        self.control()?.interrupt().await?;
        self.notice = "Interrupted".into();
        Ok(())
    }

    async fn send_prompt(&mut self, text: String) -> Result<()> {
        if self.model.phase == Phase::Busy {
            ensure!(
                self.attachments.is_empty(),
                "Wait for this turn before sending attachments."
            );
            self.control()?.steer(text).await?;
            self.notice = "Additional instructions sent".into();
        } else {
            let started = std::time::Instant::now();
            self.control()?
                .ask(text)
                .attachments(self.attachments.clone())
                .send()
                .await?;
            self.turn_started = Some(started);
            self.attachments.clear();
            self.model.phase = Phase::Busy;
            self.notice.clear();
        }
        Ok(())
    }

    pub async fn escape(&mut self, endpoint: &Endpoint) -> Result<()> {
        if let Panel::Question { id, .. } = &self.panel {
            self.control()?.reject_question(id).await?;
        } else if matches!(&self.panel, Panel::Menu(menu) if menu.settings_child) {
            self.run_command(endpoint, crate::commands::Cmd::Settings, "")
                .await?;
        } else if !matches!(self.panel, Panel::Chat) {
            self.end_form();
            if self.connections.is_none() {
                self.open(endpoint, None, None).await?;
            }
        } else if let Some(draft) = self.command_draft.take() {
            self.editor = draft;
        }
        Ok(())
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.view.scroll = if matches!(self.panel, Panel::Chat) {
            self.view.scroll.saturating_add(n)
        } else {
            self.view.scroll.saturating_sub(n)
        };
    }
    pub fn scroll_down(&mut self, n: usize) {
        self.view.scroll = if matches!(self.panel, Panel::Chat) {
            self.view.scroll.saturating_sub(n)
        } else {
            self.view.scroll.saturating_add(n)
        };
    }
    pub fn mode_title(&self) -> &'static str {
        match self.panel {
            Panel::Chat => " Conversation ",
            Panel::Menu(_) => " Menu ",
            Panel::Legacy(..) => " Workspace ",
            Panel::Question { .. } => " Agent question ",
        }
    }

    pub fn panel_text(&self) -> Option<(String, Option<usize>)> {
        match &self.panel {
            Panel::Chat => None,
            Panel::Menu(menu) => Some(menu.display()),
            Panel::Legacy(s, path) => Some((
                format!(
                    "Session {} has no saved absolute workspace.\nEnter a server directory, or press Enter to use {path}.",
                    s.session_id
                ),
                None,
            )),
            Panel::Question {
                questions, answers, ..
            } => {
                let q = &questions[answers.len()];
                let mut lines = vec![format!(
                    "{}/{}  {}\n{}",
                    answers.len() + 1,
                    questions.len(),
                    q.header,
                    q.question
                )];
                for (i, o) in q.options.iter().enumerate() {
                    lines.push(format!("{}. {}  {}", i + 1, o.label, o.description));
                }
                if !q.options.is_empty() {
                    lines.push(
                        if q.multiple {
                            "Enter option numbers separated by commas."
                        } else {
                            "Enter one option number."
                        }
                        .into(),
                    );
                }
                if q.custom {
                    lines.push("You can type a custom answer (=text for literal numbers).".into());
                }
                lines.push("Esc dismisses the question.".into());
                Some((lines.join("\n"), None))
            }
        }
    }

    pub fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let [query] = self.editor.lines() else {
            return vec![];
        };
        if self.commands_hidden || !matches!(self.panel, Panel::Chat) {
            return vec![];
        }
        crate::commands::matches(query)
    }

    pub fn open_palette(&mut self) {
        if matches!(self.panel, Panel::Chat) && self.command_draft.is_none() {
            self.command_draft = Some(std::mem::replace(&mut self.editor, editor()));
            self.editor.insert_str("/");
            self.command_index = 0;
            self.commands_hidden = false;
        }
    }

    pub fn complete_command(&mut self, command: &str) {
        self.editor.select_all();
        self.editor.insert_str(command);
        self.commands_hidden = true;
        self.command_index = 0;
    }

    pub fn workspace_label(&self) -> &str {
        if self.directory.is_empty() {
            "No workspace"
        } else {
            &self.directory
        }
    }

    pub fn model_badge(&self) -> String {
        let entry = self
            .catalog
            .iter()
            .find(|entry| entry.selector == self.model_name);
        let label = entry
            .map(|entry| entry.label.as_str())
            .unwrap_or(&self.model_name);
        let effort = self
            .effort
            .as_deref()
            .or_else(|| entry.and_then(|entry| entry.default_effort.as_deref()));
        match effort {
            Some(value) if value != gewu_protocol::PROVIDER_DEFAULT_EFFORT => {
                format!("{label} {value}")
            }
            _ => label.to_owned(),
        }
    }

    pub fn status(&self) -> &str {
        if !self.notice.is_empty() {
            &self.notice
        } else if matches!(self.panel, Panel::Question { .. }) {
            "Enter answer · Esc dismiss · Ctrl+C interrupt"
        } else if matches!(self.panel, Panel::Menu(_)) {
            "↑↓ select · Enter open · PgUp/PgDn scroll · Esc close"
        } else if !matches!(self.panel, Panel::Chat) {
            "Enter select · PgUp/PgDn scroll · Esc cancel"
        } else if let Some(status) = &self.model.status {
            status
        } else if self.model.phase == Phase::Busy {
            "Enter to add instructions"
        } else {
            "/ commands   ·   Alt+Enter newline"
        }
    }

    pub async fn submit(&mut self, endpoint: &Endpoint) -> Result<bool> {
        let input = self.editor.lines().join("\n");
        let text = input.trim();
        self.notice.clear();
        match &self.panel {
            Panel::Legacy(s, path) => {
                let path = if text.is_empty() {
                    path.clone()
                } else {
                    text.to_owned()
                };
                self.open(endpoint, Some(&s.clone()), Some(&path)).await?;
            }
            Panel::Question {
                id,
                questions,
                answers,
            } => {
                let answer = parse_answer(&questions[answers.len()], text)?;
                let mut next = answers.clone();
                next.push(answer);
                if next.len() == questions.len() {
                    self.control()?.reply_question(id, next).await?;
                    self.end_form();
                    return Ok(false);
                } else if let Panel::Question { answers, .. } = &mut self.panel {
                    *answers = next;
                }
            }
            Panel::Menu(_) => {
                self.choose_menu(endpoint, text).await?;
                return Ok(false);
            }
            Panel::Chat => {
                let (cmd, arg) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
                if let Some(spec) = crate::commands::lookup(cmd) {
                    if spec.idle && self.model.phase == Phase::Busy {
                        bail!("Wait for the current turn or interrupt it first.");
                    }
                    let exit = self.run_command(endpoint, spec.cmd, arg.trim()).await?;
                    if matches!(self.panel, Panel::Chat) {
                        if let Some(draft) = self.command_draft.take() {
                            self.editor = draft;
                        } else if self.editor.lines().join("\n") == input {
                            self.editor = editor();
                        }
                    } else {
                        self.editor = editor();
                    }
                    return Ok(exit);
                } else if cmd.starts_with('/') && !cmd[1..].contains('/') {
                    bail!("Unknown command {cmd} (see /help)");
                } else if !text.is_empty() || !self.attachments.is_empty() {
                    ensure!(
                        !endpoint.keys.missing(&self.model_name),
                        "Set the API key for this model in /settings."
                    );
                    self.send_prompt(text.to_owned()).await?;
                }
            }
        }
        self.editor = editor();
        Ok(false)
    }
}

mod menu;

fn parse_answer(question: &Question, input: &str) -> Result<Vec<String>> {
    if input.is_empty() {
        return Ok(vec![]);
    }
    if !input.starts_with('=')
        && !question.options.is_empty()
        && input.split(',').all(|s| s.trim().parse::<usize>().is_ok())
    {
        let mut labels = Vec::new();
        for s in input.split(',') {
            let i: usize = s.trim().parse()?;
            let option = question
                .options
                .get(i.wrapping_sub(1))
                .context("unknown option number")?;
            ensure!(!labels.contains(&option.label), "option already selected");
            labels.push(option.label.clone());
        }
        ensure!(question.multiple || labels.len() == 1, "choose one option");
        return Ok(labels);
    }
    ensure!(question.custom, "choose an offered option number");
    let value = input.strip_prefix('=').unwrap_or(input);
    ensure!(
        value.chars().count() <= 1000,
        "answer exceeds 1000 characters"
    );
    Ok(vec![value.to_owned()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn measured_footer_survives_a_new_turn_snapshot_but_not_a_rewritten_reply() {
        let mut history = vec![
            Block::User("first".into()),
            Block::Assistant("one".into()),
            Block::User("second".into()),
            Block::Assistant("revised".into()),
        ];
        let mut previous = history[..2].to_vec();
        previous.extend([
            Block::Info("Worked for 2s · 09:00".into()),
            history[2].clone(),
            Block::Assistant("two".into()),
            Block::Info("Worked for 3s · 09:01".into()),
        ]);
        restore_turn_footers(&previous, &mut history);
        assert_eq!(history.len(), 5);
        assert_eq!(history[2], previous[2]);
    }

    #[test]
    fn welcome_screen_anchors_composer_and_displays_configured_effort() {
        for (width, height) in [(120, 30), (60, 16)] {
            let mut chat = Chat::start("/work".into(), "deepseek/flash".into(), "build", None);
            chat.catalog.push(gewu_protocol::ModelSummary {
                selector: "deepseek/flash".into(),
                label: "deepseek-flash".into(),
                default_effort: Some("high".into()),
                supports_reasoning_effort: true,
            });
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::view::draw(frame, &mut chat))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y: u16| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert_eq!(row(height - 4).trim(), "›");
            for y in height - 5..=height - 3 {
                for x in [0, width - 1] {
                    assert_eq!(buffer[(x, y)].bg, ratatui::style::Color::Rgb(58, 63, 75));
                }
            }
            assert_eq!(terminal.backend().cursor_position(), (3, height - 4).into());
            assert!(row(height - 2).contains("deepseek-flash high"));
            assert!((0..height - 5).any(|y| row(y).contains("/resume")));
            assert_eq!(
                buffer[(width - 2, height - 3)].bg,
                ratatui::style::Color::Rgb(58, 63, 75)
            );
            chat.editor.insert_str("/");
            chat.command_index = 1;
            terminal
                .draw(|frame| crate::view::draw(frame, &mut chat))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let highlighted = (0..height)
                .find(|&row| {
                    buffer[(width - 2, row)].bg == ratatui::style::Color::Rgb(96, 165, 250)
                })
                .unwrap();
            assert!(highlighted < height - 5);
            assert_eq!(buffer[(1, highlighted)].symbol(), "›");
            assert_eq!(buffer[(3, height - 4)].symbol(), "/");
        }
    }

    #[test]
    fn composer_tracks_chinese_caret_and_repairs_the_row_tail() {
        use ratatui::{Terminal, backend::TestBackend, style::Color};
        use ratatui_textarea::CursorMove;
        let mut chat = Chat::start("/work".into(), "test".into(), "build", None);
        let mut terminal = Terminal::new(TestBackend::new(32, 16)).unwrap();
        chat.editor.insert_str("你是");
        terminal
            .draw(|frame| crate::view::draw(frame, &mut chat))
            .unwrap();
        assert_eq!(terminal.backend().cursor_position(), (7, 12).into());
        chat.editor.move_cursor(CursorMove::Back);
        terminal
            .draw(|frame| crate::view::draw(frame, &mut chat))
            .unwrap();
        assert_eq!(terminal.backend().cursor_position(), (5, 12).into());
        chat.editor.delete_char();
        // Simulate the unreported screen damage after an IME commits a word.
        let blank = ratatui::buffer::Cell::default();
        ratatui::backend::Backend::draw(
            terminal.backend_mut(),
            [0, 30, 31].into_iter().map(|x| (x, 12, &blank)),
        )
        .unwrap();
        terminal
            .draw(|frame| crate::view::draw(frame, &mut chat))
            .unwrap();
        for x in [0, 30, 31] {
            assert_eq!(
                terminal.backend().buffer()[(x, 12)].bg,
                Color::Rgb(58, 63, 75)
            );
        }
        chat.editor.move_cursor(CursorMove::End);
        chat.editor.insert_newline();
        chat.editor.insert_str("中a");
        terminal
            .draw(|frame| crate::view::draw(frame, &mut chat))
            .unwrap();
        assert_eq!(terminal.backend().cursor_position(), (6, 12).into());
    }

    #[test]
    fn slash_menu_filters_completes_and_ignores_form_fields() {
        let mut chat = Chat::start("/project".into(), "test".into(), "build", None);
        chat.editor.insert_str("/");
        assert_eq!(chat.suggestions().len(), crate::commands::COMMANDS.len());
        chat.editor.insert_str("co");
        assert_eq!(chat.suggestions()[0].0, "/context");
        chat.complete_command("/context");
        assert_eq!(chat.editor.lines(), &["/context"]);
        assert!(chat.suggestions().is_empty());
        chat.commands_hidden = false;
        chat.show_sessions(vec![]);
        assert!(chat.suggestions().is_empty());
    }

    #[test]
    fn context_tracks_latest_active_window_without_adding_messages() {
        let mut chat = Chat::start("/project".into(), "test".into(), "build", None);
        for used_tokens in [85_000, 16_000] {
            chat.apply_event(gewu_protocol::AgentEvent::Usage {
                used_tokens,
                context_limit: 100_000,
                provider: None,
            });
            assert_eq!(chat.context, Some((used_tokens, 100_000)));
        }
        assert!(chat.model.blocks.is_empty());
    }

    #[test]
    fn reconnect_replaces_history_and_keeps_an_unanswered_form_and_draft() {
        let mut chat = Chat::start("/project".into(), "test".into(), "build", None);
        chat.editor.insert_str("保留输入");
        let frame = |event| json!({"type":"event","id":null,"request_id":"r","event":event});
        let snapshot: ServerMessage = serde_json::from_value(json!({
            "type":"session_snapshot","session_id":"s","busy":true,"seq":3,"messages":[],
            "frames":[frame(json!({"type":"content_delta","delta":"hello"})),
                      frame(json!({"type":"tool_start","tool":"question","input":"{}"})),
                      frame(json!({"type":"question_asked","request_id":"q","questions":[{
                        "header":"Pick","question":"Which?", "options":[{"label":"A","description":"first"},{"label":"B","description":"second"}],"multiple":true,"custom":false
                      }]}))]
        })).unwrap();
        for _ in 0..2 {
            chat.apply(snapshot.clone()).unwrap();
            assert_eq!(chat.model.blocks.len(), 2);
            let Panel::Question { questions, .. } = &chat.panel else {
                panic!("missing question");
            };
            assert_eq!(parse_answer(&questions[0], "2,1").unwrap(), ["B", "A"]);
            assert!(parse_answer(&questions[0], "custom").is_err());
        }
        chat.apply(
            serde_json::from_value(frame(
                json!({"type":"question_resolved","request_id":"q","answers":[["B"]]}),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(chat.editor.lines(), &["保留输入"]);
        assert!(matches!(chat.panel, Panel::Chat));
    }
}
