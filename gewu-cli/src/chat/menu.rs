mod plugins;

use super::*;
use crate::commands::{COMMANDS, Cmd};
use gewu_protocol::{AskAttachment, InspectSection, InspectionEntry};
use std::path::{Component, Path};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
enum Action {
    CheckPlugin(String, String),
    EditKey,
    CustomModel(Vec<String>),
    SaveModel(Vec<String>, &'static str),
    ClearAttachments,
    Run(Cmd, String),
    File(String),
    WorkspaceFile(String),
    Detail(InspectSection, InspectionEntry),
    Node(gewu_protocol::WorkflowNodeState),
    Confirm(Change),
    Apply(Change),
    Close,
}

#[derive(Clone)]
enum Change {
    Revert(usize),
    Delete(String),
    Cancel(String),
}
type Choice = (String, Action);

pub(super) struct Menu {
    title: String,
    body: String,
    choices: Vec<Choice>,
    selected: usize,
    input: Option<Cmd>,
    pub(super) settings_child: bool,
    settings_root: bool,
    custom_fields: Option<Vec<String>>,
}

impl Menu {
    pub(super) fn display(&self) -> (String, Option<usize>) {
        let mut selected_line = None;
        let mut lines = vec![self.title.clone(), self.body.clone()];
        let start = self.selected.saturating_sub(8);
        if start > 0 {
            lines.push("  …".into());
        }
        for (index, (name, _)) in self.choices.iter().enumerate().skip(start).take(16) {
            if index == self.selected {
                selected_line = Some(lines.iter().map(|s| s.split('\n').count()).sum());
            }
            lines.push(format!(
                "{} {:>2}. {}",
                if index == self.selected { "›" } else { " " },
                index + 1,
                name
            ));
        }
        if start + 16 < self.choices.len() {
            lines.push(format!("  … ({} entries)", self.choices.len()));
        }
        (lines.join("\n"), selected_line)
    }
}

fn run(cmd: Cmd, arg: &str) -> Action {
    Action::Run(cmd, arg.into())
}
fn mime(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpeg" | "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

impl Chat {
    pub(super) fn show_sessions(&mut self, list: Vec<SessionSummary>) {
        let body = if list.is_empty() {
            "No saved sessions."
        } else {
            "Select a conversation."
        };
        self.menu(
            "Sessions",
            body,
            list.into_iter()
                .map(|s| {
                    (
                        format!(
                            "{} · {} · {} · {}",
                            s.updated_at,
                            s.agent,
                            match s.workspace_path.as_deref() {
                                Some("") => "No workspace",
                                Some(path) => path,
                                None => "legacy workspace",
                            },
                            s.session_id
                        ),
                        run(Cmd::Resume, &s.session_id),
                    )
                })
                .collect(),
            Some(Cmd::Resume),
        );
    }
    fn menu(
        &mut self,
        title: &str,
        body: impl Into<String>,
        choices: Vec<Choice>,
        input: Option<Cmd>,
    ) {
        self.panel = Panel::Menu(Menu {
            title: title.into(),
            body: body.into(),
            choices,
            selected: 0,
            settings_child: false,
            settings_root: false,
            custom_fields: None,
            input,
        });
        self.editor = editor();
        self.view.scroll = 0;
    }
    pub(crate) fn move_choice(&mut self, down: bool) -> bool {
        if let Panel::Menu(menu) = &mut self.panel
            && !menu.choices.is_empty()
        {
            let count = menu.choices.len();
            menu.selected = (menu.selected + if down { 1 } else { count - 1 }) % count;
            self.view.follow_choice = true;
            true
        } else {
            false
        }
    }
    fn idle(&self) -> Result<()> {
        ensure!(
            self.model.phase != Phase::Busy,
            "Wait for the current turn or interrupt it first."
        );
        Ok(())
    }
    fn confirm(&mut self, body: String, change: Change) {
        self.menu(
            "Confirm",
            body,
            vec![
                ("Keep unchanged".into(), Action::Close),
                ("Confirm".into(), Action::Apply(change)),
            ],
            None,
        );
    }

    fn queue(&mut self, name: String, path: String) -> Result<()> {
        ensure!(
            Path::new(&path)
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
            "Expected a workspace-relative attachment."
        );
        if !self
            .attachments
            .iter()
            .any(|a| a.path.as_ref() == Some(&path))
        {
            self.attachments.push(AskAttachment {
                mime_type: mime(&name).into(),
                name: name.clone(),
                data: None,
                path: Some(path),
            });
        }
        self.end_form();
        self.notice = format!("Queued: {name}. Send a message to attach it.");
        Ok(())
    }
    pub(super) async fn choose_menu(&mut self, endpoint: &Endpoint, text: &str) -> Result<()> {
        let Panel::Menu(menu) = &self.panel else {
            return Ok(());
        };
        let from_settings = menu.settings_child || menu.settings_root;
        let choice = if let Some(fields) = &menu.custom_fields {
            ensure!(
                !text.is_empty() && !text.contains(char::is_whitespace),
                "Enter a value without spaces."
            );
            let mut fields = fields.clone();
            fields.push(text.to_owned());
            Action::CustomModel(fields)
        } else if text.is_empty() {
            menu.choices
                .get(menu.selected)
                .map(|row| row.1.clone())
                .unwrap_or(Action::Close)
        } else if menu.input == Some(Cmd::Plugins) {
            run(Cmd::Plugins, &format!("install {text}"))
        } else if let Ok(index) = text.parse::<usize>() {
            menu.choices
                .get(index.wrapping_sub(1))
                .context("Unknown selection")?
                .1
                .clone()
        } else if let Some(cmd) = menu.input {
            run(cmd, text)
        } else {
            bail!("Use the arrow keys or enter a selection number.")
        };
        match choice {
            Action::CustomModel(fields) => {
                let prompts = [
                    "Endpoint name (for example, lab)",
                    "API base URL (include /v1 if required)",
                    "Model ID as listed by this endpoint",
                ];
                match fields.as_slice() {
                    [group] => crate::key_entry::validate_group(group)?,
                    [_, url] => {
                        crate::key_entry::clean_url(url)?;
                    }
                    [_, _, model] => crate::key_entry::validate_model_id(model)?,
                    _ => {}
                }
                if let Some(prompt) = prompts.get(fields.len()) {
                    self.menu("Custom model", *prompt, vec![], None);
                    if let Panel::Menu(menu) = &mut self.panel {
                        menu.custom_fields = Some(fields);
                    }
                } else {
                    let choices = [
                        ("OpenAI-compatible Chat Completions", "openai-compatible"),
                        ("OpenAI Responses", "openai"),
                        ("Anthropic Messages", "anthropic"),
                        ("DeepSeek", "deepseek"),
                        ("Moonshot", "moonshot"),
                        ("GLM", "glm"),
                    ]
                    .into_iter()
                    .map(|(label, protocol)| {
                        (label.into(), Action::SaveModel(fields.clone(), protocol))
                    })
                    .collect();
                    self.menu(
                        "Custom model",
                        format!(
                            "{} · {}\n{}\nSelect the API protocol to save.",
                            fields[0], fields[2], fields[1]
                        ),
                        choices,
                        None,
                    );
                }
            }
            Action::SaveModel(fields, protocol) => {
                crate::key_entry::add_model(
                    endpoint
                        .keys
                        .path
                        .as_deref()
                        .context("No local configuration")?,
                    &fields[0],
                    &fields[1],
                    &fields[2],
                    protocol,
                )?;
                self.end_form();
                self.notice = "Model saved. Reopen GeWu to select it in Settings.".into();
            }
            Action::EditKey => {
                self.key_entry = Some(endpoint.keys.entry(&self.model_name)?);
                self.editor = editor();
            }
            Action::CheckPlugin(id, version) => self.check_plugin(&id, &version).await?,
            Action::ClearAttachments => {
                self.attachments.clear();
                self.end_form();
                self.notice = "Attachment queue cleared".into();
            }
            Action::Close => self.end_form(),
            Action::Run(cmd, arg) => {
                self.run_command(endpoint, cmd, &arg).await?;
            }
            Action::WorkspaceFile(path) => {
                self.idle()?;
                self.queue(
                    Path::new(&path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into(),
                    path,
                )?;
            }
            Action::File(path) => {
                let preview = self
                    .control()?
                    .read_file(&path)
                    .await
                    .map(|(body, truncated)| {
                        if truncated {
                            format!("{body}\n[truncated]")
                        } else {
                            body
                        }
                    })
                    .unwrap_or_else(|err| format!("Preview unavailable: {err}"));
                self.menu(
                    &path,
                    preview,
                    vec![(
                        "Attach to next message".into(),
                        Action::WorkspaceFile(path.clone()),
                    )],
                    None,
                );
            }
            Action::Node(node) => {
                let mut choices = vec![];
                if !node.sessions.is_empty() {
                    let children = self.control()?.inspect(InspectSection::Subagents).await?;
                    for child in children
                        .into_iter()
                        .filter(|e| node.sessions.contains(&e.key))
                    {
                        choices.push((
                            child.label.clone(),
                            Action::Detail(InspectSection::Subagents, child),
                        ));
                    }
                }
                let body = format!(
                    "{} · {} done · {} failed\n{}",
                    node.status,
                    node.done,
                    node.failed,
                    if choices.is_empty() {
                        "No child session reported for this node yet."
                    } else {
                        "Select a child session to read its assignment and reply."
                    }
                );
                choices.push(("Back to workflows".into(), run(Cmd::Tasks, "")));
                self.menu(&node.id, body, choices, None);
            }
            Action::Detail(section, entry) => {
                let mut choices: Vec<_> = entry
                    .nodes
                    .into_iter()
                    .map(|node| {
                        (
                            format!(
                                "{} · {} · {} child sessions",
                                node.id,
                                node.status,
                                node.sessions.len()
                            ),
                            Action::Node(node),
                        )
                    })
                    .collect();
                if entry.cancellable {
                    choices.push((
                        "Cancel this run…".into(),
                        Action::Confirm(Change::Cancel(entry.key.clone())),
                    ));
                }
                choices.push(("Refresh list".into(), run(section_cmd(section), "")));
                if section == InspectSection::Subagents {
                    choices.push(("Workflows".into(), run(Cmd::Tasks, "")));
                }
                self.menu(&entry.label, &entry.detail, choices, None);
            }
            Action::Confirm(change) => {
                let body = match &change {
                    Change::Revert(turn) => format!(
                        "Remove turn {} and all later turns from this conversation? Workspace files stay unchanged. /undo restores the staged history until the next new turn.",
                        turn + 1
                    ),
                    Change::Delete(id) => {
                        format!("Permanently delete session {id} and its child sessions?")
                    }
                    Change::Cancel(id) => {
                        format!("Cancel workflow {id}? Completed work is retained.")
                    }
                };
                self.confirm(body, change);
            }
            Action::Apply(change) => {
                match change {
                    Change::Cancel(id) => self.control()?.cancel_workflow(id).await?,
                    Change::Revert(turn) => {
                        self.idle()?;
                        self.control()?.revert_chat(Some(turn), false).await?;
                        self.refresh_context().await;
                    }
                    Change::Delete(id) => {
                        self.idle()?;
                        let current = self.model.session_id.as_deref() == Some(&id);
                        let client = endpoint.connect().await?;
                        let result = client.delete_session(&id).await;
                        client.close().await;
                        result?;
                        if current {
                            self.open(endpoint, None, None).await?;
                        }
                    }
                }
                self.end_form();
                self.notice = "Done".into();
            }
        }
        if let Panel::Menu(menu) = &mut self.panel {
            menu.settings_child = from_settings && !menu.settings_root;
        }
        if !matches!(self.panel, Panel::Chat) {
            self.editor = editor();
        }
        Ok(())
    }

    async fn show_settings(&mut self, endpoint: &Endpoint) -> Result<()> {
        let settings = self.control()?.settings().await?;
        if self.directory != settings.workspace {
            self.attachments.clear();
        }
        if self.model_name != settings.model {
            self.context = None;
        }
        self.directory = settings.workspace;
        self.model_name = settings.model;
        self.agent = settings.agent;
        self.effort = settings.effort_override;
        let effort = self
            .effort
            .as_ref()
            .or(settings.default_effort.as_ref())
            .map(String::as_str)
            .filter(|v| *v != gewu_protocol::PROVIDER_DEFAULT_EFFORT)
            .unwrap_or("Provider default");
        let source = if self.effort.is_some() {
            "session override"
        } else {
            "model default"
        };
        let mut choices = vec![("Model".into(), run(Cmd::Model, ""))];
        if endpoint.keys.target(&self.model_name).is_some() {
            choices.push(("API key".into(), Action::EditKey));
        }
        if settings.supports_effort {
            choices.push((
                format!("Reasoning effort  {effort} · {source}"),
                run(Cmd::Effort, ""),
            ));
        }
        choices.extend([
            (format!("Mode  {}", self.agent), run(Cmd::Agent, "")),
            (
                format!("Workspace  {}", self.workspace_label()),
                run(Cmd::Cd, ""),
            ),
            ("Skills".into(), run(Cmd::Skills, "")),
            ("MCP tools".into(), run(Cmd::Mcp, "")),
            ("Plugins".into(), run(Cmd::Plugins, "")),
        ]);
        let details = format!(
            "Provider  {}\nServer default model  {}\nContext window  {} tokens\nFile path checks  {}\nSession server  {}\n\nCurrent session",
            settings.provider,
            settings.default_model,
            settings.context_limit,
            settings.path_checks,
            endpoint.lifecycle_label()
        );
        self.menu("Settings", details, choices, None);
        if let Panel::Menu(menu) = &mut self.panel {
            menu.settings_root = true;
        }
        Ok(())
    }

    pub(super) async fn run_command(
        &mut self,
        endpoint: &Endpoint,
        cmd: Cmd,
        arg: &str,
    ) -> Result<bool> {
        if self.directory.is_empty() && matches!(cmd, Cmd::Attach | Cmd::Files) {
            anyhow::bail!("Choose a workspace with /cd before accessing files.");
        }
        if COMMANDS.iter().any(|s| s.cmd == cmd && s.idle) {
            self.idle()?;
        }
        match cmd {
            Cmd::Exit => return Ok(true),
            Cmd::Help => {
                let lines = COMMANDS
                    .iter()
                    .map(|s| {
                        format!(
                            "{:<12} {}{}",
                            s.name,
                            s.help,
                            if s.aliases.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", s.aliases.join(", "))
                            }
                        )
                    })
                    .collect::<Vec<_>>();
                self.menu("Commands", lines.join("\n"), vec![], None);
            }
            Cmd::Workflow => {
                let profiles = self.control()?.list_agents().await?;
                ensure!(
                    arg.is_empty() || arg == "import",
                    "Usage: /workflow [import]"
                );
                let draft = self.control()?.workflow_draft(None).await?;
                let mut designer = crate::workflow::Designer::open(draft, profiles);
                if arg == "import" {
                    let reply = self
                        .model
                        .blocks
                        .iter()
                        .rev()
                        .find_map(|block| match block {
                            Block::Assistant(text) => Some(text.as_str()),
                            _ => None,
                        })
                        .context("Ask for a workflow JSON draft in chat first")?;
                    designer.import_reply(reply)?;
                }
                self.end_form();
                self.designer = Some(designer);
            }
            Cmd::New => {
                self.open(endpoint, None, None).await?;
            }
            Cmd::Resume => {
                self.pick(endpoint, (!arg.is_empty()).then_some(arg), false)
                    .await?;
            }
            Cmd::Model if arg.is_empty() => {
                let mut choices = vec![];
                let in_settings = matches!(&self.panel,Panel::Menu(m) if m.settings_root);
                if !in_settings
                    && self
                        .catalog
                        .iter()
                        .any(|m| m.selector == self.model_name && m.supports_reasoning_effort)
                {
                    choices.push(("Reasoning effort…".into(), run(Cmd::Effort, "")));
                }
                choices.extend(
                    self.catalog
                        .iter()
                        .map(|m| (m.selector.clone(), run(Cmd::Model, &m.selector))),
                );
                if endpoint.keys.path.is_some() {
                    choices.push(("Add custom model…".into(), Action::CustomModel(Vec::new())));
                }
                self.menu(
                    "Model",
                    format!("Current: {}", self.model_badge()),
                    choices,
                    (!in_settings).then_some(cmd),
                );
            }
            Cmd::Model => {
                let (_, _, effort) = self.control()?.switch_model(arg).await?;
                self.model_name = arg.into();
                self.effort = effort;
                self.context = None;
                self.refresh_context().await;
                if matches!(&self.panel, Panel::Menu(menu) if menu.settings_child) {
                    self.show_settings(endpoint).await?;
                } else {
                    self.end_form();
                }
                self.notice = self.model_badge();
            }
            Cmd::Effort if arg.is_empty() => {
                let mut choices = vec![
                    ("Configured default".into(), run(cmd, "default")),
                    (
                        "Provider default".into(),
                        run(cmd, gewu_protocol::PROVIDER_DEFAULT_EFFORT),
                    ),
                ];
                choices.extend(
                    gewu_protocol::REASONING_EFFORTS
                        .iter()
                        .map(|effort| (effort.to_string(), run(cmd, effort))),
                );
                self.menu(
                    "Reasoning effort",
                    format!("Current: {}. Applies to this session.", self.model_badge()),
                    choices,
                    Some(cmd),
                );
            }
            Cmd::Effort => {
                self.effort = self
                    .control()?
                    .switch_reasoning_effort((arg != "default").then_some(arg))
                    .await?
                    .1;
                self.end_form();
                self.notice = self.model_badge();
            }
            Cmd::Agent if arg.is_empty() => {
                let choices = self
                    .control()?
                    .list_agents()
                    .await?
                    .into_iter()
                    .filter(|name| matches!(name.as_str(), "build" | "plan"))
                    .map(|name| (name.clone(), run(Cmd::Agent, &name)))
                    .collect();
                self.menu(
                    "Agent",
                    format!("Current: {}", self.agent),
                    choices,
                    Some(cmd),
                );
            }
            Cmd::Plan | Cmd::Build | Cmd::Agent => {
                let name = match cmd {
                    Cmd::Plan => "plan",
                    Cmd::Build => "build",
                    _ => arg,
                };
                self.control()?.switch_agent(name).await?;
                self.agent = name.into();
                self.end_form();
                self.notice = format!("Active agent: {name}");
            }
            Cmd::Plugins => self.plugins(endpoint, arg).await?,
            Cmd::Settings => self.show_settings(endpoint).await?,
            Cmd::Status | Cmd::Context | Cmd::Usage => {
                self.refresh_context().await;
                let context = self
                    .context
                    .map(|(used, limit)| {
                        format!(
                            "Context estimate: {used} / {limit} tokens · {} remaining",
                            limit.saturating_sub(used)
                        )
                    })
                    .unwrap_or_else(|| "No context estimate yet".into());
                let body=match cmd{
                    Cmd::Context=>format!("{context}\nOlder history is summarized automatically near the limit."),
                    Cmd::Usage=>self.provider_usage.map(|u|format!("Uncached input: {}\nCache hits: {}\nCache writes: {}\nOutput: {}\nReasoning tokens reported: {}\nCache hit rate: {}",u.input_tokens,u.cached_tokens,u.cache_write_tokens,u.output_tokens,u.reasoning_tokens,u.cache_hit_rate_percent().map_or("—".into(),|n|format!("{n}%")))).unwrap_or_else(||"No provider usage reported yet.".into()),
                    _=>format!("Session: {}\nWorkspace: {}\nAgent: {}\nModel: {}\nPhase: {:?}\n{context}",self.model.session_id.as_deref().unwrap_or("—"),self.workspace_label(),self.agent,self.model_badge(),self.model.phase)
                };
                self.menu("Session information", body, vec![], None);
            }
            Cmd::Tools => {
                let mut tools = self.control()?.list_tools().await?;
                tools.sort();
                self.menu("Advertised tools", tools.join("\n"), vec![], None);
            }
            Cmd::Pwd => self.menu(
                "Server workspace",
                self.workspace_label().to_owned(),
                vec![],
                None,
            ),
            Cmd::Cd if arg.is_empty() || arg.starts_with("browse:") => {
                let path = arg.strip_prefix("browse:").unwrap_or(&self.directory);
                let (here, parent, entries) = self.control()?.browse_workspace(path).await?;
                let mut choices = vec![("Use this directory".into(), run(cmd, &here))];
                if let Some(parent) = parent {
                    choices.push(("..".into(), run(cmd, &format!("browse:{parent}"))));
                }
                choices.extend(entries.iter().map(|entry| {
                    (
                        entry.name.clone(),
                        run(cmd, &format!("browse:{}", entry.path)),
                    )
                }));
                self.menu("Choose server directory", here, choices, Some(cmd));
            }
            Cmd::Cd => {
                let path = Path::new(&self.directory).join(arg);
                let (_, root) = self
                    .control()?
                    .add_workspace(path.to_string_lossy())
                    .await?;
                self.directory = root.path;
                self.attachments.clear();
                self.end_form();
                self.notice = format!("Workspace: {}", self.directory);
            }
            Cmd::Files => {
                let path = Path::new(if arg.is_empty() { "." } else { arg });
                ensure!(
                    path.components()
                        .all(|c| matches!(c, Component::CurDir | Component::Normal(_))),
                    "Use a workspace-relative directory."
                );
                let entries = self.control()?.list_files(path.to_string_lossy()).await?;
                let mut choices = vec![];
                if path != Path::new(".") {
                    choices.push((
                        "..".into(),
                        run(
                            cmd,
                            &path.parent().unwrap_or(Path::new(".")).to_string_lossy(),
                        ),
                    ));
                }
                for entry in entries {
                    let child = path.join(&entry.name).to_string_lossy().into_owned();
                    choices.push(if entry.is_dir {
                        (format!("{}/", entry.name), run(cmd, &child))
                    } else {
                        (entry.name, Action::File(child))
                    });
                }
                self.menu(
                    "Workspace files",
                    path.display().to_string(),
                    choices,
                    Some(cmd),
                );
            }
            Cmd::Attach if arg.is_empty() => {
                let names = self
                    .attachments
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.menu(
                    "Attachments",
                    format!("Queued: {names}\nEnter a local path, or browse the server workspace."),
                    if self.attachments.is_empty() {
                        vec![("Browse workspace files".into(), run(Cmd::Files, ""))]
                    } else {
                        vec![
                            ("Browse workspace files".into(), run(Cmd::Files, "")),
                            ("Remove queued attachments".into(), Action::ClearAttachments),
                        ]
                    },
                    Some(cmd),
                );
            }
            Cmd::Attach => {
                let path = Path::new(arg);
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .context("Invalid local filename")?
                    .to_owned();
                let file = tokio::fs::File::open(path).await?;
                ensure!(file.metadata().await?.is_file(), "Expected a regular file");
                let limit = (gewu_server::server::MAX_LINE_BYTES - 4096) * 3 / 4;
                ensure!(
                    file.metadata().await?.len() <= limit as u64,
                    "Too large for one WebSocket upload. Put it in the server workspace and attach it via /files."
                );
                let mut bytes = vec![];
                file.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
                ensure!(
                    bytes.len() <= limit,
                    "File grew beyond the upload limit; attach via /files."
                );
                let (stored, _) = self
                    .control()?
                    .upload_attachment(name.clone(), &bytes)
                    .await?;
                self.queue(name, stored)?;
            }
            Cmd::Copy => {
                let reply = self
                    .model
                    .blocks
                    .iter()
                    .rev()
                    .find_map(|block| {
                        if let Block::Assistant(text) = block {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .context("No assistant reply to copy")?;
                use std::io::Write;
                let mut copy = std::process::Command::new("/usr/bin/pbcopy")
                    .stdin(std::process::Stdio::piped())
                    .spawn()?;
                copy.stdin
                    .take()
                    .context("Clipboard unavailable")?
                    .write_all(reply.as_bytes())?;
                ensure!(copy.wait()?.success(), "Clipboard write failed");
                self.end_form();
                self.notice = "Reply copied".into();
            }
            Cmd::Export => {
                let default = format!(
                    "gewu-{}.md",
                    self.model
                        .session_id
                        .as_deref()
                        .unwrap_or("session")
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .take(24)
                        .collect::<String>()
                );
                let path = Path::new(if arg.is_empty() {
                    default.as_str()
                } else {
                    arg
                });
                let mut options = tokio::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                let mut file = options
                    .open(path)
                    .await
                    .context("Choose a new local export path")?;
                for block in &self.model.blocks {
                    file.write_all(
                        format!("## {}\n\n{}\n\n", block.label(), block.text()).as_bytes(),
                    )
                    .await?;
                }
                file.flush().await?;
                self.end_form();
                self.notice = format!("Exported to {}", std::path::absolute(path)?.display());
            }
            Cmd::Revert => {
                let messages = self
                    .control()?
                    .get_session(self.model.session_id.as_deref().context("No session")?)
                    .await?;
                let turns = messages
                    .iter()
                    .filter_map(|msg| {
                        if let gewu_protocol::Message::User { content } = msg {
                            Some(
                                content
                                    .iter()
                                    .find_map(|part| {
                                        if let gewu_protocol::ContentPart::Text { text } = part {
                                            Some(
                                                text.lines()
                                                    .next()
                                                    .unwrap_or("")
                                                    .chars()
                                                    .take(90)
                                                    .collect::<String>(),
                                            )
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or("Attachment".into()),
                            )
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                if arg.is_empty() {
                    let choices = turns
                        .into_iter()
                        .enumerate()
                        .map(|(index, title)| (title, Action::Confirm(Change::Revert(index))))
                        .collect();
                    self.menu(
                        "Revert from turn",
                        "The selected turn and later turns will be removed. Files stay unchanged.",
                        choices,
                        None,
                    );
                } else {
                    let num: usize = arg.parse()?;
                    ensure!(num > 0 && num <= turns.len(), "Unknown turn");
                    self.confirm(
                        format!(
                            "Remove turn {num}: {} and later turns? Files stay unchanged.",
                            turns[num - 1]
                        ),
                        Change::Revert(num - 1),
                    );
                }
            }
            Cmd::Undo => {
                self.control()?.revert_chat(None, true).await?;
                self.refresh_context().await;
                self.end_form();
                self.notice = "Conversation restored".into();
            }
            Cmd::Delete => {
                let choices = Self::sessions(endpoint)
                    .await?
                    .into_iter()
                    .map(|s| {
                        (
                            format!("{}  {}", s.updated_at, s.session_id),
                            Action::Confirm(Change::Delete(s.session_id)),
                        )
                    })
                    .collect();
                self.menu(
                    "Delete a saved session",
                    "Select a session; a separate confirmation follows.",
                    choices,
                    None,
                );
            }
            Cmd::Tasks | Cmd::Subagents | Cmd::Todos | Cmd::Skills | Cmd::Mcp => {
                let section = match cmd {
                    Cmd::Tasks => InspectSection::Tasks,
                    Cmd::Subagents => InspectSection::Subagents,
                    Cmd::Todos => InspectSection::Todos,
                    Cmd::Skills => InspectSection::Skills,
                    _ => InspectSection::Mcp,
                };
                let entries = self.control()?.inspect(section).await?;
                let body = if entries.is_empty() {
                    match section {
                        InspectSection::Subagents => {
                            "No child sessions yet. Ask the agent to delegate a task to explore or general."
                        }
                        _ => "No entries reported.",
                    }
                } else {
                    "Select an entry for details."
                };
                let mut choices: Vec<_> = entries
                    .into_iter()
                    .map(|entry| (entry.label.clone(), Action::Detail(section, entry)))
                    .collect();
                choices.push(("Refresh".into(), run(cmd, "")));
                self.menu(&format!("{section:?}"), body, choices, None);
            }
        }
        Ok(false)
    }
}

fn section_cmd(section: InspectSection) -> Cmd {
    match section {
        InspectSection::Tasks => Cmd::Tasks,
        InspectSection::Subagents => Cmd::Subagents,
        InspectSection::Todos => Cmd::Todos,
        InspectSection::Skills => Cmd::Skills,
        InspectSection::Mcp => Cmd::Mcp,
    }
}
