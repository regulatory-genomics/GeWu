//! One browser connection's workspace and upstream event loop.
//!
//! Each browser gets an isolated [`Workspace`] and its own lazily-created
//! `gewu_client::Client` per tab (the server pins one session per connection).
//! The output is JSON messages carrying rendered HTML.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use gewu_client::session::{Block, Phase};
use gewu_client::{AskStream, Client, Error as ClientError};
use gewu_protocol::{AgentEvent, ServerMessage};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::cli::Endpoint;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::view;
use crate::workspace::{Workspace, short_id};

type Sender = SplitSink<WebSocket, Message>;

/// Per-tab upstream connection plus the task forwarding its ask frames.
#[derive(Default)]
struct Conn {
    client: Option<Client>,
    observer: Option<Client>,
    observing: bool,
    task: Option<JoinHandle<()>>,
    /// When the streaming Markdown body was last pushed, throttling
    /// re-renders to [`STREAM_INTERVAL`]. `None` means the next delta is due.
    last_stream_render: Option<Instant>,
}

/// Minimum time between full Markdown re-renders of a streaming block.
const STREAM_INTERVAL: Duration = Duration::from_millis(100);

/// A frame forwarded from a tab's ask stream; `None` marks the stream's end.
struct TabEvent {
    tab_id: u64,
    frame: Option<gewu_client::Result<ServerMessage>>,
}

/// What one tab event changed, driving the browser update. Targeted updates
/// avoid re-rendering the whole transcript on every streamed frame.
enum TabUpdate {
    /// Nothing visible changed.
    None,
    /// Rebuild the whole transcript (terminal frames, notes, unknown shapes).
    Full,
    /// Append one rendered block at the transcript's end.
    BlockAppend(usize),
    /// Replace one tool card's contents in place (`run` is the run's first
    /// block index).
    ToolCard(usize),
    /// Append a new tool card at the transcript's end (start of a tool run).
    ToolCardStart(usize),
    /// Replace one streaming block's rendered Markdown body in place.
    StreamingBody(usize),
}

/// The targeted update for a tool event: replace the run's card, or start a
/// new one when the run itself just started. Falls back to a full render
/// when the shape is unexpected.
fn tool_update(blocks: &[Block], affected: Option<usize>, appended: bool) -> TabUpdate {
    let Some(index) = affected else {
        return TabUpdate::Full;
    };
    let Some(run_start) = tool_run_start(blocks, index) else {
        return TabUpdate::Full;
    };
    let new_run = appended && (index == 0 || !matches!(blocks[index - 1], Block::Tool { .. }));
    if new_run {
        TabUpdate::ToolCardStart(run_start)
    } else {
        TabUpdate::ToolCard(run_start)
    }
}

/// The display label for a model selector within the catalog.
fn label_for(models: &[gewu_protocol::ModelSummary], selector: &str) -> Option<String> {
    models
        .iter()
        .find(|model| model.selector == selector)
        .map(|model| model.label.clone())
}

/// One browser's isolated GUI session.
pub struct GuiSession {    endpoint: Endpoint,
    server_token: Option<String>,
    admin: Client,
    workspace: Workspace,
    conns: HashMap<u64, Conn>,
    model_label: Option<String>,
    /// Switchable models from the upstream's catalog (empty when the upstream
    /// predates `list_models` or exposes a single model).
    models: Vec<gewu_protocol::ModelSummary>,
    /// Tool names for the sidebar, fetched once; fixed for the default agent.
    tools: Result<Vec<String>, String>,
    /// Working directory the upstream server's agent operates in.
    working_directory: Option<String>,
    expanded_dirs: BTreeSet<String>,
    file_cache: BTreeMap<String, Vec<gewu_protocol::FileEntry>>,
    tab_tx: Option<mpsc::UnboundedSender<TabEvent>>,
}

enum Incoming {
    Socket(Option<Result<Message, axum::Error>>),
    Tab(Option<TabEvent>),
}

impl GuiSession {
    /// Connect upstream and load the stored sessions into tabs.
    pub async fn new(endpoint: &Endpoint, server_token: Option<&str>) -> Result<Self> {
        let admin = endpoint
            .connect_with_token(server_token)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to the gewu server at {} (is `gewu-server serve` running?)",
                    endpoint.display()
                )
            })?;

        let (model_label, working_directory) = match admin.info().await {
            // The status bar shows the model name without the provider.
            // An empty directory means the upstream predates `info.cwd`.
            Ok(info) => (
                Some(info.model.clone()),
                Some(info.working_directory.clone()).filter(|dir| !dir.is_empty()),
            ),
            Err(e) => {
                debug!("could not read server info: {e}");
                (None, None)
            }
        };

        let tools = admin.list_tools().await.map_err(|e| {
            debug!("could not list tools: {e}");
            e.to_string()
        });

        let models = match admin.list_models().await {
            Ok(models) => models,
            Err(e) => {
                debug!("could not list models: {e}");
                Vec::new()
            }
        };

        let mut file_cache = BTreeMap::new();
        if let Ok(entries) = admin.list_files("").await {
            file_cache.insert(String::new(), entries);
        }

        let mut workspace = Workspace::new();

        // `list_sessions` is newest-first; opening in reverse puts the oldest
        // tab on the left and focuses the newest.
        match admin.list_sessions().await {
            Ok(sessions) => {
                for summary in sessions.into_iter().rev() {
                    match admin.get_session(&summary.session_id).await {
                        Ok(messages) => {
                            let id = workspace.open_session(&summary.session_id, &messages);
                            workspace.tab_mut(id).expect("opened").agent = summary.agent.clone();
                        }
                        Err(e) => {
                            let id = workspace.open_blank();
                            workspace
                                .tab_mut(id)
                                .expect("just added")
                                .model
                                .note(format!(
                                    "could not load session {}: {e}",
                                    short_id(&summary.session_id)
                                ));
                        }
                    }
                }
            }
            Err(e) => {
                let id = workspace.open_blank();
                workspace
                    .tab_mut(id)
                    .expect("just added")
                    .model
                    .note(format!("could not list sessions: {e}"));
            }
        }
        if workspace.is_empty() {
            workspace.open_blank();
        }
        for tab in &mut workspace.tabs {
            tab.model_label = model_label.clone();
        }

        Ok(Self {
            endpoint: endpoint.clone(),
            server_token: server_token.map(str::to_string),
            admin,
            workspace,
            conns: HashMap::new(),
            model_label,
            models,
            tools,
            working_directory,
            expanded_dirs: BTreeSet::new(),
            file_cache,
            tab_tx: None,
        })
    }

    /// Serve one browser WebSocket until it disconnects.
    pub async fn run(mut self, socket: WebSocket) -> Result<()> {
        let (mut sender, mut receiver) = socket.split();
        let (tab_tx, mut tab_rx) = mpsc::unbounded_channel::<TabEvent>();
        // Keep a sender alive so `tab_rx.recv()` only yields real events.
        let _tab_keepalive = tab_tx.clone();
        self.tab_tx = Some(tab_tx.clone());

        match self.admin.ping().await {
            Ok(()) => self.note_active("connected"),
            Err(e) => self.fail_active(format!("server not responding: {e}")),
        }

        send_msg(&mut sender, &self.snapshot()).await?;

        // Refresh context usage for the focused session. This may create the
        // tab's upstream connection.
        self.activate_active(&mut sender).await?;

        let ids: Vec<_> = self.workspace.tabs.iter().filter_map(|tab| tab.model.session_id.clone().map(|s| (tab.id, s))).collect();
        for (id, session_id) in ids {
            // One bad stored session must not prevent the browser session
            // from starting; log and continue with the remaining tabs.
            if let Err(e) = self.ensure_client(id).await {
                warn!("failed to restore connection for tab {id}: {e}");
                continue;
            }
            if let Err(e) = self.observe(id, session_id, true).await {
                warn!("failed to restore observer for tab {id}: {e}");
            }
        }

        loop {
            let incoming = tokio::select! {
                message = receiver.next() => Incoming::Socket(message),
                event = tab_rx.recv() => Incoming::Tab(event),
            };

            match incoming {
                Incoming::Socket(None) => break,
                Incoming::Socket(Some(Err(e))) => {
                    warn!("browser websocket error: {e}");
                    break;
                }
                Incoming::Socket(Some(Ok(Message::Text(text)))) => {
                    let msg = match serde_json::from_str::<ClientMsg>(text.as_str()) {
                        Ok(msg) => msg,
                        Err(e) => {
                            self.send_error(&mut sender, format!("invalid message: {e}"))
                                .await?;
                            continue;
                        }
                    };
                    if let Err(e) = self.handle_client(msg, &mut sender, &tab_tx).await {
                        warn!("handling browser message failed: {e}");
                        break;
                    }
                }
                Incoming::Socket(Some(Ok(Message::Close(_)))) => break,
                // Ping/pong and binary frames need no handling.
                Incoming::Socket(Some(Ok(_))) => {}
                Incoming::Tab(Some(event)) => {
                    // A failed upstream call for one tab must not disconnect
                    // the whole browser session.
                    if let Err(e) = self.handle_tab_event(event, &mut sender).await {
                        warn!("handling tab event failed: {e}");
                        self.send_error(&mut sender, e.to_string()).await?;
                    }
                }
                // The keepalive sender means this cannot normally happen.
                Incoming::Tab(None) => break,
            }
        }

        self.shutdown().await;
        Ok(())
    }

    fn snapshot(&self) -> ServerMsg {
        let active = self.workspace.active_id();
        let tab = self
            .workspace
            .tab(active)
            .expect("workspace is never empty");
        ServerMsg::Snapshot {
            tabs: self.workspace.infos(),
            active,
            transcript: view::transcript_html(tab),
            status: view::status_html(tab, &self.models),
            sidebar: view::sidebar_tabs_html(
                &self.tools,
                self.working_directory.as_deref(),
                self.working_directory.as_deref(),
                &self.expanded_dirs,
                &self.file_cache,
            ),
        }
    }

    async fn send_tabs(&self, sender: &mut Sender) -> Result<()> {
        send_msg(
            sender,
            &ServerMsg::Tabs {
                tabs: self.workspace.infos(),
                active: self.workspace.active_id(),
            },
        )
        .await?;
        Ok(())
    }

    async fn send_status(&self, sender: &mut Sender) -> Result<()> {
        let tab = self
            .workspace
            .tab(self.workspace.active_id())
            .expect("workspace is never empty");
        send_msg(
            sender,
            &ServerMsg::Status {
                html: view::status_html(tab, &self.models),
            },
        )
        .await?;
        Ok(())
    }

    /// Send just the tool card containing `block`, so toggles do not re-render
    /// the whole transcript. Falls back to a full transcript if the card
    /// cannot be located.
    async fn send_tool_card(&self, tab_id: u64, block: usize, sender: &mut Sender) -> Result<()> {
        if let Some(tab) = self.workspace.tab(tab_id)
            && let Some(first) = tool_run_start(&tab.model.blocks, block)
            && let Some(html) = view::tool_card_html(tab, first)
        {
            return send_msg(sender, &ServerMsg::ToolCard { first, html }).await;
        }
        self.send_transcript(sender).await
    }

    /// Send just a thought block's body, so toggling its reasoning does not
    /// re-render the whole transcript. Falls back to a full transcript when
    /// the block cannot be located.
    async fn send_thought_body(&self, tab_id: u64, block: usize, sender: &mut Sender) -> Result<()> {
        if let Some(tab) = self.workspace.tab(tab_id)
            && let Some(html) = view::assistant_body_html(tab, block, false)
        {
            return send_msg(sender, &ServerMsg::StreamingBody { index: block, html }).await;
        }
        self.send_transcript(sender).await
    }

    async fn send_transcript(&self, sender: &mut Sender) -> Result<()> {
        let tab = self
            .workspace
            .tab(self.workspace.active_id())
            .expect("workspace is never empty");
        send_msg(
            sender,
            &ServerMsg::Transcript {
                html: view::transcript_html(tab),
            },
        )
        .await?;
        Ok(())
    }

    async fn send_active_full(&self, sender: &mut Sender) -> Result<()> {
        self.send_tabs(sender).await?;
        self.send_transcript(sender).await?;
        self.send_status(sender).await?;
        Ok(())
    }

    async fn send_error(&self, sender: &mut Sender, message: impl Into<String>) -> Result<()> {
        send_msg(
            sender,
            &ServerMsg::Error {
                message: message.into(),
            },
        )
        .await?;
        Ok(())
    }

    async fn send_notice(&self, sender: &mut Sender, message: impl Into<String>) -> Result<()> {
        send_msg(
            sender,
            &ServerMsg::Notice {
                message: message.into(),
            },
        )
        .await?;
        Ok(())
    }

    /// Reload every visible tree level from the upstream and push the fresh
    /// rendering to the browser.
    async fn send_refreshed_tree(&mut self, sender: &mut Sender) -> Result<()> {
        let paths: Vec<String> = std::iter::once(String::new())
            .chain(self.expanded_dirs.iter().cloned())
            .collect();
        for path in paths {
            if let Ok(entries) = self.admin.list_files(path.clone()).await {
                self.file_cache.insert(path, entries);
            }
        }
        send_msg(
            sender,
            &ServerMsg::FileTree {
                html: view::file_tree_html(
                    self.working_directory.as_deref(),
                    &self.expanded_dirs,
                    &self.file_cache,
                ),
            },
        )
        .await?;
        Ok(())
    }

    fn note_active(&mut self, message: impl Into<String>) {
        self.workspace.active_tab().model.note(message);
    }

    fn fail_active(&mut self, message: impl Into<String>) {
        self.workspace.active_tab().model.fail(message);
    }

    /// The focused tab's own connection, created on first use.
    async fn ensure_client(&mut self, tab_id: u64) -> Result<(), ClientError> {
        if self
            .conns
            .get(&tab_id)
            .and_then(|conn| conn.client.as_ref())
            .is_some()
        {
            return Ok(());
        }
        let endpoint = self.endpoint.clone();
        let client = endpoint
            .connect_with_token(self.server_token.as_deref())
            .await?;
        self.conns.entry(tab_id).or_default().client = Some(client);
        Ok(())
    }

    fn client(&self, tab_id: u64) -> Option<Client> {
        self.conns
            .get(&tab_id)
            .and_then(|conn| conn.client.as_ref())
            .cloned()
    }

    async fn observe(&mut self, tab_id: u64, session_id: String, restore: bool) -> Result<()> {
        let observer = self.endpoint.connect_with_token(self.server_token.as_deref()).await?;
        let mut stream = observer.watch_session(session_id).await?;
        let tx = self.tab_tx.clone().expect("running event loop");
        let task = tokio::spawn(async move {
            let mut first = true;
            while let Some(frame) = stream.next_frame().await {
                // After our own turn finishes, the local model already has
                // every event. Do not replace it with an idle snapshot and
                // shift block indices by discarding connection-local notes.
                let skip = first && !restore && matches!(&frame,
                    Ok(ServerMessage::SessionSnapshot { busy: false, .. }));
                first = false;
                if skip { continue; }
                if tx.send(TabEvent { tab_id, frame: Some(frame) }).is_err() { return; }
            }
        });
        let conn = self.conns.entry(tab_id).or_default();
        if let Some(previous) = conn.task.replace(task) { previous.abort(); }
        if let Some(previous) = conn.observer.replace(observer) { previous.close().await; }
        conn.observing = true;
        Ok(())
    }

    /// On focusing a tab: make sure it has a connection and refresh usage.
    async fn activate_active(&mut self, sender: &mut Sender) -> Result<()> {
        let tab_id = self.workspace.active_id();
        let Some(session_id) = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.model.session_id.clone())
        else {
            self.send_status(sender).await?;
            return Ok(());
        };

        if let Err(e) = self.ensure_client(tab_id).await {
            self.workspace
                .tab_mut(tab_id)
                .expect("active tab")
                .model
                .note(format!("context usage unavailable: {e}"));
            self.send_active_full(sender).await?;
            return Ok(());
        }

        let client = self.client(tab_id).expect("just ensured");
        match client.usage(Some(&session_id)).await {
            Ok(usage) if usage.used_tokens > 0 && usage.context_limit > 0 => {
                let tab = self.workspace.tab_mut(tab_id).expect("active tab");
                tab.context = Some((usage.used_tokens, usage.context_limit));
                tab.cache = usage.provider_usage;
            }
            Ok(_) => {}
            Err(e) => {
                self.workspace
                    .tab_mut(tab_id)
                    .expect("active tab")
                    .model
                    .note(format!("context usage unavailable: {e}"));
                self.send_active_full(sender).await?;
                return Ok(());
            }
        }
        self.send_status(sender).await
    }

    /// Refresh a tab's context usage after an ask, silently on failure.
    async fn refresh_usage(&mut self, tab_id: u64) {
        let Some(client) = self.client(tab_id) else {
            return;
        };
        let session_id = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.model.session_id.clone());
        match client.usage(session_id.as_deref()).await {
            Ok(usage) if usage.used_tokens > 0 && usage.context_limit > 0 => {
                if let Some(tab) = self.workspace.tab_mut(tab_id) {
                    tab.context = Some((usage.used_tokens, usage.context_limit));
                    tab.cache = usage.provider_usage;
                }
            }
            Ok(_) => {}
            Err(e) => debug!("context usage refresh failed: {e}"),
        }
    }

    async fn handle_client(
        &mut self,
        msg: ClientMsg,
        sender: &mut Sender,
        tab_tx: &mpsc::UnboundedSender<TabEvent>,
    ) -> Result<()> {
        match msg {
            ClientMsg::RevertChat { tab, block, undo } => {
                let Some(tab_state) = self.workspace.tab(tab) else { return self.send_error(sender, "unknown tab").await; };
                if tab_state.busy() { return self.send_error(sender, "wait for the current turn to finish").await; }
                let turn = if let Some(index) = block {
                    if !matches!(tab_state.model.blocks.get(index), Some(Block::User(_))) { return self.send_error(sender, "not a user message").await; }
                    Some(tab_state.model.blocks[..index].iter().filter(|b| matches!(b, Block::User(_))).count())
                } else { None };
                let session_id = tab_state.model.session_id.clone();
                if session_id.is_none() { return self.send_error(sender, "no conversation to revert").await; }
                if let Err(e) = self.ensure_client(tab).await { return self.send_error(sender, e.to_string()).await; }
                let client = self.client(tab).expect("ensured");
                if let Err(e) = client.usage(session_id.as_deref()).await { return self.send_error(sender, e.to_string()).await; }
                match client.revert_chat(turn, undo).await {
                    Ok((id, messages)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known");
                        tab_state.model.load_history(&messages);
                        tab_state.model.set_session(Some(id));
                        tab_state.model.note("Conversation updated. Workspace files were NOT undone.");
                        tab_state.expanded_groups.clear();
                        tab_state.expanded_outputs.clear();
                        tab_state.context = None;
                        if self.workspace.active_id() == tab { self.send_active_full(sender).await?; }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::SwitchAgent { tab, agent } => {
                if !["build", "plan"].contains(&agent.as_str()) { return self.send_error(sender, "unknown mode").await; }
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) { return self.send_error(sender, "wait for the current turn to finish").await; }
                if let Err(e) = self.ensure_client(tab).await { return self.send_error(sender, e.to_string()).await; }
                let client = self.client(tab).expect("ensured");
                // Bind resumed tabs before switching so their conversation is retained.
                if let Some(id) = self.workspace.tab(tab).and_then(|tab| tab.model.session_id.clone())
                    && let Err(e) = client.usage(Some(&id)).await { return self.send_error(sender, e.to_string()).await; }
                match client.switch_agent(agent.clone()).await {
                    Ok((id, tools)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.model.set_session(Some(id));
                        tab_state.agent = agent;
                        self.tools = Ok(tools);
                        if self.workspace.active_id() == tab {
                            self.send_status(sender).await?;
                            send_msg(sender, &ServerMsg::Tools {
                                html: view::sidebar_tools_content_html(&self.tools),
                            }).await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::SwitchModel { tab, model } => {
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) { return self.send_error(sender, "wait for the current turn to finish").await; }
                if let Err(e) = self.ensure_client(tab).await { return self.send_error(sender, e.to_string()).await; }
                let client = self.client(tab).expect("ensured");
                // Bind resumed tabs before switching so their conversation is retained.
                if let Some(id) = self.workspace.tab(tab).and_then(|tab| tab.model.session_id.clone())
                    && let Err(e) = client.usage(Some(&id)).await { return self.send_error(sender, e.to_string()).await; }
                match client.switch_model(model.clone()).await {
                    Ok((id, label)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.model.set_session(Some(id));
                        tab_state.model_selector = Some(model);
                        tab_state.model_label = Some(label);
                        if self.workspace.active_id() == tab {
                            self.send_status(sender).await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            // Auth is consumed by the transport before the session starts.
            ClientMsg::Auth { .. } => {}            ClientMsg::Ask { tab, prompt } => {
                self.handle_ask(tab, prompt, sender, tab_tx).await?;
            }
            ClientMsg::Interrupt { tab } => {
                if let Some(client) = self.client(tab)
                    && let Err(e) = client.interrupt().await
                {
                    self.send_error(sender, format!("could not interrupt: {e}"))
                        .await?;
                }
            }
            ClientMsg::OpenTab => {
                let id = self.workspace.open_blank();
                self.workspace.tab_mut(id).expect("just added").model_label =
                    self.model_label.clone();
                self.send_active_full(sender).await?;
            }
            ClientMsg::CloseTab { tab } => {
                self.close_tab(tab).await;
                self.send_active_full(sender).await?;
            }
            ClientMsg::SwitchTab { tab } => {
                if self.workspace.switch_to_id(tab) {
                    self.send_active_full(sender).await?;
                    self.activate_active(sender).await?;
                }
            }
            ClientMsg::ToggleTool { tab, block } => {
                if let Some(tab) = self.workspace.tab_mut(tab) {
                    toggle(&mut tab.expanded_outputs, block);
                }
                if self.workspace.active_id() == tab {
                    self.send_tool_card(tab, block, sender).await?;
                }
            }
            ClientMsg::ToggleGroup { tab, block } => {
                if let Some(tab) = self.workspace.tab_mut(tab) {
                    toggle(&mut tab.expanded_groups, block);
                }
                if self.workspace.active_id() == tab {
                    self.send_tool_card(tab, block, sender).await?;
                }
            }
            ClientMsg::ToggleThought { tab, block } => {
                if let Some(tab) = self.workspace.tab_mut(tab) {
                    toggle(&mut tab.expanded_thoughts, block);
                }
                if self.workspace.active_id() == tab {
                    self.send_thought_body(tab, block, sender).await?;
                }
            }
            ClientMsg::Refresh => {
                self.send_active_full(sender).await?;
            }
            ClientMsg::ToggleDir { path } => {
                if self.expanded_dirs.contains(&path) {
                    self.expanded_dirs.remove(&path);
                } else {
                    self.expanded_dirs.insert(path.clone());
                    if !self.file_cache.contains_key(&path) {
                        // A failed listing must not tear down the whole
                        // browser session; report it and keep the tree open.
                        match self.admin.list_files(path.clone()).await {
                            Ok(entries) => {
                                self.file_cache.insert(path.clone(), entries);
                            }
                            Err(e) => {
                                self.expanded_dirs.remove(&path);
                                self.send_error(
                                    sender,
                                    format!("failed to list '{}': {e}", path),
                                )
                                .await?;
                            }
                        }
                    }
                }
                send_msg(
                    sender,
                    &ServerMsg::FileTree {
                        html: view::file_tree_html(
                            self.working_directory.as_deref(),
                            &self.expanded_dirs,
                            &self.file_cache,
                        ),
                    },
                )
                .await?;
            }
            ClientMsg::RefreshTree => {
                self.send_refreshed_tree(sender).await?;
            }
            ClientMsg::UploadFiles { files } => {
                let mut uploaded = Vec::new();
                let mut failures = Vec::new();
                for file in files {
                    let Some(name) = sanitize_upload_name(&file.name) else {
                        failures.push(format!("'{}' is not a valid file name", file.name));
                        continue;
                    };
                    use base64::Engine as _;
                    let bytes = match base64::engine::general_purpose::STANDARD
                        .decode(file.content_base64.as_bytes())
                    {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            failures.push(format!("'{name}' is not valid base64"));
                            continue;
                        }
                    };
                    match self.admin.write_file(name.clone(), &bytes).await {
                        Ok((_, bytes_written)) => uploaded.push(format!(
                            "{name} ({} kB)",
                            bytes_written / 1024
                        )),
                        Err(e) => failures.push(format!("could not upload '{name}': {e}")),
                    }
                }
                if !uploaded.is_empty() {
                    self.send_refreshed_tree(sender).await?;
                    self.send_notice(sender, format!("Uploaded {}", uploaded.join(", ")))
                        .await?;
                }
                if !failures.is_empty() {
                    self.send_error(sender, failures.join("; ")).await?;
                }
            }
            ClientMsg::ReadFile { path } => match self.admin.read_file(path.clone()).await {
                Ok((content, truncated)) => {
                    send_msg(
                        sender,
                        &ServerMsg::FilePreview {
                            html: view::file_preview_html(&path, &content, truncated),
                        },
                    )
                    .await?
                }
                Err(e) => {
                    self.send_error(sender, format!("could not preview file: {e}"))
                        .await?
                }
            },
            ClientMsg::ClosePreview => send_msg(sender, &ServerMsg::ClosePreview).await?,
        }
        Ok(())
    }

    async fn handle_ask(
        &mut self,
        tab_id: u64,
        prompt: String,
        sender: &mut Sender,
        tab_tx: &mpsc::UnboundedSender<TabEvent>,
    ) -> Result<()> {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return self.send_error(sender, "prompt is empty").await;
        }
        let Some(tab) = self.workspace.tab(tab_id) else {
            return self
                .send_error(sender, format!("unknown tab {tab_id}"))
                .await;
        };
        if tab.model.phase != Phase::Idle {
            return self
                .send_error(
                    sender,
                    "wait for the current reply to finish or interrupt it",
                )
                .await;
        }

        if let Err(e) = self.ensure_client(tab_id).await {
            self.workspace
                .tab_mut(tab_id)
                .expect("tab exists")
                .model
                .fail(format!("could not connect: {e}"));
            return self.send_after_tab_change(tab_id, sender).await;
        }

        let session_id = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.model.session_id.clone());
        let client = self.client(tab_id).expect("just ensured");
        let mut builder = client.ask(prompt.clone());
        if let Some(session_id) = session_id {
            builder = builder.session_id(session_id);
        }

        match builder.send().await {
            Ok(stream) => {
                if self.conns.get(&tab_id).is_some_and(|conn| conn.observing) {
                    // The observer receives the authoritative new-turn snapshot;
                    // do not also apply the initiating connection's deltas.
                    drop(stream);
                    return Ok(());
                }
                let user_block = {
                    let tab = self.workspace.tab_mut(tab_id).expect("tab exists");
                    tab.model.submit(prompt);
                    tab.model.blocks.len() - 1
                };
                let task = spawn_ask_forwarder(tab_id, stream, tab_tx.clone());
                if let Some(conn) = self.conns.get_mut(&tab_id) {
                    conn.task = Some(task);
                }
                // Push just the new user block: every streaming update that
                // follows is targeted, so no full transcript render is needed.
                let active = self.workspace.active_id() == tab_id;
                if active {
                    let html = self
                        .workspace
                        .tab(tab_id)
                        .and_then(|tab| view::block_html(tab, user_block));
                    match html {
                        Some(html) => {
                            send_msg(
                                sender,
                                &ServerMsg::BlockAppend {
                                    index: user_block,
                                    html,
                                },
                            )
                            .await?;
                        }
                        None => self.send_transcript(sender).await?,
                    }
                    self.send_status(sender).await?;
                }
                self.send_tabs(sender).await?;
            }
            Err(e) => {
                self.workspace
                    .tab_mut(tab_id)
                    .expect("tab exists")
                    .model
                    .fail(format!("could not send: {e}"));
                self.send_after_tab_change(tab_id, sender).await?;
            }
        }
        Ok(())
    }

    async fn send_after_tab_change(&self, tab_id: u64, sender: &mut Sender) -> Result<()> {
        if self.workspace.active_id() == tab_id {
            self.send_active_full(sender).await
        } else {
            self.send_tabs(sender).await
        }
    }

    /// Close a tab: stop any running ask, release its connection, delete the
    /// session from the store, then remove the tab. If deletion fails the tab
    /// stays open with an error note.
    async fn close_tab(&mut self, tab_id: u64) {
        if let Some(mut conn) = self.conns.remove(&tab_id) {
            if let Some(task) = conn.task.take() {
                if let Some(client) = &conn.client {
                    let _ = client.interrupt().await;
                }
                if conn.observing { task.abort(); }
                let _ = task.await;
            }
            if let Some(client) = conn.client.take() {
                client.close().await;
            }
            if let Some(observer) = conn.observer.take() { observer.close().await; }
        }

        let session_id = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.model.session_id.clone());
        if let Some(session_id) = session_id
            && let Err(e) = self.admin.delete_session(&session_id).await
        {
            if let Some(tab) = self.workspace.tab_mut(tab_id) {
                tab.model.note(format!("could not delete session: {e}"));
            }
            return;
        }

        self.workspace.close(tab_id);
        if self.workspace.is_empty() {
            let id = self.workspace.open_blank();
            self.workspace.tab_mut(id).expect("just added").model_label = self.model_label.clone();
        }
    }

    /// Whether enough time has passed since the tab's last streaming render
    /// to push a fresh one.
    fn stream_update_due(&self, tab_id: u64) -> bool {
        self.conns
            .get(&tab_id)
            .and_then(|conn| conn.last_stream_render)
            .is_none_or(|last| last.elapsed() >= STREAM_INTERVAL)
    }

    async fn handle_tab_event(&mut self, event: TabEvent, sender: &mut Sender) -> Result<()> {
        let active_id = self.workspace.active_id();
        let mut update = TabUpdate::None;
        let mut terminal = false;
        let mut usage_updated = false;
        // Index of a thought block that an applied event collapsed (live
        // "Thinking…" settled to "Thought Ns"); the triggering event usually
        // appends a different block, so the settled body needs its own swap.
        let mut collapsed = None;
        // Decide the throttle before borrowing the tab below.
        let stream_due = self.stream_update_due(event.tab_id);

        {
            let Some(tab) = self.workspace.tab_mut(event.tab_id) else {
                return Ok(()); // tab closed mid-ask; drop late frames
            };
            let before = tab.model.blocks.len();

            match event.frame {
                Some(Ok(ServerMessage::SessionSnapshot { session_id, messages, frames, busy, model: snapshot_model, .. })) => {
                    let mut model = gewu_client::SessionModel::new();
                    model.set_session(Some(session_id));
                    for message in &messages { model.apply_message(message); }
                    for frame in frames {
                        if let ServerMessage::Event { event, .. } = frame
                            && let Ok(event) = serde_json::from_value::<AgentEvent>(event) { model.apply_event(&event); }
                    }
                    model.phase = if busy { Phase::Busy } else { Phase::Idle };
                    let changed = tab.model.blocks != model.blocks || tab.model.phase != model.phase;
                    tab.model = model;
                    if let Some(selector) = snapshot_model {
                        tab.model_selector = Some(selector.clone());
                        tab.model_label = Some(label_for(&self.models, &selector).unwrap_or(selector));
                    }
                    terminal = !busy;
                    update = if changed { TabUpdate::Full } else { TabUpdate::None };
                }
                None => {
                    terminal = true;
                    update = TabUpdate::Full;
                }
                Some(Ok(ServerMessage::Event { event, .. })) => {
                    match serde_json::from_value::<AgentEvent>(event) {
                        Ok(AgentEvent::ContentDelta { delta: text }) => {
                            let affected = tab
                                .model
                                .apply_event(&AgentEvent::ContentDelta { delta: text });
                            collapsed = tab.model.take_collapsed();
                            if tab.model.blocks.len() != before {
                                // A fresh block: render it immediately; the
                                // throttle window starts on its first update.
                                update = affected.map_or(TabUpdate::Full, TabUpdate::BlockAppend);
                            } else if stream_due {
                                update = affected.map_or(TabUpdate::Full, TabUpdate::StreamingBody);
                            }
                            // Otherwise the delta is only in the model and
                            // rides along with the next due render.
                        }
                        Ok(AgentEvent::Usage {
                            used_tokens,
                            context_limit,
                            provider,
                        }) => {
                            if used_tokens > 0 && context_limit > 0 {
                                tab.context = Some((used_tokens, context_limit));
                            }
                            if let Some(usage) = provider {
                                tab.cache = Some(usage);
                            }
                            usage_updated = true;
                        }
                        // Reasoning drives the live "Thinking…" indicator;
                        // Finish is followed by Result.
                        Ok(agent_event @ AgentEvent::ReasoningDelta { .. }) => {
                            let affected = tab.model.apply_event(&agent_event);
                            let appended = tab.model.blocks.len() != before;
                            if appended {
                                update = affected.map_or(TabUpdate::Full, TabUpdate::BlockAppend);
                            } else if stream_due {
                                update = affected.map_or(TabUpdate::Full, TabUpdate::StreamingBody);
                            }
                        }
                        // Applying Finish settles a thought left open by a
                        // turn that produced no visible output; nothing else
                        // in the transcript changes.
                        Ok(agent_event @ AgentEvent::Finish { .. }) => {
                            tab.model.apply_event(&agent_event);
                            collapsed = tab.model.take_collapsed();
                        }
                        Ok(AgentEvent::Error { error }) => {
                            let affected = tab.model.apply_event(&AgentEvent::Error { error });
                            collapsed = tab.model.take_collapsed();
                            update = affected.map_or(TabUpdate::Full, TabUpdate::BlockAppend);
                        }
                        Ok(agent_event) => {
                            // ToolStart / ToolFinish: update just the run's card.
                            let affected = tab.model.apply_event(&agent_event);
                            collapsed = tab.model.take_collapsed();
                            let appended = tab.model.blocks.len() != before;
                            update = tool_update(&tab.model.blocks, affected, appended);
                        }
                        Err(e) => {
                            tab.model.note(format!("unrecognized event: {e}"));
                            update = TabUpdate::Full;
                        }
                    }
                }
                Some(Ok(ServerMessage::Result { session_id, .. })) => {
                    tab.model.set_session(Some(session_id));
                    terminal = true;
                    update = TabUpdate::Full;
                }
                Some(Ok(ServerMessage::Error { error, .. })) => {
                    tab.model.fail(error);
                    terminal = true;
                    update = TabUpdate::Full;
                }
                Some(Ok(_)) => {}
                Some(Err(ClientError::Interrupted)) => {
                    tab.model.note("interrupted");
                    terminal = true;
                    update = TabUpdate::Full;
                }
                Some(Err(e)) => {
                    tab.model.fail(e.to_string());
                    terminal = true;
                    update = TabUpdate::Full;
                }
            }

            if terminal {
                tab.model.finish_ask();
            }
        }

        if terminal && let Some(conn) = self.conns.get_mut(&event.tab_id) {
            if !conn.observing { conn.task = None; }
            conn.last_stream_render = None;
        }

        // Correct any drift with the authoritative server-side usage before
        // the terminal status is rendered.
        if terminal {
            self.refresh_usage(event.tab_id).await;
            let session_id = self.workspace.tab(event.tab_id).and_then(|tab| tab.model.session_id.clone());
            if !self.conns.get(&event.tab_id).is_some_and(|conn| conn.observing)
                && let Some(session_id) = session_id { self.observe(event.tab_id, session_id, false).await?; }
        }

        let active = active_id == event.tab_id;
        // An event can collapse an open thought while appending a different
        // block (tool card, assistant text) whose targeted update never
        // re-renders the thought; swap the settled "Thought Ns" body in
        // place so the live label does not stick until the next full render.
        // Full renders (and background tabs, rebuilt on focus) need no swap.
        if active
            && !matches!(update, TabUpdate::Full)
            && let Some(index) = collapsed
        {
            self.send_thought_body(event.tab_id, index, sender).await?;
        }
        match update {
            TabUpdate::None => {}
            TabUpdate::Full => {
                if active {
                    self.send_transcript(sender).await?;
                    self.send_status(sender).await?;
                }
                self.send_tabs(sender).await?;
            }
            TabUpdate::BlockAppend(index) if active => {
                let html = self
                    .workspace
                    .tab(event.tab_id)
                    .and_then(|tab| view::block_html(tab, index));
                match html {
                    Some(html) => {
                        send_msg(sender, &ServerMsg::BlockAppend { index, html }).await?;
                    }
                    None => self.send_transcript(sender).await?,
                }
            }
            TabUpdate::ToolCard(run_start) if active => {
                self.send_tool_card(event.tab_id, run_start, sender).await?;
            }
            TabUpdate::ToolCardStart(run_start) if active => {
                let tab = self.workspace.tab(event.tab_id).expect("active tab");
                let html = view::tool_card_html(tab, run_start);
                let assistant = if run_start > 0 {
                    view::assistant_body_html(tab, run_start - 1, false)
                } else {
                    None
                };
                match html {
                    Some(html) => {
                        let assistant = assistant.map(|body| (run_start - 1, body));
                        send_msg(
                            sender,
                            &ServerMsg::ToolCardStart {
                                first: run_start,
                                html,
                                assistant,
                            },
                        )
                        .await?;
                    }
                    None => self.send_transcript(sender).await?,
                }
            }
            TabUpdate::StreamingBody(index) if active => {
                let tab = self.workspace.tab(event.tab_id).expect("active tab");
                match view::assistant_body_html(tab, index, true) {
                    Some(html) => {
                        if let Some(conn) = self.conns.get_mut(&event.tab_id) {
                            conn.last_stream_render = Some(Instant::now());
                        }
                        send_msg(sender, &ServerMsg::StreamingBody { index, html }).await?;
                    }
                    None => self.send_transcript(sender).await?,
                }
            }
            // Background tabs are not rendered; their DOM is rebuilt from a
            // full transcript when the tab is focused.
            _ => {}
        }
        // A usage event leaves `update` untouched, so it needs its own
        // status push.
        if usage_updated && active {
            self.send_status(sender).await?;
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        for (_, mut conn) in self.conns.drain() {
            if let Some(task) = conn.task.take() {
                task.abort();
            }
            if let Some(client) = conn.client.take() {
                client.close().await;
            }
            if let Some(observer) = conn.observer.take() { observer.close().await; }
        }
        self.admin.close().await;
    }
}

/// Start of the run of consecutive tool blocks containing `block`, or `None`
/// when `block` is not a tool block.
fn tool_run_start(blocks: &[Block], block: usize) -> Option<usize> {
    if !matches!(blocks.get(block), Some(Block::Tool { .. })) {
        return None;
    }
    let mut start = block;
    while start > 0 && matches!(blocks[start - 1], Block::Tool { .. }) {
        start -= 1;
    }
    Some(start)
}

/// Insert `value` into `set` if absent, remove it otherwise.
fn toggle(set: &mut std::collections::HashSet<usize>, value: usize) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

/// Forward one tab's ask frames into the event loop, so background tabs keep
/// streaming while another tab is focused.
fn spawn_ask_forwarder(
    tab_id: u64,
    mut stream: AskStream,
    tx: mpsc::UnboundedSender<TabEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = stream.next_frame().await {
            let terminal = frame.as_ref().is_ok_and(|message| message.is_ask_terminal().is_some()
                || matches!(message, ServerMessage::SessionSnapshot { busy: false, .. }))
                || frame.is_err();
            if tx
                .send(TabEvent {
                    tab_id,
                    frame: Some(frame),
                })
                .is_err()
            {
                return; // event loop gone
            }
            if terminal { return; }
        }
        let _ = tx.send(TabEvent {
            tab_id,
            frame: None,
        });
    })
}

async fn send_msg(sender: &mut Sender, msg: &ServerMsg) -> Result<()> {
    let text = serde_json::to_string(msg).expect("server messages are serializable");
    sender.send(Message::Text(text.into())).await?;
    Ok(())
}

/// Reduce a browser-provided upload name to a bare, safe file name.
/// Mirrors the server-side sanitization: only the final path component
/// survives, and empty/dot names are rejected.
fn sanitize_upload_name(name: &str) -> Option<String> {
    let name = std::path::Path::new(name).file_name()?.to_str()?;
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_flips_membership() {
        let mut set = std::collections::HashSet::new();
        toggle(&mut set, 3);
        assert!(set.contains(&3));
        toggle(&mut set, 3);
        assert!(!set.contains(&3));
    }

    #[test]
    fn sanitize_upload_name_strips_components_and_rejects_dots() {
        assert_eq!(sanitize_upload_name("notes.bin").as_deref(), Some("notes.bin"));
        // Only the final component survives traversal attempts.
        assert_eq!(sanitize_upload_name("../evil.txt").as_deref(), Some("evil.txt"));
        assert_eq!(
            sanitize_upload_name("some/dir/file.md").as_deref(),
            Some("file.md")
        );
        assert_eq!(
            sanitize_upload_name("/absolute/path/data.csv").as_deref(),
            Some("data.csv")
        );
        assert_eq!(sanitize_upload_name(".."), None);
        assert_eq!(sanitize_upload_name("."), None);
        assert_eq!(sanitize_upload_name(""), None);
        assert_eq!(sanitize_upload_name("   "), None);
    }

    fn tool(tool: &str, output: Option<&str>) -> Block {
        Block::Tool {
            tool: tool.into(),
            input: String::new(),
            output: output.map(str::to_string),
        }
    }

    #[test]
    fn tool_update_replaces_the_run_card() {
        // A fill on an existing run replaces its card.
        let blocks = vec![
            Block::User("hi".into()),
            tool("bash", None),
            tool("bash", Some("out")),
        ];
        assert!(matches!(
            tool_update(&blocks, Some(2), false),
            TabUpdate::ToolCard(1)
        ));

        // A ToolStart extending the run replaces the same card.
        assert!(matches!(
            tool_update(&blocks, Some(2), true),
            TabUpdate::ToolCard(1)
        ));
    }

    #[test]
    fn tool_update_starts_a_card_for_a_new_run() {
        // First tool block after a non-tool block: the run is new.
        let blocks = vec![Block::User("hi".into()), tool("bash", None)];
        assert!(matches!(
            tool_update(&blocks, Some(1), true),
            TabUpdate::ToolCardStart(1)
        ));

        // A new run that follows an earlier run starts its own card.
        let blocks = vec![
            tool("bash", Some("out")),
            Block::Assistant("done".into()),
            tool("grep", None),
        ];
        assert!(matches!(
            tool_update(&blocks, Some(2), true),
            TabUpdate::ToolCardStart(2)
        ));
    }

    #[test]
    fn tool_update_falls_back_to_full_render_on_unexpected_shapes() {
        let blocks = vec![Block::User("hi".into())];
        assert!(matches!(tool_update(&blocks, None, true), TabUpdate::Full));
        // A non-tool affected block cannot be located as a card.
        assert!(matches!(
            tool_update(&blocks, Some(0), false),
            TabUpdate::Full
        ));
    }
}
