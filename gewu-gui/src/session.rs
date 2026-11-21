//! One browser connection's workspace and upstream event loop.
//!
//! Each browser gets an isolated [`Workspace`] and its own lazily-created
//! `gewu_client::Client` per tab (the server pins one session per connection).
//! The output is JSON messages carrying rendered HTML.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use gewu_client::session::{Block, Phase};
use gewu_client::{AskStream, Client, Error as ClientError};
use gewu_protocol::{AgentEvent, ServerMessage, WorkspaceSummary};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::cli::Endpoint;
use crate::download::DownloadTickets;
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
    plugin_views: Vec<String>,
    /// When the streaming Markdown body was last pushed, throttling
    /// re-renders to [`STREAM_INTERVAL`]. `None` means the next delta is due.
    last_stream_render: Option<Instant>,
}

/// Minimum time between full Markdown re-renders of a streaming block.
const STREAM_INTERVAL: Duration = Duration::from_millis(100);

/// Which upstream stream a tab event rode in on. Connection failures on the
/// observer stream are background noise (the watch is rebuilt on the next
/// terminal ask); failures on the ask stream end the visible turn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabSource {
    Ask,
    Observer,
}

/// A frame forwarded from a tab's ask or observer stream; `None` marks the
/// ask stream's end.
struct TabEvent {
    tab_id: u64,
    frame: Option<gewu_client::Result<ServerMessage>>,
    source: TabSource,
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
    /// Replace one rendered block in place (e.g. a steered message's
    /// "queued" badge settling once the agent ingested it).
    BlockReplace(usize),
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
    let new_run =
        appended && (index == 0 || !view::same_tool_run(&blocks[index - 1], &blocks[index]));
    if new_run {
        TabUpdate::ToolCardStart(run_start)
    } else {
        TabUpdate::ToolCard(run_start)
    }
}

/// Whether pending steer entries form a contiguous run of blocks that ends
/// at the transcript's tail — i.e. each rendered block already sits where
/// the agent ingested it, so the confirming `steered` event needs no move.
fn steers_in_place(group: &[(String, usize)], block_count: usize) -> bool {
    let Some((_, index)) = group.first() else {
        return true;
    };
    let mut expected = *index;
    for (_, index) in &group[1..] {
        expected += 1;
        if *index != expected {
            return false;
        }
    }
    expected + 1 == block_count
}

/// Reconcile a live `steered` event against the steers this session already
/// displayed at submit time, returning the targeted transcript update plus
/// any thought block the event settled.
fn confirm_steered(
    model: &mut gewu_client::SessionModel,
    pending: &mut Vec<(String, usize)>,
    text: String,
) -> (TabUpdate, Option<usize>) {
    let Some(pos) = pending.iter().position(|(t, _)| *t == text) else {
        // Not rendered here (another connection steered, or a snapshot
        // rebuilt the transcript): append at the event's position as usual.
        let affected = model.apply_event(&AgentEvent::Steered { text });
        let collapsed = model.take_collapsed();
        return (
            affected.map_or(TabUpdate::Full, TabUpdate::BlockAppend),
            collapsed,
        );
    };
    if steers_in_place(&pending[pos..], model.blocks.len()) {
        // Only later pending steers follow the block: it already sits at
        // the position where the agent injected it. Re-render it so the
        // confirming event settles its "queued" badge.
        let (_, index) = pending[pos];
        pending.remove(pos);
        return (TabUpdate::BlockReplace(index), None);
    }
    // Blocks landed between the submit and the ingestion (the steer raced
    // ahead of the ask's first events): re-anchor the unconfirmed group at
    // the transcript's end, where the agent actually injected them,
    // preserving submit order.
    let mut group = pending.split_off(pos);
    group.sort_by_key(|(_, index)| *index);
    let intact = group.iter().all(|(pending, index)| {
        matches!(
            model.blocks.get(*index),
            Some(Block::User(rendered)) if rendered == pending
        )
    });
    if !intact {
        // The transcript no longer matches the record; stop tracking and
        // let the event append as usual.
        pending.clear();
        let affected = model.apply_event(&AgentEvent::Steered { text });
        let collapsed = model.take_collapsed();
        return (
            affected.map_or(TabUpdate::Full, TabUpdate::BlockAppend),
            collapsed,
        );
    }
    // Remove highest index first so the stored positions stay valid.
    for (_, index) in group.iter().rev() {
        model.blocks.remove(*index);
    }
    let mut kept = Vec::new();
    for (k, (pending, _)) in group.iter().enumerate() {
        let appended = model
            .apply_event(&AgentEvent::Steered {
                text: pending.clone(),
            })
            .unwrap_or_else(|| model.blocks.len().saturating_sub(1));
        if k > 0 {
            // The first entry is the one being confirmed here.
            kept.push((pending.clone(), appended));
        }
    }
    let collapsed = model.take_collapsed();
    pending.extend(kept);
    (TabUpdate::Full, collapsed)
}

/// The display label for a model selector within the catalog.
fn label_for(models: &[gewu_protocol::ModelSummary], selector: &str) -> Option<String> {
    models
        .iter()
        .find(|model| model.selector == selector)
        .map(|model| model.label.clone())
}

/// A workflow run whose graph the zoom panel is showing.
struct ZoomTarget {
    tab: u64,
    run_id: String,
}

/// One browser's isolated GUI session.
pub struct GuiSession {
    endpoint: Endpoint,
    server_token: Option<String>,
    /// Version shown in the session sidebar.
    version: String,
    /// Latest release notice shown in the session sidebar.
    update: Option<gewu_update::Update>,
    /// Release notices published by the GUI's background check.
    updates: watch::Receiver<Option<gewu_update::Update>>,
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
    workspaces: Vec<WorkspaceSummary>,
    closed_workspaces: BTreeSet<String>,
    expanded_dirs: BTreeSet<String>,
    file_cache: BTreeMap<String, Vec<gewu_protocol::FileEntry>>,
    /// Workflow run currently shown in the zoom panel, if any.
    zoom: Option<ZoomTarget>,
    downloads: Arc<DownloadTickets>,
    tab_tx: Option<mpsc::UnboundedSender<TabEvent>>,
    /// Replies produced by background plugin requests. They are written from
    /// the connection loop so the WebSocket sink has one owner.
    out_tx: Option<mpsc::UnboundedSender<ServerMsg>>,
}

enum Incoming {
    Socket(Option<Result<Message, axum::Error>>),
    Tab(Option<TabEvent>),
    Update(bool),
    Out(Option<ServerMsg>),
}

impl GuiSession {
    /// Connect upstream and load the stored sessions into tabs.
    pub async fn new(
        endpoint: &Endpoint,
        server_token: Option<&str>,
        version: &str,
        updates: watch::Receiver<Option<gewu_update::Update>>,
        downloads: Arc<DownloadTickets>,
    ) -> Result<Self> {
        let admin = endpoint
            .connect_with_token(server_token)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to the gewu server at {} (is `gewu-server serve` running?)",
                    endpoint.display()
                )
            })?;

        let (model_label, working_directory, workspaces) = match admin.info().await {
            // The status bar shows the model name without the provider.
            // An empty directory means the upstream predates `info.cwd`.
            Ok(info) => (
                Some(info.model.clone()),
                Some(info.working_directory.clone()).filter(|dir| !dir.is_empty()),
                info.workspaces,
            ),
            Err(e) => {
                debug!("could not read server info: {e}");
                (None, None, Vec::new())
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

        let update = updates.borrow().clone();
        Ok(Self {
            endpoint: endpoint.clone(),
            server_token: server_token.map(str::to_string),
            version: version.to_string(),
            update,
            updates,
            admin,
            workspace,
            conns: HashMap::new(),
            model_label,
            models,
            tools,
            working_directory,
            workspaces,
            closed_workspaces: BTreeSet::new(),
            expanded_dirs: BTreeSet::new(),
            file_cache,
            zoom: None,
            downloads,
            tab_tx: None,
            out_tx: None,
        })
    }

    /// Serve one browser WebSocket until it disconnects.
    pub async fn run(mut self, socket: WebSocket) -> Result<()> {
        let (mut sender, mut receiver) = socket.split();
        let (tab_tx, mut tab_rx) = mpsc::unbounded_channel::<TabEvent>();
        // Keep a sender alive so `tab_rx.recv()` only yields real events.
        let _tab_keepalive = tab_tx.clone();
        self.tab_tx = Some(tab_tx.clone());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
        self.out_tx = Some(out_tx);

        match self.admin.ping().await {
            Ok(()) => self.note_active("connected"),
            Err(e) => self.fail_active(format!("server not responding: {e}")),
        }

        let ids: Vec<_> = self
            .workspace
            .tabs
            .iter()
            .filter_map(|tab| tab.model.session_id.clone().map(|s| (tab.id, s)))
            .collect();
        for (id, session_id) in ids {
            // One bad stored session must not prevent the browser session
            // from starting; log and continue with the remaining tabs.
            if let Err(e) = self.ensure_client(id).await {
                warn!("failed to restore connection for tab {id}: {e}");
                continue;
            }
            if let Some(client) = self.client(id)
                && let Ok(info) = client.info_for_session(&session_id).await
            {
                for workspace in info.workspaces {
                    if !self.workspaces.iter().any(|root| root.id == workspace.id) {
                        self.workspaces.push(workspace);
                    }
                }
                if let Some(tab) = self.workspace.tab_mut(id) {
                    tab.workspace = info.active_workspace;
                }
            }
            if let Err(e) = self.observe(id, session_id, true).await {
                warn!("failed to restore observer for tab {id}: {e}");
            }
        }
        // Resolve the active root and visible file cache before the first
        // snapshot, so reconnect does not emit a second UI state sequence.
        self.refresh_tree_cache().await?;
        send_msg(&mut sender, &self.snapshot()).await?;

        // Refresh context usage for the focused session. This may create the
        // tab's upstream connection.
        self.activate_active(&mut sender).await?;

        let mut updates_live = true;
        loop {
            let incoming = tokio::select! {
                message = receiver.next() => Incoming::Socket(message),
                event = tab_rx.recv() => Incoming::Tab(event),
                reply = out_rx.recv() => Incoming::Out(reply),
                changed = self.updates.changed(), if updates_live => Incoming::Update(changed.is_ok()),
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
                Incoming::Out(Some(message)) => {
                    send_msg(&mut sender, &message).await?;
                }
                Incoming::Out(None) => break,
                Incoming::Update(true) => {
                    self.update = self.updates.borrow().clone();
                    send_msg(
                        &mut sender,
                        &ServerMsg::SessionSidebar {
                            html: self.session_sidebar(),
                        },
                    )
                    .await?;
                }
                Incoming::Update(false) => updates_live = false,
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
        let root = self.workspace_root(&tab.workspace);
        ServerMsg::Snapshot {
            tabs: self.workspace.infos(),
            active,
            transcript: view::transcript_html(tab),
            status: view::status_html(tab, &self.models),
            sidebar: view::sidebar_tabs_with_workspaces_html(
                &self.tools,
                root.map(|workspace| workspace.path.as_str())
                    .or(self.working_directory.as_deref()),
                &self.workspaces,
                &tab.workspace,
                &self.expanded_dirs,
                &self.file_cache,
            ),
            session_sidebar: self.session_sidebar(),
        }
    }

    fn workspace_root(&self, id: &str) -> Option<&WorkspaceSummary> {
        self.workspaces.iter().find(|workspace| workspace.id == id)
    }

    fn visible_workspaces(&self) -> Vec<WorkspaceSummary> {
        self.workspaces
            .iter()
            .filter(|root| !self.closed_workspaces.contains(&root.id))
            .cloned()
            .collect()
    }

    fn active_root_path(&self) -> Option<String> {
        let tab = self.workspace.tab(self.workspace.active_id())?;
        self.workspace_root(&tab.workspace)
            .map(|workspace| workspace.path.clone())
            .or_else(|| self.working_directory.clone())
    }

    /// Session sidebar HTML, including any available release notice.
    fn session_sidebar(&self) -> String {
        let latest = self.update.as_ref().map(|update| update.latest.to_string());
        view::session_sidebar_html_with_update(
            &self.workspace.infos(),
            &self.visible_workspaces(),
            self.workspace.active_id(),
            &self.version,
            latest.as_deref(),
        )
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
        send_msg(
            sender,
            &ServerMsg::SessionSidebar {
                html: self.session_sidebar(),
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
            && block >= tab.transcript_start
            && let Some(first) = tool_run_start(&tab.model.blocks, block)
            && first >= tab.transcript_start
            && let Some(html) = view::tool_card_html(tab, first)
        {
            return send_msg(sender, &ServerMsg::ToolCard { first, html }).await;
        }
        self.send_transcript(sender).await
    }

    /// Send just a thought block's body, so toggling its reasoning does not
    /// re-render the whole transcript. Falls back to a full transcript when
    /// the block cannot be located.
    async fn send_thought_body(
        &self,
        tab_id: u64,
        block: usize,
        sender: &mut Sender,
    ) -> Result<()> {
        if let Some(tab) = self.workspace.tab(tab_id)
            && block >= tab.transcript_start
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
        // Keyed by the text: repeats of one failure update a single box
        // entry instead of stacking.
        let message: String = message.into();
        let key = format!("error:{message}");
        self.send_alert(sender, message, Some(key)).await
    }

    /// Push a message into the browser's floating alert box; `key` marks
    /// entries that update in place (e.g. retries of one failure) instead
    /// of stacking.
    async fn send_alert(
        &self,
        sender: &mut Sender,
        message: impl Into<String>,
        key: Option<String>,
    ) -> Result<()> {
        send_msg(
            sender,
            &ServerMsg::Error {
                message: message.into(),
                key,
            },
        )
        .await?;
        Ok(())
    }

    /// Push a provider retry wait into the floating alert box; the browser
    /// renders a live countdown and dismisses the entry at zero.
    async fn send_retry_wait(
        &self,
        sender: &mut Sender,
        key: String,
        prefix: String,
        attempt: u32,
        delay_secs: u64,
    ) -> Result<()> {
        send_msg(
            sender,
            &ServerMsg::RetryWait {
                key,
                prefix,
                attempt,
                delay_secs,
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
        self.refresh_tree_cache().await?;
        send_msg(
            sender,
            &ServerMsg::FileTree {
                html: view::file_tree_with_workspaces_html(
                    self.active_root_path().as_deref(),
                    &self.workspaces,
                    &self.workspace.active_tab().workspace,
                    &self.expanded_dirs,
                    &self.file_cache,
                ),
            },
        )
        .await?;
        Ok(())
    }

    async fn refresh_tree_cache(&mut self) -> Result<()> {
        if self
            .workspace
            .tab(self.workspace.active_id())
            .is_some_and(|tab| tab.model.session_id.is_none() && tab.workspace == "cwd")
        {
            self.file_cache.clear();
            return Ok(());
        }
        let client = self.file_client().await?;
        let paths: Vec<String> = std::iter::once(String::new())
            .chain(self.expanded_dirs.iter().cloned())
            .collect();
        for path in paths {
            if let Ok(entries) = client.list_files(path.clone()).await {
                self.file_cache.insert(path, entries);
            }
        }
        Ok(())
    }

    /// Use the active tab's session connection for file operations so its
    /// selected workspace, rather than the server default, is applied.
    async fn file_client(&mut self) -> Result<Client> {
        let tab_id = self.workspace.active_id();
        self.ensure_session(tab_id).await?;
        Ok(self.client(tab_id).expect("just ensured"))
    }

    fn note_active(&mut self, message: impl Into<String>) {
        self.workspace.active_tab().model.note(message);
    }

    fn fail_active(&mut self, message: impl Into<String>) {
        self.workspace.active_tab().model.fail(message);
    }

    /// The focused tab's own connection, created on first use and replaced
    /// when the upstream restarted underneath it.
    async fn ensure_client(&mut self, tab_id: u64) -> Result<(), ClientError> {
        if self
            .conns
            .get(&tab_id)
            .and_then(|conn| conn.client.as_ref())
            .is_some_and(|client| !client.is_closed())
        {
            return Ok(());
        }
        if let Some(conn) = self.conns.get_mut(&tab_id)
            && conn
                .client
                .as_ref()
                .is_some_and(|client| client.is_closed())
        {
            // The upstream restarted (or the socket dropped): the tab's
            // observer and ask-forwarding task died with the same
            // connection. Drop them so the terminal-frame path re-observes.
            conn.client = None;
            conn.observer = None;
            conn.observing = false;
            if let Some(task) = conn.task.take() {
                task.abort();
            }
            if let Some(tab) = self.workspace.tab_mut(tab_id) {
                tab.model.note("reconnected to server");
            }
        }
        let endpoint = self.endpoint.clone();
        let client = endpoint
            .connect_with_token(self.server_token.as_deref())
            .await?;
        self.conns.entry(tab_id).or_default().client = Some(client);
        Ok(())
    }

    /// The admin connection, reconnected when the upstream restarted since
    /// it was created. Workspace browsing and session deletion ride this
    /// connection, so it must heal across upstream restarts.
    async fn admin_client(&mut self) -> Result<Client, ClientError> {
        if self.admin.is_closed() {
            let endpoint = self.endpoint.clone();
            self.admin = endpoint
                .connect_with_token(self.server_token.as_deref())
                .await?;
            self.note_active("reconnected to server");
        }
        Ok(self.admin.clone())
    }

    fn client(&self, tab_id: u64) -> Option<Client> {
        self.conns
            .get(&tab_id)
            .and_then(|conn| conn.client.as_ref())
            .cloned()
    }

    async fn ensure_session(&mut self, tab_id: u64) -> Result<(), ClientError> {
        let tab = self
            .workspace
            .tab(tab_id)
            .ok_or_else(|| ClientError::Protocol(format!("unknown tab {tab_id}")))?;
        let session_id = tab.model.session_id.clone();
        let workspace = tab.workspace.clone();
        self.ensure_client(tab_id).await?;
        let client = self.client(tab_id).expect("just ensured");
        if let Some(session_id) = session_id {
            client.usage(Some(&session_id)).await?;
        } else {
            let (session_id, workspace) = client.switch_workspace(workspace).await?;
            if let Some(root) = self
                .workspaces
                .iter_mut()
                .find(|root| root.id == workspace.id)
            {
                *root = workspace.clone();
            } else {
                self.workspaces.push(workspace.clone());
            }
            let tab = self.workspace.tab_mut(tab_id).expect("known tab");
            tab.model.set_session(Some(session_id));
            tab.workspace = workspace.id;
        }
        Ok(())
    }

    async fn observe(&mut self, tab_id: u64, session_id: String, restore: bool) -> Result<()> {
        let observer = self
            .endpoint
            .connect_with_token(self.server_token.as_deref())
            .await?;
        let mut stream = observer.watch_session(session_id).await?;
        let tx = self.tab_tx.clone().expect("running event loop");
        let task = tokio::spawn(async move {
            let mut first = true;
            while let Some(frame) = stream.next_frame().await {
                // After our own turn finishes, the local model already has
                // every event. Do not replace it with an idle snapshot and
                // shift block indices by discarding connection-local notes.
                let skip = first
                    && !restore
                    && matches!(
                        &frame,
                        Ok(ServerMessage::SessionSnapshot { busy: false, .. })
                    );
                first = false;
                if skip {
                    continue;
                }
                if tx
                    .send(TabEvent {
                        tab_id,
                        frame: Some(frame),
                        source: TabSource::Observer,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let conn = self.conns.entry(tab_id).or_default();
        if let Some(previous) = conn.task.replace(task) {
            previous.abort();
        }
        if let Some(previous) = conn.observer.replace(observer) {
            previous.close().await;
        }
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
            ClientMsg::Plugin {
                tab,
                request,
                plugin,
            } => {
                use gewu_protocol::plugin::HostAction;
                if matches!(&plugin, HostAction::MarketplaceInstall { .. }) {
                    self.ensure_session(tab).await?;
                    let ws = self
                        .workspace
                        .tab(tab)
                        .expect("known tab")
                        .workspace
                        .clone();
                    let session_id = self
                        .workspace
                        .tab(tab)
                        .and_then(|tab| tab.model.session_id.clone());
                    let client = self.client(tab).expect("ensured");
                    let out = self
                        .out_tx
                        .clone()
                        .context("GUI connection has no reply channel")?;
                    tokio::spawn(async move {
                        let response = client.plugin(ws, plugin).await;
                        let (result, error) = match response {
                            Ok(result) => (Some(result), None),
                            Err(error) => (None, Some(error.to_string())),
                        };
                        let _ = out.send(ServerMsg::Plugin {
                            request,
                            result,
                            error,
                            session_id,
                        });
                    });
                    return Ok(());
                }
                let attaching = matches!(&plugin, HostAction::Attach { .. });
                let detaching = if let HostAction::Detach { view_id } = &plugin {
                    Some(view_id.clone())
                } else {
                    None
                };
                let response = async {
                    self.ensure_session(tab).await?;
                    let ws = self
                        .workspace
                        .tab(tab)
                        .expect("known tab")
                        .workspace
                        .clone();
                    self.client(tab).expect("ensured").plugin(ws, plugin).await
                }
                .await;
                if let Ok(result) = &response
                    && let Some(conn) = self.conns.get_mut(&tab)
                {
                    if attaching
                        && let Some(id) = result.get("viewId").and_then(serde_json::Value::as_str)
                    {
                        conn.plugin_views.push(id.into());
                    }
                    if let Some(id) = detaching {
                        conn.plugin_views.retain(|saved| saved != &id);
                    }
                }
                let session_id = self
                    .workspace
                    .tab(tab)
                    .and_then(|tab| tab.model.session_id.clone());
                let (result, error) = match response {
                    Ok(result) => (Some(result), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                send_msg(
                    sender,
                    &ServerMsg::Plugin {
                        request,
                        result,
                        error,
                        session_id,
                    },
                )
                .await?;
            }
            ClientMsg::RevertChat { tab, block, undo } => {
                self.close_zoom_for_tab(tab, sender).await?;
                let Some(tab_state) = self.workspace.tab(tab) else {
                    return self.send_error(sender, "unknown tab").await;
                };
                if tab_state.busy() {
                    return self
                        .send_error(sender, "wait for the current turn to finish")
                        .await;
                }
                let turn = if let Some(index) = block {
                    if !matches!(tab_state.model.blocks.get(index), Some(Block::User(_))) {
                        return self.send_error(sender, "not a user message").await;
                    }
                    Some(
                        tab_state.model.blocks[..index]
                            .iter()
                            .filter(|b| matches!(b, Block::User(_)))
                            .count(),
                    )
                } else {
                    None
                };
                let session_id = tab_state.model.session_id.clone();
                if session_id.is_none() {
                    return self.send_error(sender, "no conversation to revert").await;
                }
                if let Err(e) = self.ensure_session(tab).await {
                    return self.send_error(sender, e.to_string()).await;
                }
                let client = self.client(tab).expect("ensured");
                match client.revert_chat(turn, undo).await {
                    Ok((id, messages)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known");
                        tab_state.model.load_history(&messages);
                        tab_state.model.set_session(Some(id));
                        tab_state
                            .model
                            .note("Conversation updated. Workspace files were NOT undone.");
                        tab_state.reset_transcript_window();
                        tab_state.expanded_groups.clear();
                        tab_state.expanded_outputs.clear();
                        tab_state.context = None;
                        if self.workspace.active_id() == tab {
                            self.send_active_full(sender).await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::SwitchAgent { tab, agent } => {
                if !["build", "plan"].contains(&agent.as_str()) {
                    return self.send_error(sender, "unknown mode").await;
                }
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) {
                    return self
                        .send_error(sender, "wait for the current turn to finish")
                        .await;
                }
                if let Err(e) = self.ensure_session(tab).await {
                    return self.send_error(sender, e.to_string()).await;
                }
                let client = self.client(tab).expect("ensured");
                match client.switch_agent(agent.clone()).await {
                    Ok((id, tools)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.model.set_session(Some(id));
                        tab_state.agent = agent;
                        self.tools = Ok(tools);
                        if self.workspace.active_id() == tab {
                            self.send_status(sender).await?;
                            send_msg(
                                sender,
                                &ServerMsg::Tools {
                                    html: view::sidebar_tools_content_html(&self.tools),
                                },
                            )
                            .await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::SwitchModel { tab, model } => {
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) {
                    return self
                        .send_error(sender, "wait for the current turn to finish")
                        .await;
                }
                if let Err(e) = self.ensure_session(tab).await {
                    return self.send_error(sender, e.to_string()).await;
                }
                let client = self.client(tab).expect("ensured");
                match client.switch_model(model.clone()).await {
                    Ok((id, label, reasoning_effort)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.model.set_session(Some(id));
                        tab_state.model_selector = Some(model);
                        tab_state.model_label = Some(label);
                        tab_state.reasoning_effort = reasoning_effort;
                        if self.workspace.active_id() == tab {
                            self.send_status(sender).await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::SwitchReasoningEffort { tab, effort } => {
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) {
                    return self
                        .send_error(sender, "wait for the current turn to finish")
                        .await;
                }
                if let Err(e) = self.ensure_session(tab).await {
                    return self.send_error(sender, e.to_string()).await;
                }
                let client = self.client(tab).expect("ensured");
                match client.switch_reasoning_effort(effort.as_deref()).await {
                    Ok((id, effort)) => {
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.model.set_session(Some(id));
                        tab_state.reasoning_effort = effort;
                        if self.workspace.active_id() == tab {
                            self.send_status(sender).await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::SwitchWorkspace { tab, workspace } => {
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) {
                    return self
                        .send_error(sender, "wait for the current turn to finish")
                        .await;
                }
                if !self.workspaces.iter().any(|root| root.id == workspace) {
                    return self.send_error(sender, "unknown workspace").await;
                }
                if let Err(e) = self.ensure_session(tab).await {
                    return self.send_error(sender, e.to_string()).await;
                }
                if let Some(conn) = self.conns.get_mut(&tab) {
                    detach_views(conn).await;
                }
                let client = self.client(tab).expect("ensured");
                match client.switch_workspace(workspace.clone()).await {
                    Ok((session_id, workspace)) => {
                        self.closed_workspaces.remove(&workspace.id);
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.workspace = workspace.id;
                        tab_state.model.set_session(Some(session_id));
                        if self.workspace.active_id() == tab {
                            self.expanded_dirs.clear();
                            self.file_cache.clear();
                            self.send_active_full(sender).await?;
                            self.send_refreshed_tree(sender).await?;
                        }
                    }
                    Err(e) => self.send_error(sender, e.to_string()).await?,
                }
            }
            ClientMsg::BrowseWorkspace { path } => match self.admin_client().await {
                Ok(admin) => match admin.browse_workspace(path).await {
                    Ok((path, parent, entries)) => {
                        send_msg(
                            sender,
                            &ServerMsg::WorkspaceBrowser {
                                html: view::workspace_browser_html(
                                    &path,
                                    parent.as_deref(),
                                    &entries,
                                ),
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        self.send_error(sender, format!("could not browse directory: {e}"))
                            .await?
                    }
                },
                Err(e) => {
                    self.send_error(sender, format!("could not browse directory: {e}"))
                        .await?
                }
            },
            ClientMsg::BrowseNewSession { path } => match self.admin_client().await {
                Ok(admin) => match admin.browse_workspace(path).await {
                    Ok((path, parent, entries)) => {
                        send_msg(
                            sender,
                            &ServerMsg::WorkspacePicker {
                                html: view::new_session_browser_html(
                                    &path,
                                    parent.as_deref(),
                                    &entries,
                                ),
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        self.send_error(sender, format!("could not browse directory: {e}"))
                            .await?
                    }
                },
                Err(e) => {
                    self.send_error(sender, format!("could not browse directory: {e}"))
                        .await?
                }
            },
            ClientMsg::CreateTab { workspace, path } => {
                let known = self.workspaces.iter().any(|root| root.id == workspace)
                    || workspace.starts_with("runtime:");
                if !known || path.trim().is_empty() {
                    return self.send_error(sender, "invalid session workspace").await;
                }
                if !self.workspaces.iter().any(|root| root.id == workspace) {
                    self.workspaces.push(WorkspaceSummary {
                        id: workspace.clone(),
                        path,
                    });
                }
                self.closed_workspaces.remove(&workspace);
                let id = self.workspace.open_blank_in_workspace(workspace);
                self.workspace.tab_mut(id).expect("just added").model_label =
                    self.model_label.clone();
                self.expanded_dirs.clear();
                self.file_cache.clear();
                send_msg(sender, &ServerMsg::CloseWorkspaceBrowser).await?;
                self.send_active_full(sender).await?;
                self.send_refreshed_tree(sender).await?;
            }
            ClientMsg::AddWorkspace { tab, path } => {
                if self.workspace.tab(tab).is_none_or(|tab| tab.busy()) {
                    return self
                        .send_error(sender, "wait for the current turn to finish")
                        .await;
                }
                if let Err(e) = self.ensure_session(tab).await {
                    return self.send_error(sender, e.to_string()).await;
                }
                let client = self.client(tab).expect("ensured");
                match client.add_workspace(path).await {
                    Ok((session_id, workspace)) => {
                        self.closed_workspaces.remove(&workspace.id);
                        if !self.workspaces.iter().any(|root| root.id == workspace.id) {
                            self.workspaces.push(workspace.clone());
                        }
                        let tab_state = self.workspace.tab_mut(tab).expect("known tab");
                        tab_state.workspace = workspace.id;
                        tab_state.model.set_session(Some(session_id));
                        if self.workspace.active_id() == tab {
                            self.expanded_dirs.clear();
                            self.file_cache.clear();
                            send_msg(sender, &ServerMsg::CloseWorkspaceBrowser).await?;
                            self.send_active_full(sender).await?;
                            self.send_refreshed_tree(sender).await?;
                        }
                    }
                    Err(e) => {
                        self.send_error(sender, format!("could not add directory: {e}"))
                            .await?
                    }
                }
            }
            ClientMsg::CloseWorkspaceBrowser => {
                send_msg(sender, &ServerMsg::CloseWorkspaceBrowser).await?;
            }
            // Auth is consumed by the transport before the session starts.
            ClientMsg::Auth { .. } => {}
            ClientMsg::Ask {
                tab,
                prompt,
                attachments,
            } => {
                self.handle_ask(tab, prompt, attachments, sender, tab_tx)
                    .await?;
            }
            ClientMsg::AnswerQuestion {
                tab,
                request_id,
                answers,
            } => {
                let Some(client) = self.client(tab) else {
                    return self.send_error(sender, "unknown tab").await;
                };
                if let Err(e) = client.reply_question(&request_id, answers).await {
                    self.send_error(sender, format!("could not answer question: {e}"))
                        .await?;
                }
            }
            ClientMsg::RejectQuestion { tab, request_id } => {
                let Some(client) = self.client(tab) else {
                    return self.send_error(sender, "unknown tab").await;
                };
                if let Err(e) = client.reject_question(&request_id).await {
                    self.send_error(sender, format!("could not dismiss question: {e}"))
                        .await?;
                }
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
                let browse_path = self
                    .working_directory
                    .clone()
                    .or_else(|| {
                        self.workspaces
                            .first()
                            .map(|workspace| workspace.path.clone())
                    })
                    .unwrap_or_default();
                send_msg(
                    sender,
                    &ServerMsg::WorkspacePicker {
                        html: view::session_workspace_picker_html(&self.workspaces, &browse_path),
                    },
                )
                .await?;
            }
            ClientMsg::CloseTab { tab } => {
                self.close_zoom_for_tab(tab, sender).await?;
                self.close_tab(tab).await;
                self.send_active_full(sender).await?;
            }
            ClientMsg::CloseWorkspace { workspace } => {
                let active = self.workspace.active_id();
                let tabs = self.workspace.close_workspace(&workspace);
                let closed: Vec<u64> = tabs.iter().map(|tab| tab.id).collect();
                if self
                    .zoom
                    .as_ref()
                    .is_some_and(|zoom| closed.contains(&zoom.tab))
                {
                    self.close_zoom(sender).await?;
                }
                for tab in tabs {
                    self.stop_tab(tab.id).await;
                }
                self.closed_workspaces.insert(workspace);
                self.ensure_blank_tab();
                if self.workspace.active_id() != active {
                    self.expanded_dirs.clear();
                    self.file_cache.clear();
                    self.send_active_full(sender).await?;
                    self.send_refreshed_tree(sender).await?;
                    self.activate_active(sender).await?;
                } else {
                    self.send_tabs(sender).await?;
                }
            }
            ClientMsg::SwitchTab { tab } => {
                let previous = self.workspace.active_id();
                if self.workspace.switch_to_id(tab) {
                    if previous != tab {
                        self.close_zoom(sender).await?;
                    }
                    self.expanded_dirs.clear();
                    self.file_cache.clear();
                    self.send_active_full(sender).await?;
                    self.send_refreshed_tree(sender).await?;
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
                    // Workflow runs default to expanded, so their toggle
                    // collapses; other runs toggle expansion as before.
                    let run_len =
                        view::tool_run_len(&tab.model.blocks, block, tab.model.blocks.len());
                    let workflow = block < tab.model.blocks.len()
                        && crate::view::is_workflow_run(&tab.model.blocks[block..block + run_len]);
                    if workflow {
                        toggle(&mut tab.collapsed_groups, block);
                    } else {
                        toggle(&mut tab.expanded_groups, block);
                    }
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
            ClientMsg::ZoomWorkflow { tab, block } => {
                let Some(tab_state) = self.workspace.tab(tab) else {
                    return self.send_error(sender, "unknown tab").await;
                };
                let Some(run_id) =
                    crate::workflow::run_id_for_block(&tab_state.model.blocks, block)
                else {
                    return self
                        .send_error(sender, "this workflow run is not ready yet")
                        .await;
                };
                let Some(html) = crate::workflow::zoom_html(tab_state, &run_id) else {
                    return self
                        .send_error(sender, "could not render the workflow graph")
                        .await;
                };
                self.zoom = Some(ZoomTarget {
                    tab,
                    run_id: run_id.clone(),
                });
                send_msg(sender, &ServerMsg::WorkflowZoom { run_id, html }).await?;
            }
            ClientMsg::CloseWorkflowZoom => {
                self.zoom = None;
                send_msg(sender, &ServerMsg::CloseWorkflowZoom).await?;
            }
            ClientMsg::LoadEarlier { tab } => {
                let page = if self.workspace.active_id() == tab {
                    self.workspace
                        .tab_mut(tab)
                        .and_then(|tab_state| tab_state.load_earlier())
                } else {
                    None
                };
                let Some((start, end)) = page else {
                    // Already at the beginning (or the tab switched under the
                    // click): a full render re-syncs the browser.
                    return self.send_transcript(sender).await;
                };
                let tab_state = self.workspace.tab(tab).expect("page loaded from a tab");
                let html = view::transcript_prepend_html(tab_state, start, end);
                send_msg(sender, &ServerMsg::TranscriptPrepend { html }).await?;
            }
            ClientMsg::SetFollow { tab, follow } => {
                let evicted = self.workspace.tab_mut(tab).is_some_and(|tab_state| {
                    tab_state.follow = follow;
                    follow && tab_state.evict_overflow()
                });
                if evicted && self.workspace.active_id() == tab {
                    self.send_transcript(sender).await?;
                }
            }
            ClientMsg::Refresh => {
                self.send_active_full(sender).await?;
            }
            ClientMsg::ToggleDir { path } => {
                let client = self.file_client().await?;
                if self.expanded_dirs.contains(&path) {
                    self.expanded_dirs.remove(&path);
                } else {
                    self.expanded_dirs.insert(path.clone());
                    if !self.file_cache.contains_key(&path) {
                        // A failed listing must not tear down the whole
                        // browser session; report it and keep the tree open.
                        match client.list_files(path.clone()).await {
                            Ok(entries) => {
                                self.file_cache.insert(path.clone(), entries);
                            }
                            Err(e) => {
                                self.expanded_dirs.remove(&path);
                                self.send_error(sender, format!("failed to list '{}': {e}", path))
                                    .await?;
                            }
                        }
                    }
                }
                send_msg(
                    sender,
                    &ServerMsg::FileTree {
                        html: view::file_tree_with_workspaces_html(
                            self.active_root_path().as_deref(),
                            &self.workspaces,
                            &self.workspace.active_tab().workspace,
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
                let client = self.file_client().await?;
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
                    match client.write_file(name.clone(), &bytes).await {
                        Ok((_, bytes_written)) => {
                            uploaded.push(format!("{name} ({} kB)", bytes_written / 1024))
                        }
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
            ClientMsg::AttachFiles { files } => {
                let client = self.file_client().await?;
                let mut stored = Vec::new();
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
                    match client.upload_attachment(name.clone(), &bytes).await {
                        Ok((_, _path)) => {
                            stored.push(format!("{name} ({} kB)", bytes.len() / 1024))
                        }
                        Err(e) => failures.push(format!("could not attach '{name}': {e}")),
                    }
                }
                if !stored.is_empty() {
                    self.send_notice(sender, format!("Attached {}", stored.join(", ")))
                        .await?;
                }
                if !failures.is_empty() {
                    self.send_error(sender, failures.join("; ")).await?;
                }
            }
            ClientMsg::ReadFile { path } => {
                match self.file_client().await?.read_file(path.clone()).await {
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
                }
            }
            ClientMsg::Download { path } => {
                let tab_id = self.workspace.active_id();
                self.file_client().await?;
                let session_id = self
                    .workspace
                    .tab(tab_id)
                    .and_then(|tab| tab.model.session_id.clone());
                match session_id {
                    Some(session_id) => {
                        let id = self.downloads.mint(session_id, path);
                        send_msg(
                            sender,
                            &ServerMsg::DownloadReady {
                                url: format!("/download/{id}"),
                            },
                        )
                        .await?
                    }
                    None => {
                        self.send_error(sender, "no active session to download from")
                            .await?
                    }
                }
            }
            ClientMsg::PreviewFile { path } => {
                if !is_html_path(&path) {
                    self.send_error(sender, format!("'{path}' is not an HTML file"))
                        .await?
                } else {
                    let tab_id = self.workspace.active_id();
                    self.file_client().await?;
                    let session_id = self
                        .workspace
                        .tab(tab_id)
                        .and_then(|tab| tab.model.session_id.clone());
                    match session_id {
                        Some(session_id) => {
                            let id = self.downloads.mint_preview(session_id, path);
                            send_msg(
                                sender,
                                &ServerMsg::PreviewReady {
                                    url: format!("/preview/{id}"),
                                },
                            )
                            .await?
                        }
                        None => {
                            self.send_error(sender, "no active session to preview from")
                                .await?
                        }
                    }
                }
            }
            ClientMsg::ClosePreview => send_msg(sender, &ServerMsg::ClosePreview).await?,
        }
        Ok(())
    }

    async fn handle_ask(
        &mut self,
        tab_id: u64,
        prompt: String,
        attachments: Vec<gewu_protocol::AskAttachment>,
        sender: &mut Sender,
        tab_tx: &mpsc::UnboundedSender<TabEvent>,
    ) -> Result<()> {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() && attachments.is_empty() {
            return self.send_error(sender, "prompt is empty").await;
        }
        let Some(tab) = self.workspace.tab(tab_id) else {
            return self
                .send_error(sender, format!("unknown tab {tab_id}"))
                .await;
        };
        // While a turn runs, a submit steers the running agent instead of
        // starting a new ask: the message is injected at the agent's next
        // round boundary. It is rendered immediately at the transcript's
        // end — the position where it will be ingested, since every event
        // the running turn can still emit (deltas, tool results) updates
        // existing blocks in place. The pending entry lets the later
        // `steered` event confirm the block instead of duplicating it.
        if tab.model.phase != Phase::Idle {
            if !attachments.is_empty() {
                return self
                    .send_error(
                        sender,
                        "attachments are only supported on a new message; wait for the current turn to finish",
                    )
                    .await;
            }
            if let Err(e) = self.ensure_client(tab_id).await {
                return self
                    .send_error(sender, format!("could not connect: {e}"))
                    .await;
            }
            let client = self.client(tab_id).expect("just ensured");
            return match client.steer(prompt.clone()).await {
                Ok(()) => {
                    let (index, collapsed) = {
                        let tab = self.workspace.tab_mut(tab_id).expect("tab exists");
                        let index = tab
                            .model
                            .apply_event(&AgentEvent::Steered {
                                text: prompt.clone(),
                            })
                            .expect("a steered event appends a user block");
                        tab.pending_steers.push((prompt, index));
                        (index, tab.model.take_collapsed())
                    };
                    if self.workspace.active_id() == tab_id {
                        if let Some(collapsed) = collapsed {
                            self.send_thought_body(tab_id, collapsed, sender).await?;
                        }
                        let html = self
                            .workspace
                            .tab(tab_id)
                            .and_then(|tab| view::block_html(tab, index))
                            .expect("the appended block renders");
                        send_msg(sender, &ServerMsg::BlockAppend { index, html }).await?;
                    }
                    Ok(())
                }
                Err(e) => {
                    self.send_error(sender, format!("could not steer: {e}"))
                        .await
                }
            };
        }

        if let Err(e) = self.ensure_session(tab_id).await {
            self.workspace
                .tab_mut(tab_id)
                .expect("tab exists")
                .model
                .fail(format!("could not connect: {e}"));
            self.send_error(sender, format!("could not connect: {e}"))
                .await?;
            return self.send_after_tab_change(tab_id, sender).await;
        }

        let session_id = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.model.session_id.clone());
        let client = self.client(tab_id).expect("just ensured");
        let workspace = self.workspace.tab(tab_id).map(|tab| tab.workspace.clone());
        let mut builder = client.ask(prompt.clone());
        if !attachments.is_empty() {
            builder = builder.attachments(attachments.clone());
        }
        if let Some(session_id) = session_id {
            builder = builder.session_id(session_id);
        }
        if let Some(workspace) = workspace {
            builder = builder.workspace(workspace);
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
                    tab.model
                        .submit_with_media(prompt, local_media_parts(&attachments));
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

    /// Dismiss the zoom panel if it belongs to `tab`.
    async fn close_zoom_for_tab(&mut self, tab: u64, sender: &mut Sender) -> Result<()> {
        if self.zoom.as_ref().is_some_and(|zoom| zoom.tab == tab) {
            self.zoom = None;
            send_msg(sender, &ServerMsg::CloseWorkflowZoom).await?;
        }
        Ok(())
    }

    /// Dismiss the zoom panel unconditionally.
    async fn close_zoom(&mut self, sender: &mut Sender) -> Result<()> {
        if self.zoom.take().is_some() {
            send_msg(sender, &ServerMsg::CloseWorkflowZoom).await?;
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
        self.stop_tab(tab_id).await;

        let session_id = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.model.session_id.clone());
        if let Some(session_id) = session_id {
            let deleted = match self.admin_client().await {
                Ok(admin) => admin.delete_session(&session_id).await,
                Err(e) => Err(e),
            };
            if let Err(e) = deleted {
                if let Some(tab) = self.workspace.tab_mut(tab_id) {
                    tab.model.note(format!("could not delete session: {e}"));
                }
                return;
            }
        }

        self.workspace.close(tab_id);
        self.ensure_blank_tab();
    }

    fn ensure_blank_tab(&mut self) {
        if self.workspace.is_empty() {
            self.closed_workspaces.remove("cwd");
            let id = self.workspace.open_blank();
            self.workspace.tab_mut(id).expect("just added").model_label = self.model_label.clone();
        }
    }

    async fn stop_tab(&mut self, tab_id: u64) {
        if let Some(mut conn) = self.conns.remove(&tab_id) {
            detach_views(&mut conn).await;
            if let Some(task) = conn.task.take() {
                if let Some(client) = &conn.client {
                    let _ = client.interrupt().await;
                }
                if conn.observing {
                    task.abort();
                }
                let _ = task.await;
            }
            if let Some(client) = conn.client.take() {
                client.close().await;
            }
            if let Some(observer) = conn.observer.take() {
                observer.close().await;
            }
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
        // A transient status (LLM retry backoff, compaction) arrived: only
        // the status line re-renders, not the transcript.
        let mut status_updated = false;
        // A question form opened or settled: the sidebar's waiting pulse and
        // per-tab state changed.
        let mut sidebar_updated = false;
        // An error to surface in the floating alert box, collected while
        // the tab is borrowed and sent once it is released. The optional
        // key lets the browser update an existing entry (repeats of one
        // failure) instead of stacking.
        let mut alert: Option<(String, Option<String>)> = None;
        // A provider retry wait with a live countdown (key, prefix, attempt,
        // delay), collected like the error above.
        let mut retry_wait: Option<(String, String, u32, u64)> = None;
        // Index of a thought block that an applied event collapsed (live
        // "Thinking…" settled to "Thought Ns"); the triggering event usually
        // appends a different block, so the settled body needs its own swap.
        let mut collapsed = None;
        // Run id to refresh in the zoom panel after this event.
        let mut zoom_run = None;
        // Decide the throttle before borrowing the tab below.
        let stream_due = self.stream_update_due(event.tab_id);

        {
            let Some(tab) = self.workspace.tab_mut(event.tab_id) else {
                return Ok(()); // tab closed mid-ask; drop late frames
            };
            let before = tab.model.blocks.len();

            match event.frame {
                Some(Ok(ServerMessage::SessionSnapshot {
                    session_id,
                    messages,
                    frames,
                    busy,
                    model: snapshot_model,
                    reasoning_effort: snapshot_effort,
                    ..
                })) => {
                    let mut model = gewu_client::SessionModel::new();
                    model.set_session(Some(session_id));
                    for message in &messages {
                        model.apply_message(message);
                    }
                    let mut workflows_changed = false;
                    for frame in frames {
                        match frame {
                            ServerMessage::Event { event, .. } => {
                                if let Ok(event) = serde_json::from_value::<AgentEvent>(event) {
                                    model.apply_event(&event);
                                }
                            }
                            ServerMessage::WorkflowProgress { run, .. } => {
                                tab.workflows.insert(
                                    run.run_id.clone(),
                                    crate::workflow::WfSnapshot::from_run(&run),
                                );
                                workflows_changed = true;
                            }
                            _ => {}
                        }
                    }
                    model.phase = if busy { Phase::Busy } else { Phase::Idle };
                    let changed =
                        tab.model.blocks != model.blocks || tab.model.phase != model.phase;
                    tab.model = model;
                    if changed {
                        tab.reset_transcript_window();
                    }
                    // The snapshot is authoritative: an ingested steer is
                    // already in its frames/messages, and one still queued
                    // upstream re-arrives as a live `steered` event.
                    tab.pending_steers.clear();
                    if let Some(selector) = snapshot_model {
                        tab.model_selector = Some(selector.clone());
                        tab.model_label =
                            Some(label_for(&self.models, &selector).unwrap_or(selector));
                    }
                    tab.reasoning_effort = snapshot_effort;
                    terminal = !busy;
                    update = if changed || workflows_changed {
                        TabUpdate::Full
                    } else {
                        TabUpdate::None
                    };
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
                            // The transcript block is the durable record; the
                            // floating alert box makes the failure visible,
                            // keyed by the text so repeats of one failure
                            // update a single entry.
                            alert = Some((error.clone(), Some(format!("error:{error}"))));
                            let affected = tab.model.apply_event(&AgentEvent::Error { error });
                            collapsed = tab.model.take_collapsed();
                            update = affected.map_or(TabUpdate::Full, TabUpdate::BlockAppend);
                        }
                        Ok(AgentEvent::Steered { text }) => {
                            // The message was already rendered when its
                            // submit was routed to `steer`; this event only
                            // confirms the agent ingested it.
                            let (steer_update, steer_collapsed) =
                                confirm_steered(&mut tab.model, &mut tab.pending_steers, text);
                            update = steer_update;
                            collapsed = steer_collapsed;
                        }
                        Ok(AgentEvent::Status { message }) => {
                            // Transient busy-indicator text (context
                            // compaction): the transcript is unchanged, so
                            // only the status line is pushed. Retry waits
                            // arrive as typed `Retrying` events instead.
                            tab.model.apply_event(&AgentEvent::Status { message });
                            status_updated = true;
                        }
                        Ok(AgentEvent::Retrying {
                            kind,
                            message,
                            attempt,
                            delay_secs,
                        }) => {
                            // The failure surfaces in the floating alert
                            // box, keyed by the failure's stable kind (not
                            // the volatile body text) so retries of one
                            // failure update a single entry instead of
                            // stacking. The browser counts the wait down
                            // and dismisses the entry at zero; the bottom
                            // bar keeps its plain busy label during the
                            // wait.
                            retry_wait = Some((
                                format!("retry:{kind}"),
                                AgentEvent::retry_prefix(&message),
                                attempt,
                                delay_secs,
                            ));
                        }
                        Ok(agent_event) => {
                            // A pending `question` form flips the status line
                            // to its waiting phase and the sidebar pulse to
                            // amber, so form events need those pushes in
                            // addition to the card update.
                            if matches!(
                                agent_event,
                                AgentEvent::QuestionAsked { .. }
                                    | AgentEvent::QuestionResolved { .. }
                            ) {
                                status_updated = true;
                                sidebar_updated = true;
                            }
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
                Some(Ok(ServerMessage::WorkflowProgress { run, .. })) => {
                    // Live workflow state: re-render the start card's whole
                    // group with the new node colors. If the start card's
                    // output is not in yet, the later ToolFinish re-renders
                    // it with this stored state.
                    let target = crate::workflow::run_update_target(&tab.model.blocks, &run.run_id);
                    tab.workflows.insert(
                        run.run_id.clone(),
                        crate::workflow::WfSnapshot::from_run(&run),
                    );
                    if self
                        .zoom
                        .as_ref()
                        .is_some_and(|zoom| zoom.tab == event.tab_id && zoom.run_id == run.run_id)
                    {
                        zoom_run = Some(run.run_id.clone());
                    }
                    if let Some(first) = target {
                        update = TabUpdate::ToolCard(first);
                    }
                }
                Some(Ok(ServerMessage::Result { session_id, .. })) => {
                    tab.model.set_session(Some(session_id));
                    terminal = true;
                    update = TabUpdate::Full;
                }
                Some(Ok(ServerMessage::Error { error, .. })) => {
                    alert = Some((error.clone(), Some(format!("error:{error}"))));
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
                Some(Err(_e)) if event.source == TabSource::Observer => {
                    // The background watch died — e.g. the upstream restarted
                    // while the tab was idle. Nothing visible was lost: the
                    // next client use reconnects, and the next terminal ask
                    // re-observes. Surface it quietly instead of failing the
                    // tab.
                    if let Some(conn) = self.conns.get_mut(&event.tab_id) {
                        conn.observing = false;
                        conn.observer = None;
                    }
                    tab.model
                        .note("connection lost; will reconnect on next use");
                    update = TabUpdate::Full;
                }
                Some(Err(e)) => {
                    let text = e.to_string();
                    alert = Some((text.clone(), Some(format!("error:{text}"))));
                    tab.model.fail(text);
                    terminal = true;
                    update = TabUpdate::Full;
                }
            }

            if terminal {
                tab.model.finish_ask();
                // A steer queued upstream but dropped before ingestion (turn
                // ended first) must not suppress a future identical steer.
                tab.pending_steers.clear();
            }
            if tab.follow && tab.evict_overflow() && event.tab_id == active_id {
                update = TabUpdate::Full;
            }
        }

        if let Some(run_id) = zoom_run {
            let html = self
                .workspace
                .tab(event.tab_id)
                .and_then(|tab| crate::workflow::zoom_html(tab, &run_id));
            match html {
                Some(html) => send_msg(sender, &ServerMsg::WorkflowZoom { run_id, html }).await?,
                None => {
                    self.zoom = None;
                    send_msg(sender, &ServerMsg::CloseWorkflowZoom).await?;
                }
            }
        }

        if terminal && let Some(conn) = self.conns.get_mut(&event.tab_id) {
            if !conn.observing {
                conn.task = None;
            }
            conn.last_stream_render = None;
        }

        // Push the collected error into the floating alert box.
        if let Some((message, key)) = alert {
            self.send_alert(sender, message, key).await?;
        }
        if let Some((key, prefix, attempt, delay_secs)) = retry_wait {
            self.send_retry_wait(sender, key, prefix, attempt, delay_secs)
                .await?;
        }

        // Correct any drift with the authoritative server-side usage before
        // the terminal status is rendered.
        if terminal {
            self.refresh_usage(event.tab_id).await;
            let session_id = self
                .workspace
                .tab(event.tab_id)
                .and_then(|tab| tab.model.session_id.clone());
            if !self
                .conns
                .get(&event.tab_id)
                .is_some_and(|conn| conn.observing)
                && let Some(session_id) = session_id
            {
                self.observe(event.tab_id, session_id, false).await?;
            }
            if active_id == event.tab_id && !self.file_cache.contains_key("") {
                self.send_refreshed_tree(sender).await?;
            }
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
            TabUpdate::BlockReplace(index) if active => {
                let html = self
                    .workspace
                    .tab(event.tab_id)
                    .and_then(|tab| view::block_html(tab, index));
                match html {
                    Some(html) => {
                        send_msg(sender, &ServerMsg::BlockReplace { index, html }).await?;
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
        // Usage and status events leave `update` untouched, so they need
        // their own status push.
        if (usage_updated || status_updated) && active {
            self.send_status(sender).await?;
        }
        if sidebar_updated {
            send_msg(
                sender,
                &ServerMsg::SessionSidebar {
                    html: self.session_sidebar(),
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        for (_, mut conn) in self.conns.drain() {
            detach_views(&mut conn).await;
            if let Some(task) = conn.task.take() {
                task.abort();
            }
            if let Some(client) = conn.client.take() {
                client.close().await;
            }
            if let Some(observer) = conn.observer.take() {
                observer.close().await;
            }
        }
        self.admin.close().await;
    }
}

async fn detach_views(conn: &mut Conn) {
    if let Some(client) = &conn.client {
        for view_id in conn.plugin_views.drain(..) {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                client.plugin(
                    "cwd".into(),
                    gewu_protocol::plugin::HostAction::Detach { view_id },
                ),
            )
            .await;
        }
    }
}

/// Start of the run of consecutive tool blocks containing `block`, or `None`
/// when `block` is not a tool block.
fn tool_run_start(blocks: &[Block], block: usize) -> Option<usize> {
    if !matches!(blocks.get(block), Some(Block::Tool { .. })) {
        return None;
    }
    let mut start = block;
    while start > 0 && view::same_tool_run(&blocks[start - 1], &blocks[start]) {
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

/// Map an ask's inline attachments to the media parts the local preview
/// block renders. The server performs the authoritative resolution (path
/// references, size caps); this local mapping mirrors it for inline data so
/// the initiating connection shows thumbnails immediately. Non-media
/// attachments have no local preview.
fn local_media_parts(
    attachments: &[gewu_protocol::AskAttachment],
) -> Vec<gewu_protocol::ContentPart> {
    attachments
        .iter()
        .filter_map(|attachment| {
            let data = attachment.data.as_ref()?;
            match attachment.mime_type.as_str() {
                "application/pdf" => Some(gewu_protocol::ContentPart::File {
                    data: data.clone(),
                    mime_type: attachment.mime_type.clone(),
                }),
                mime if mime.starts_with("image/") => Some(gewu_protocol::ContentPart::Image {
                    data: data.clone(),
                    mime_type: mime.to_string(),
                }),
                _ => None,
            }
        })
        .collect()
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
            let terminal = frame.as_ref().is_ok_and(|message| {
                message.is_ask_terminal().is_some()
                    || matches!(message, ServerMessage::SessionSnapshot { busy: false, .. })
            }) || frame.is_err();
            if tx
                .send(TabEvent {
                    tab_id,
                    frame: Some(frame),
                    source: TabSource::Ask,
                })
                .is_err()
            {
                return; // event loop gone
            }
            if terminal {
                return;
            }
        }
        let _ = tx.send(TabEvent {
            tab_id,
            frame: None,
            source: TabSource::Ask,
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

/// Whether a workspace path names an HTML file the preview route may serve.
fn is_html_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_html_paths_are_previewable() {
        assert!(is_html_path("docs/a.html"));
        assert!(is_html_path("A.HTM"));
        assert!(is_html_path("index.HTML"));
        assert!(!is_html_path("src/main.rs"));
        assert!(!is_html_path("docs/a.html.bak"));
        assert!(!is_html_path("html"));
    }

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
        assert_eq!(
            sanitize_upload_name("notes.bin").as_deref(),
            Some("notes.bin")
        );
        // Only the final component survives traversal attempts.
        assert_eq!(
            sanitize_upload_name("../evil.txt").as_deref(),
            Some("evil.txt")
        );
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
            question: None,
            tool: tool.into(),
            input: String::new(),
            output: output.map(str::to_string),
            images: Vec::new(),
            status: None,
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
    fn tool_update_treats_skill_cards_as_standalone() {
        // A skill next to another tool gets its own card.
        let blocks = vec![tool("bash", Some("out")), tool("skill", None)];
        assert_eq!(tool_run_start(&blocks, 1), Some(1));
        assert!(matches!(
            tool_update(&blocks, Some(1), false),
            TabUpdate::ToolCard(1)
        ));

        // A tool after a skill starts a fresh card.
        let blocks = vec![tool("skill", Some("loaded")), tool("bash", None)];
        assert_eq!(tool_run_start(&blocks, 1), Some(1));
        assert!(matches!(
            tool_update(&blocks, Some(1), true),
            TabUpdate::ToolCardStart(1)
        ));

        // Two skills never share a card.
        let blocks = vec![tool("skill", None), tool("skill", Some("loaded"))];
        assert_eq!(tool_run_start(&blocks, 1), Some(1));
    }

    #[test]
    fn tool_update_resolves_the_card_behind_a_steered_user_block() {
        // A steer split the run: the finished tool sits before the user
        // block, so the affected index must still locate its own card.
        let blocks = vec![
            Block::User("hi".into()),
            tool("bash", Some("out")),
            Block::User("focus on tests".into()),
        ];
        assert!(matches!(
            tool_update(&blocks, Some(1), false),
            TabUpdate::ToolCard(1)
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

    #[test]
    fn steers_in_place_requires_a_contiguous_tail() {
        assert!(steers_in_place(&[("a".into(), 2)], 3));
        assert!(steers_in_place(&[("a".into(), 2), ("b".into(), 3)], 4));
        assert!(steers_in_place(&[], 3));
        // A non-steer block after the group breaks the tail.
        assert!(!steers_in_place(&[("a".into(), 2)], 4));
        // A gap between steers breaks contiguity.
        assert!(!steers_in_place(&[("a".into(), 2), ("b".into(), 4)], 5));
    }

    fn user_texts(model: &gewu_client::SessionModel) -> Vec<String> {
        model
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::User(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn confirm_steered_drops_pending_when_already_in_place() {
        let mut model = gewu_client::SessionModel::new();
        model.submit("hi".into());
        model.apply_event(&AgentEvent::ContentDelta {
            delta: "working".into(),
        });
        let index = model
            .apply_event(&AgentEvent::Steered {
                text: "focus".into(),
            })
            .expect("steered appends a block");
        let mut pending = vec![("focus".to_string(), index)];

        let (update, collapsed) = confirm_steered(&mut model, &mut pending, "focus".into());

        assert!(matches!(update, TabUpdate::BlockReplace(2)));
        assert_eq!(collapsed, None);
        assert!(pending.is_empty());
        assert_eq!(user_texts(&model), ["hi", "focus"]);
        assert_eq!(model.blocks.len(), 3);
    }

    #[test]
    fn confirm_steered_relocates_group_after_race_blocks() {
        // The steers were displayed before the ask's first block arrived.
        let mut model = gewu_client::SessionModel::new();
        model.submit("hi".into());
        let a = model
            .apply_event(&AgentEvent::Steered { text: "a".into() })
            .expect("steered appends a block");
        let b = model
            .apply_event(&AgentEvent::Steered { text: "b".into() })
            .expect("steered appends a block");
        model.apply_event(&AgentEvent::ContentDelta {
            delta: "working".into(),
        });
        let mut pending = vec![("a".to_string(), a), ("b".to_string(), b)];

        let (update, _) = confirm_steered(&mut model, &mut pending, "a".into());

        // The group moved below the late assistant block; `b` stays pending
        // at its new index.
        assert!(matches!(update, TabUpdate::Full));
        assert_eq!(pending, vec![("b".to_string(), 3)]);

        let (update, _) = confirm_steered(&mut model, &mut pending, "b".into());
        assert!(matches!(update, TabUpdate::BlockReplace(3)));
        assert_eq!(user_texts(&model), ["hi", "a", "b"]);
        assert!(matches!(model.blocks[1], Block::Assistant(_)));
    }

    #[test]
    fn confirm_steered_appends_when_not_tracked_locally() {
        let mut model = gewu_client::SessionModel::new();
        model.submit("hi".into());
        let mut pending = Vec::new();

        let (update, _) = confirm_steered(&mut model, &mut pending, "from elsewhere".into());

        assert!(matches!(update, TabUpdate::BlockAppend(1)));
        assert_eq!(user_texts(&model), ["hi", "from elsewhere"]);
    }
}
