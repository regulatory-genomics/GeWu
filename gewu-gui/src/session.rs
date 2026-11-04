//! One browser connection's workspace and upstream event loop.
//!
//! Each browser gets an isolated [`Workspace`] and its own lazily-created
//! `gewu_client::Client` per tab (the server pins one session per connection).
//! The control flow mirrors `gewu-tui/src/main.rs`, but the output is JSON
//! messages carrying rendered HTML instead of ratatui frames.

use std::collections::HashMap;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use gewu_client::session::Phase;
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
    task: Option<JoinHandle<()>>,
}

/// A frame forwarded from a tab's ask stream; `None` marks the stream's end.
struct TabEvent {
    tab_id: u64,
    frame: Option<gewu_client::Result<ServerMessage>>,
}

/// One browser's isolated GUI session.
pub struct GuiSession {
    endpoint: Endpoint,
    admin: Client,
    workspace: Workspace,
    conns: HashMap<u64, Conn>,
    model_label: Option<String>,
}

enum Incoming {
    Socket(Option<Result<Message, axum::Error>>),
    Tab(Option<TabEvent>),
}

impl GuiSession {
    /// Connect upstream and load the stored sessions into tabs.
    pub async fn new(endpoint: &Endpoint) -> Result<Self> {
        let admin = endpoint.connect().await.with_context(|| {
            format!(
                "failed to connect to the gewu server at {} (is `gewu-server serve` running?)",
                endpoint.display()
            )
        })?;

        let model_label = match admin.info().await {
            Ok(info) => Some(format!("{}/{}", info.provider, info.model)),
            Err(e) => {
                debug!("could not read server info: {e}");
                None
            }
        };

        let mut workspace = Workspace::new();

        // `list_sessions` is newest-first; opening in reverse puts the oldest
        // tab on the left and focuses the newest, like gewu-tui.
        match admin.list_sessions().await {
            Ok(sessions) => {
                for summary in sessions.into_iter().rev() {
                    match admin.get_session(&summary.session_id).await {
                        Ok(messages) => {
                            workspace.open_session(&summary.session_id, &messages);
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
            admin,
            workspace,
            conns: HashMap::new(),
            model_label,
        })
    }

    /// Serve one browser WebSocket until it disconnects.
    pub async fn run(mut self, socket: WebSocket) -> Result<()> {
        let (mut sender, mut receiver) = socket.split();
        let (tab_tx, mut tab_rx) = mpsc::unbounded_channel::<TabEvent>();
        // Keep a sender alive so `tab_rx.recv()` only yields real events.
        let _tab_keepalive = tab_tx.clone();

        match self.admin.ping().await {
            Ok(()) => self.note_active("connected"),
            Err(e) => self.fail_active(format!("server not responding: {e}")),
        }

        send_msg(&mut sender, &self.snapshot()).await?;

        // Refresh context usage for the focused session. This may create the
        // tab's upstream connection.
        self.activate_active(&mut sender).await?;

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
                    self.handle_tab_event(event, &mut sender).await?;
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
            status: view::status_html(tab),
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
                html: view::status_html(tab),
            },
        )
        .await?;
        Ok(())
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
        let client = endpoint.connect().await?;
        self.conns.entry(tab_id).or_default().client = Some(client);
        Ok(())
    }

    fn client(&self, tab_id: u64) -> Option<Client> {
        self.conns
            .get(&tab_id)
            .and_then(|conn| conn.client.as_ref())
            .cloned()
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
                self.workspace.tab_mut(tab_id).expect("active tab").context =
                    Some((usage.used_tokens, usage.context_limit));
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
            // Auth is consumed by the transport before the session starts.
            ClientMsg::Auth { .. } => {}
            ClientMsg::Ask { tab, prompt } => {
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
                    self.send_transcript(sender).await?;
                }
            }
            ClientMsg::ToggleGroup { tab, block } => {
                if let Some(tab) = self.workspace.tab_mut(tab) {
                    toggle(&mut tab.expanded_groups, block);
                }
                if self.workspace.active_id() == tab {
                    self.send_transcript(sender).await?;
                }
            }
            ClientMsg::Refresh => {
                self.send_active_full(sender).await?;
            }
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
                self.workspace
                    .tab_mut(tab_id)
                    .expect("tab exists")
                    .model
                    .submit(prompt);
                let task = spawn_ask_forwarder(tab_id, stream, tab_tx.clone());
                if let Some(conn) = self.conns.get_mut(&tab_id) {
                    conn.task = Some(task);
                }
            }
            Err(e) => {
                self.workspace
                    .tab_mut(tab_id)
                    .expect("tab exists")
                    .model
                    .fail(format!("could not send: {e}"));
            }
        }
        self.send_after_tab_change(tab_id, sender).await
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
                let _ = task.await;
            }
            if let Some(client) = conn.client.take() {
                client.close().await;
            }
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

    async fn handle_tab_event(&mut self, event: TabEvent, sender: &mut Sender) -> Result<()> {
        let active_id = self.workspace.active_id();
        let mut structural = false;
        let mut delta: Option<(usize, String)> = None;
        let mut terminal = false;

        {
            let Some(tab) = self.workspace.tab_mut(event.tab_id) else {
                return Ok(()); // tab closed mid-ask; drop late frames
            };
            let before = tab.model.blocks.len();

            match event.frame {
                None => {
                    terminal = true;
                    structural = true;
                }
                Some(Ok(ServerMessage::Event { event, .. })) => {
                    match serde_json::from_value::<AgentEvent>(event) {
                        Ok(AgentEvent::ContentDelta { delta: text }) => {
                            tab.model.apply_event(&AgentEvent::ContentDelta {
                                delta: text.clone(),
                            });
                            if tab.model.blocks.len() != before {
                                structural = true;
                            } else {
                                delta = Some((tab.model.blocks.len() - 1, text));
                            }
                        }
                        // Reasoning is not shown; Finish is followed by Result.
                        Ok(AgentEvent::ReasoningDelta { .. }) | Ok(AgentEvent::Finish { .. }) => {}
                        Ok(agent_event) => {
                            tab.model.apply_event(&agent_event);
                            structural = true;
                        }
                        Err(e) => {
                            tab.model.note(format!("unrecognized event: {e}"));
                            structural = true;
                        }
                    }
                }
                Some(Ok(ServerMessage::Result { session_id, .. })) => {
                    tab.model.set_session(Some(session_id));
                    terminal = true;
                    structural = true;
                }
                Some(Ok(ServerMessage::Error { error, .. })) => {
                    tab.model.fail(error);
                    terminal = true;
                    structural = true;
                }
                Some(Ok(_)) => {}
                Some(Err(ClientError::Interrupted)) => {
                    tab.model.note("interrupted");
                    terminal = true;
                    structural = true;
                }
                Some(Err(e)) => {
                    tab.model.fail(e.to_string());
                    terminal = true;
                    structural = true;
                }
            }

            if terminal {
                tab.model.finish_ask();
            }
        }

        if terminal && let Some(conn) = self.conns.get_mut(&event.tab_id) {
            conn.task = None;
        }

        let active = active_id == event.tab_id;
        if active {
            if structural {
                self.send_transcript(sender).await?;
            } else if let Some((block, delta)) = delta {
                send_msg(sender, &ServerMsg::TextDelta { block, delta }).await?;
            }
        }
        if structural {
            self.send_tabs(sender).await?;
        }
        if active && structural {
            self.send_status(sender).await?;
        }
        if terminal {
            self.refresh_usage(event.tab_id).await;
            if active {
                self.send_status(sender).await?;
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        for (_, mut conn) in self.conns.drain() {
            if let Some(task) = conn.task.take() {
                if let Some(client) = &conn.client {
                    let _ = client.interrupt().await;
                }
                let _ = task.await;
            }
            if let Some(client) = conn.client.take() {
                client.close().await;
            }
        }
        self.admin.close().await;
    }
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
            if tx
                .send(TabEvent {
                    tab_id,
                    frame: Some(frame),
                })
                .is_err()
            {
                return; // event loop gone
            }
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
}
