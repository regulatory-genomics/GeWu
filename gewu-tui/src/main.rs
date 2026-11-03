//! `gewu-tui` terminal UI: chat with a running `gewu-server serve` in a
//! full-screen terminal interface, with one tab per conversation session.
//!
//! ```text
//! cargo run -p gewu-server -- serve          # in another terminal
//! cargo run -p gewu-tui
//! ```

mod tui;
mod tui_view;
mod workspace;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use gewu_client::{AskStream, Client, Error as ClientError};
use gewu_protocol::{AgentEvent, ServerMessage};
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode,
    KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::{init as tui_init, restore as tui_restore};
use std::collections::HashMap;
use std::io::Write;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tui::App;
use tui_view::draw;
use workspace::{TabClick, Workspace, short_id};

const SCROLL_STEP: usize = 3;

#[derive(clap::Parser)]
#[command(name = "gewu-tui")]
#[command(version = "0.1.0")]
#[command(about = "Terminal UI for a running gewu server (start it with `gewu-server serve`)", long_about = None)]
struct Cli {
    /// Host running the gewu server
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// TCP port of the gewu server (newline-delimited JSON)
    #[arg(long, default_value_t = 8421)]
    tcp_port: u16,

    /// Focus a specific stored session (also via GEWU_SESSION_ID)
    #[arg(long, env = "GEWU_SESSION_ID")]
    session_id: Option<String>,

    /// Focus a fresh session tab instead of the most recent conversation
    #[arg(long)]
    new: bool,
}

/// Per-tab connection state: the tab's own server connection (created the
/// first time the tab needs to talk — ask, usage, interrupt) plus the task
/// forwarding its ask stream into the event loop.
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Admin connection: session listing/loading at startup, deletions later.
    // It is never used for asks, so the server does not pin a session to it.
    let admin = Client::tcp(&cli.host, cli.tcp_port)
        .await
        .context("failed to connect to the gewu server (is `gewu-server serve` running?)")?;

    let mut ws = Workspace::new();
    let mut conns: HashMap<u64, Conn> = HashMap::new();

    // Show which model the server runs. Non-fatal: the status line simply
    // omits it when unavailable.
    let model_label = match admin.info().await {
        Ok(info) => Some(format!("{}/{}", info.provider, info.model)),
        Err(e) => {
            eprintln!("could not read server info: {e}");
            None
        }
    };

    // One tab per stored session. `list_sessions` is newest-first; opening
    // them in reverse puts the oldest tab on the left and the newest on the
    // right, so the bar reads oldest → newest like OpenCode's.
    match admin.list_sessions().await {
        Ok(sessions) => {
            for summary in sessions.into_iter().rev() {
                match admin.get_session(&summary.session_id).await {
                    Ok(messages) => {
                        ws.open_session(&summary.session_id, &messages);
                    }
                    Err(e) => {
                        let id = ws.open_blank();
                        let app = &mut ws.tab_mut(id).expect("just added").app;
                        app.model_label = model_label.clone();
                        app.note(format!(
                            "could not load session {}: {e}",
                            short_id(&summary.session_id)
                        ));
                    }
                }
            }
        }
        Err(e) => {
            let id = ws.open_blank();
            ws.tab_mut(id)
                .expect("just added")
                .app
                .note(format!("could not list sessions: {e}"));
        }
    }
    if ws.is_empty() {
        ws.open_blank();
    }

    // `--session-id` focuses (or opens) that session's tab; `--new` focuses a
    // fresh tab. All stored tabs stay open either way.
    if let Some(explicit) = &cli.session_id
        && !ws.focus_session(explicit)
    {
        ws.open_session(explicit, &[]);
    }
    if cli.new {
        ws.open_blank();
    }
    for tab in &mut ws.tabs {
        tab.app.model_label = model_label.clone();
    }

    // Context usage for the initially focused session (connects its tab).
    activate_tab(&mut ws, &mut conns, &cli.host, cli.tcp_port).await;

    let result = run(
        admin,
        &mut ws,
        &mut conns,
        &cli.host,
        cli.tcp_port,
        model_label,
    )
    .await;
    restore_terminal();
    result?;

    // Surface every open tab's session id on normal exit so any of them can
    // be resumed (e.g. via GEWU_SESSION_ID).
    for tab in &ws.tabs {
        if let Some(session) = &tab.app.model.session_id {
            writeln!(std::io::stderr(), "[session: {session}]")?;
        }
    }
    Ok(())
}

/// Enter the alternate screen and ask the terminal for mouse events.
///
/// Mouse capture is what makes touchpad/wheel scrolling reach the app instead
/// of scrolling the terminal's own (alternate-screen) history — important
/// over SSH, where there is no local scrollback to fall back on.
fn init_terminal() -> ratatui::DefaultTerminal {
    let terminal = tui_init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);

    // Chain onto ratatui's restore-before-panic hook so mouse mode is
    // released even if the app panics.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    terminal
}

/// Release mouse mode, then restore the terminal.
fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    tui_restore();
}

/// Refresh the context-usage readout for a tab.
///
/// A successful measurement with real numbers updates the tab's status line;
/// "nothing to measure" replies are fine. The error is returned so callers
/// can decide whether to surface it.
async fn refresh_usage(client: &Client, app: &mut App) -> gewu_client::Result<()> {
    match client.usage(app.model.session_id.as_deref()).await {
        Ok(usage) if usage.used_tokens > 0 && usage.context_limit > 0 => {
            app.context = Some((usage.used_tokens as usize, usage.context_limit as usize));
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The focused tab's own connection, created on first use.
///
/// The server pins one session per connection, so each tab talks over its
/// own; `session_id` on the first ask/usage pins the right conversation.
async fn ensure_client(
    conns: &mut HashMap<u64, Conn>,
    tab_id: u64,
    host: &str,
    port: u16,
) -> Result<(), ClientError> {
    let conn = conns.entry(tab_id).or_default();
    if conn.client.is_none() {
        conn.client = Some(Client::tcp(host, port).await?);
    }
    Ok(())
}

/// On focusing a tab: make sure it has a connection and refresh its context
/// usage. Failures are noted in the tab, never fatal.
async fn activate_tab(ws: &mut Workspace, conns: &mut HashMap<u64, Conn>, host: &str, port: u16) {
    let tab_id = ws.active_tab().id;
    if ws.active_tab().app.model.session_id.is_none() {
        return;
    }
    if let Err(e) = ensure_client(conns, tab_id, host, port).await {
        let app = &mut ws.active_tab().app;
        app.note(format!("context usage unavailable: {e}"));
        return;
    }
    let client = conns
        .get(&tab_id)
        .and_then(|conn| conn.client.as_ref())
        .expect("just ensured");
    let app = &mut ws.active_tab().app;
    if let Err(e) = refresh_usage(client, app).await {
        app.note(format!("context usage unavailable: {e}"));
    }
}

/// Close a tab: stop any running ask, release its connection, delete the
/// session from the store, then remove the tab.
///
/// If deletion fails (e.g. a server too old for the `delete_session` op) the
/// tab stays open with an error note, so nothing is lost silently.
async fn close_tab(ws: &mut Workspace, conns: &mut HashMap<u64, Conn>, admin: &Client, id: u64) {
    if let Some(mut conn) = conns.remove(&id) {
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
    let session_id = ws.tab(id).and_then(|tab| tab.app.model.session_id.clone());
    if let Some(session_id) = session_id
        && let Err(e) = admin.delete_session(&session_id).await
    {
        if let Some(tab) = ws.tab_mut(id) {
            tab.app.note(format!("could not delete session: {e}"));
        }
        return;
    }
    ws.close(id);
}

/// Forward one tab's ask frames into the event loop, so background tabs keep
/// streaming while another tab is focused (the client's ask channel is a
/// bounded broadcast; an unpolled stream would lag and error).
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

/// Terminal event loop: routes crossterm events and per-tab ask frames into
/// the [`Workspace`], redrawing on every change.
async fn run(
    admin: Client,
    ws: &mut Workspace,
    conns: &mut HashMap<u64, Conn>,
    host: &str,
    port: u16,
    model_label: Option<String>,
) -> Result<()> {
    let mut terminal = init_terminal();
    let mut events = EventStream::new();

    // Liveness check; surfaces connection problems before the first prompt.
    match admin.ping().await {
        Ok(()) => ws.active_tab().app.note("connected"),
        Err(e) => {
            let app = &mut ws.active_tab().app;
            app.fail(format!("server not responding: {e}"));
            draw_once(&mut terminal, ws);
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            return Ok(());
        }
    }
    draw_once(&mut terminal, ws);

    // Ask frames from every tab arrive here; the loop keeps a sender alive so
    // `recv` only ever yields real events.
    let (tx, mut rx) = mpsc::unbounded_channel::<TabEvent>();
    let _tx_keepalive = tx.clone();

    loop {
        // Ensure the terminal reflects the latest state.
        draw_once(&mut terminal, ws);

        let active_id = ws.active_tab().id;
        tokio::select! {
            // -- keyboard / mouse --------------------------------------------
            maybe_event = events.next() => {
                let Some(event) = maybe_event else { break };
                match event? {
                    TermEvent::Key(key) => {
                        // Only act on key presses (not terminal-reported releases).
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL)
                                && ws.active_tab().app.model.phase == tui::Phase::Idle
                                && ws.active_tab().app.input.is_empty() => break,
                            // Tabs: Ctrl+T opens a fresh session, Ctrl+W closes
                            // (and deletes) the current one, Ctrl+PgUp/PgDn and
                            // Alt+1..9 switch.
                            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                let id = ws.open_blank();
                                ws.tab_mut(id).expect("just added").app.model_label = model_label.clone();
                            }
                            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                close_tab(ws, conns, &admin, active_id).await;
                                if ws.is_empty() {
                                    break;
                                }
                            }
                            KeyCode::PageUp if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                ws.switch_prev();
                                activate_tab(ws, conns, host, port).await;
                            }
                            KeyCode::PageDown if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                ws.switch_next();
                                activate_tab(ws, conns, host, port).await;
                            }
                            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT)
                                && c.is_ascii_digit() => {
                                ws.switch_to(c.to_digit(10).unwrap_or(0).saturating_sub(1) as usize);
                                activate_tab(ws, conns, host, port).await;
                            }
                            KeyCode::Esc => {
                                if ws.active_tab().app.model.phase == tui::Phase::Busy
                                    && let Some(client) = conns
                                        .get(&active_id)
                                        .and_then(|conn| conn.client.as_ref())
                                {
                                    let _ = client.interrupt().await;
                                }
                            }
                            KeyCode::Enter if ws.active_tab().app.model.phase == tui::Phase::Idle => {
                                let tab_id = ws.active_tab().id;
                                let app = &mut ws.active_tab().app;
                                if let Some(prompt) = app.take_prompt() {
                                    match ensure_client(conns, tab_id, host, port).await {
                                        Ok(()) => {
                                            let session = app.model.session_id.clone();
                                            let client = conns
                                                .get(&tab_id)
                                                .and_then(|conn| conn.client.as_ref())
                                                .expect("just ensured");
                                            let mut builder = client.ask(prompt.clone());
                                            if let Some(session) = session {
                                                builder = builder.session_id(session);
                                            }
                                            match builder.send().await {
                                                Ok(stream) => {
                                                    app.submit(prompt);
                                                    let task = spawn_ask_forwarder(tab_id, stream, tx.clone());
                                                    if let Some(conn) = conns.get_mut(&tab_id) {
                                                        conn.task = Some(task);
                                                    }
                                                }
                                                Err(e) => app.fail(format!("could not send: {e}")),
                                            }
                                        }
                                        Err(e) => app.fail(format!("could not connect: {e}")),
                                    }
                                }
                            }
                            KeyCode::Backspace => ws.active_tab().app.backspace(),
                            KeyCode::Delete => ws.active_tab().app.delete(),
                            KeyCode::Left => ws.active_tab().app.move_left(),
                            KeyCode::Right => ws.active_tab().app.move_right(),
                            // With an empty composer these scroll the transcript;
                            // otherwise they move the input cursor.
                            KeyCode::Home if ws.active_tab().app.input.is_empty() => ws.active_tab().app.scroll_to_top(),
                            KeyCode::End if ws.active_tab().app.input.is_empty() => ws.active_tab().app.scroll_to_bottom(),
                            KeyCode::Home => ws.active_tab().app.move_home(),
                            KeyCode::End => ws.active_tab().app.move_end(),
                            KeyCode::Up if ws.active_tab().app.input.is_empty() => {
                                ws.active_tab().app.scroll_up(SCROLL_STEP);
                            }
                            KeyCode::Down if ws.active_tab().app.input.is_empty() => {
                                ws.active_tab().app.scroll_down(SCROLL_STEP);
                            }
                            KeyCode::PageUp => ws.active_tab().app.scroll_up(10),
                            KeyCode::PageDown => ws.active_tab().app.scroll_down(10),
                            KeyCode::Char(c) => ws.active_tab().app.push_char(c),
                            _ => {}
                        }
                    }
                    // Touchpad/wheel scrolling arrives as mouse events once
                    // capture is on; route them into the transcript in any
                    // phase, including while a reply streams. The tab bar row
                    // is handled by the workspace: clicks switch tabs, the
                    // per-tab `×` closes (and deletes), `+` opens. A plain
                    // left click elsewhere toggles tool output/disclosure;
                    // Shift+click is left to the terminal for text selection.
                    TermEvent::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => ws.active_tab().app.scroll_up(SCROLL_STEP),
                        MouseEventKind::ScrollDown => ws.active_tab().app.scroll_down(SCROLL_STEP),
                        MouseEventKind::Down(MouseButton::Left)
                            if !mouse.modifiers.contains(KeyModifiers::SHIFT) =>
                        {
                            match ws.click(mouse.column, mouse.row) {
                                TabClick::Switch(index) => {
                                    ws.switch_to(index);
                                    activate_tab(ws, conns, host, port).await;
                                }
                                TabClick::Close(id) => {
                                    close_tab(ws, conns, &admin, id).await;
                                    if ws.is_empty() {
                                        break;
                                    }
                                }
                                TabClick::New => {
                                    let id = ws.open_blank();
                                    ws.tab_mut(id).expect("just added").app.model_label = model_label.clone();
                                }
                                TabClick::Nothing => {
                                    ws.active_tab().app.click_at(mouse.column, mouse.row);
                                }
                            }
                        }
                        _ => {}
                    },
                    // Resize and focus events just trigger a redraw.
                    _ => {}
                }
            }
            // -- ask frames from any tab --------------------------------------
            event = rx.recv() => {
                let Some(event) = event else { break };
                let Some(tab) = ws.tab_mut(event.tab_id) else {
                    continue; // tab closed mid-ask; drop the late frames
                };
                let terminal_frame = match &event.frame {
                    None => true,
                    Some(Ok(ServerMessage::Result { .. })) | Some(Err(_)) => true,
                    Some(_) => false,
                };
                match event.frame {
                    Some(Ok(ServerMessage::Event { event, .. })) => {
                        match serde_json::from_value::<AgentEvent>(event) {
                            Ok(event) => tab.app.apply_event(&event),
                            Err(e) => tab.app.note(format!("unrecognized event: {e}")),
                        }
                    }
                    Some(Ok(ServerMessage::Result { session_id, .. })) => {
                        tab.app.set_session(Some(session_id));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        let interrupted = matches!(e, ClientError::Interrupted);
                        if interrupted {
                            tab.app.note("interrupted");
                        } else {
                            tab.app.fail(e.to_string());
                        }
                    }
                    None => {}
                }
                if terminal_frame {
                    tab.app.finish_ask();
                    if let Some(client) = conns
                        .get(&event.tab_id)
                        .and_then(|conn| conn.client.as_ref())
                    {
                        let _ = refresh_usage(client, &mut tab.app).await;
                    }
                    if let Some(conn) = conns.get_mut(&event.tab_id) {
                        conn.task = None;
                    }
                }
            }
        }
    }

    // Shut every tab down: abort running asks, release all connections.
    for conn in conns.values_mut() {
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
    admin.close().await;
    Ok(())
}

/// Redraw immediately (used outside the main loop).
fn draw_once(terminal: &mut ratatui::DefaultTerminal, ws: &mut Workspace) {
    let _ = terminal.draw(|f| draw(f, ws));
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use workspace::tab_title;
    #[test]
    fn cli_has_no_subcommands() {
        // The client is a TUI only: no ask/chat/tools/agents/ping subcommands.
        let mut cmd = Cli::command();
        cmd.build();
        assert!(cmd.get_subcommands().next().is_none());
    }

    #[test]
    fn cli_defaults() {
        let cli = Cli::try_parse_from(["gewu-tui"]).unwrap();
        assert_eq!(cli.host, "127.0.0.1");
        assert_eq!(cli.tcp_port, 8421);
        assert_eq!(cli.session_id, None);
        assert!(!cli.new);
    }

    #[test]
    fn cli_accepts_new_flag() {
        let cli = Cli::try_parse_from(["gewu-tui", "--new"]).unwrap();
        assert!(cli.new);
    }

    #[test]
    fn cli_accepts_flags_and_env() {
        let cli = Cli::try_parse_from([
            "gewu-tui",
            "--host",
            "example.org",
            "--tcp-port",
            "9000",
            "--session-id",
            "abc",
        ])
        .unwrap();
        assert_eq!(cli.host, "example.org");
        assert_eq!(cli.tcp_port, 9000);
        assert_eq!(cli.session_id.as_deref(), Some("abc"));

        // GEWU_SESSION_ID feeds the same option.
        let cmd = Cli::command();
        let env = cmd
            .get_arguments()
            .find(|a| a.get_id() == "session_id")
            .and_then(|a| a.get_env())
            .is_some();
        assert!(env, "session_id reads GEWU_SESSION_ID");
    }

    #[test]
    fn tab_title_falls_back_to_new() {
        let app = App::new();
        assert_eq!(tab_title(&app), "new");
    }
}
