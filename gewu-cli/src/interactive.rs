use crate::{chat::Chat, endpoint::Endpoint, view};
use anyhow::{Context, Result, ensure};
use crossterm::event::{
    self, Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use gewu_client::Phase;
use gewu_server::bootstrap::CommonArgs;
use std::io::{self, IsTerminal};
use std::time::Duration;

#[derive(clap::Args)]
pub struct CliArgs {
    /// Start with this prompt and stay in the session
    pub prompt: Option<String>,
    /// Connect to an existing agent server over WebSocket
    #[arg(long)]
    pub server: Option<String>,
    /// Select a saved session or resume a specific ID
    #[arg(long, num_args=0..=1, value_name="ID", conflicts_with="last")]
    pub resume: Option<Option<String>>,
    /// Resume the most recent session in the server's current workspace
    #[arg(long)]
    pub last: bool,
}

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            io::stdout(),
            event::DisableMouseCapture,
            event::DisableBracketedPaste
        );
        let _ = ratatui::try_restore();
    }
}

pub async fn cli(args: CliArgs, common: &CommonArgs, token: Option<String>) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "gewu cli requires a terminal; use gewu exec for scripts"
    );
    // The fullscreen renderer owns the terminal while its private server runs.
    let _ = tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
    let external = args.server.is_some();
    let endpoint = Endpoint::open(args.server.clone(), token, common).await?;
    let mut session = None;
    let mut saved = None;
    let result = async {
        let mut chat = Chat::new(&endpoint, &common.agent, args.prompt).await?;
        if args.resume.is_some() || args.last {
            chat.pick(&endpoint, args.resume.flatten().as_deref(), args.last)
                .await?;
        } else {
            chat.open(&endpoint, None, None).await?;
        }
        let result = screen(&mut chat, &endpoint).await;
        if let Some(id) = &chat.model.session_id
            && let Ok(client) = endpoint.connect().await
        {
            saved = client
                .list_sessions()
                .await
                .ok()
                .map(|list| list.iter().any(|item| &item.session_id == id));
            client.close().await;
        }
        chat.disconnect().await;
        session = chat.model.session_id.take();
        result
    }
    .await;
    let shutdown = endpoint.close().await;
    if let Some(id) = session {
        let id = shell_arg(&id);
        if saved == Some(false) {
            eprintln!("No saved conversation yet. To start a new one, run: gewu");
            return result.and(shutdown);
        }
        if saved.is_none() {
            eprintln!("Could not verify whether this conversation was saved.");
        }
        let command = match args.server {
            Some(url) if url.contains(['@', '?', '#']) => {
                format!("gewu cli --server \"$GEWU_SERVER_URL\" --resume {id}")
            }
            Some(url) => format!("gewu cli --server {} --resume {id}", shell_arg(&url)),
            None => format!("gewu cli --resume {id}"),
        };
        if io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
            eprintln!("\x1b[36m{command}\x1b[0m");
        } else {
            eprintln!("{command}");
        }
        if command.contains("\"$GEWU_SERVER_URL\"") {
            eprintln!("Set GEWU_SERVER_URL to the original server URL first.");
        }
        if !external && common.db_path.is_some() {
            eprintln!("Use the original --db-path before the cli subcommand.");
        }
        if external {
            eprintln!("To interrupt an active turn, reconnect and press Ctrl+C.");
        }
    }
    result.and(shutdown)
}

fn shell_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

async fn screen(chat: &mut Chat, endpoint: &Endpoint) -> Result<()> {
    let _restore = Restore;
    let mut term = ratatui::try_init()?;
    crossterm::execute!(
        io::stdout(),
        event::EnableMouseCapture,
        event::EnableBracketedPaste
    )?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut displayed_seconds = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let seconds = chat.turn_started.map(|start| start.elapsed().as_secs());
                dirty |= seconds != displayed_seconds;
                displayed_seconds = seconds;
                if dirty { term.draw(|f| view::draw(f, chat))?; dirty = false; }
            }
            frame = chat.frame() => {
                let frame = frame.context("server disconnected; resume the session to reconnect")??;
                if let gewu_protocol::ServerMessage::DraftChanged { revision, .. } = &frame
                    && let Some(designer) = &mut chat.designer
                    && *revision > designer.revision()
                {
                    if designer.dirty() {
                        designer.message = format!("Revision {revision} saved elsewhere. Ctrl+L to reload.");
                    } else {
                        match chat.control()?.workflow_draft(None).await {
                            Ok(draft) => chat.designer.as_mut().unwrap().loaded(draft),
                            Err(e) => chat.designer.as_mut().unwrap().message = e.to_string(),
                        }
                    }
                }
                chat.apply(frame)?;
                dirty = true;
            }
            event = events.next() => {
                let Some(event) = event.transpose()? else { break };
                dirty = true;
                if let Some(entry) = &mut chat.key_entry {
                    use crate::key_entry::Input;
                    match entry.input(event) {
                        Input::Stay => {}
                        Input::Cancel => chat.key_entry = None,
                        Input::Save(test_request) => {
                            entry.message = "Checking model access…".into();
                            term.draw(|f| view::draw(f, chat))?;
                            let entry = chat.key_entry.as_ref().unwrap();
                            match endpoint.keys.verify_and_save(entry, test_request).await {
                                Ok(()) => {
                                    chat.key_entry = None;
                                    chat.notice = "Model access verified. API key active.".into();
                                }
                                Err(e) => chat.key_entry.as_mut().unwrap().message = e.to_string(),
                            }
                        }
                    }
                    continue;
                }
                if let Some(designer) = &mut chat.designer {
                    use crate::workflow::Effect;
                    let action = match event {
                        Event::Paste(text) => { designer.paste(text); continue; }
                        Event::Key(k) if k.kind != KeyEventKind::Release => designer.key(k),
                        _ => continue,
                    };
                    match action {
                        Err(error) => designer.message = format!("{error:#}"),
                        Ok(Effect::Stay) => {}
                        Ok(Effect::Close) => chat.designer = None,
                        Ok(Effect::Reload) => {
                            match chat.control()?.workflow_draft(None).await {
                                Ok(draft) => chat.designer.as_mut().unwrap().loaded(draft),
                                Err(e) => chat.designer.as_mut().unwrap().message = e.to_string(),
                            }
                        }
                        Ok(Effect::Save(update)) => {
                            match chat.control()?.workflow_draft(Some(update)).await {
                                Ok(draft) => chat.designer.as_mut().unwrap().loaded(draft),
                                Err(e) => chat.designer.as_mut().unwrap().message = e.to_string(),
                            }
                        }
                        Ok(Effect::Run(spec)) => {
                            let result = chat.control()?.start_workflow(spec).await;
                            match result {
                                Ok(id) => {
                                    chat.designer = None;
                                    chat.notice = format!("Started {id} · /tasks for progress");
                                }
                                Err(error) => chat.designer.as_mut().unwrap().message = error.to_string(),
                            }
                        }
                    }
                    continue;
                }
                match event {
                    Event::Paste(text) => { chat.editor.insert_str(text); chat.commands_hidden = false; chat.command_index = 0; }
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => chat.view.click(m.column, m.row),
                        MouseEventKind::ScrollUp => chat.scroll_up(3),
                        MouseEventKind::ScrollDown => chat.scroll_down(3),
                        _ => {}
                    },
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if k.modifiers == KeyModifiers::NONE && matches!(k.code,KeyCode::Up|KeyCode::Down)
                            && chat.move_choice(k.code==KeyCode::Down) { continue; }
                        let suggestions = chat.suggestions();
                        if !suggestions.is_empty() && k.modifiers == KeyModifiers::NONE {
                            let count = suggestions.len();
                            let selected = chat.command_index.min(count - 1);
                            match k.code {
                                KeyCode::Up => { chat.command_index = (selected + count - 1) % count; continue; }
                                KeyCode::Down => { chat.command_index = (selected + 1) % count; continue; }
                                KeyCode::Esc => { chat.commands_hidden = true; continue; }
                                KeyCode::Tab | KeyCode::Enter => {
                                    chat.complete_command(suggestions[selected].0);
                                    if k.code == KeyCode::Tab { continue; }
                                }
                                _ => {}
                            }
                        }
                        match (k.code, k.modifiers) {
                            (KeyCode::Char('p'), KeyModifiers::CONTROL) => chat.open_palette(),
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) if chat.model.phase == Phase::Busy => {
                                if let Err(e) = chat.interrupt().await { chat.notice = e.to_string(); }
                            }
                            (KeyCode::Char('c'|'d'), KeyModifiers::CONTROL) => break,
                            (KeyCode::Enter, KeyModifiers::NONE) => match chat.submit(endpoint).await {
                                Ok(true) => break,
                                Ok(false) => {}
                                Err(e) => chat.notice = format!("{e:#}"),
                            },
                            (KeyCode::Enter, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => chat.editor.insert_newline(),
                            (KeyCode::PageUp, _) => chat.scroll_up(10),
                            (KeyCode::PageDown, _) => chat.scroll_down(10),
                            (KeyCode::Esc, _) => if let Err(e) = chat.escape(endpoint).await { chat.notice = e.to_string(); },
                            _ => { chat.editor.input(k); chat.command_index = 0; chat.commands_hidden = false; }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
