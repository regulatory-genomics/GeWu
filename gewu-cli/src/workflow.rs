//! Keyboard editor for the session's shared workflow document.
use anyhow::{Result, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_protocol::DraftUpdate;
use gewu_tools::workflow::spec::WorkflowSpec;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin},
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use ratatui_textarea::TextArea;
use serde_json::{Value, json};

pub(crate) enum Effect {
    Stay,
    Close,
    Save(DraftUpdate),
    Reload,
    Run(Value),
}

#[derive(Clone, Copy)]
enum Confirm {
    Close,
    Reload,
    Run,
}

pub(crate) struct Designer {
    saved: DraftUpdate,
    original: String,
    text: TextArea<'static>,
    profiles: Vec<String>,
    confirm: Option<Confirm>,
    new_step: Option<TextArea<'static>>,
    pub message: String,
}

fn area(text: &str) -> TextArea<'static> {
    let mut field = TextArea::from(text.lines().map(str::to_owned));
    field.set_cursor_line_style(Style::default());
    field.set_cursor_style(Style::default());
    field
}

impl Designer {
    pub fn open(draft: DraftUpdate, profiles: Vec<String>) -> Self {
        let original = serde_json::to_string_pretty(&draft.spec).unwrap();
        Self {
            text: area(&original),
            original,
            profiles,
            saved: draft,
            confirm: None,
            new_step: None,
            message: String::new(),
        }
    }

    pub fn loaded(&mut self, draft: DraftUpdate) {
        *self = Self::open(draft, std::mem::take(&mut self.profiles));
        self.message = format!("Shared revision {}", self.saved.revision);
    }

    pub fn dirty(&self) -> bool {
        self.new_step.is_some() || self.text.lines().join("\n") != self.original
    }

    pub fn revision(&self) -> u64 {
        self.saved.revision
    }

    fn spec(&self) -> Result<(Value, WorkflowSpec)> {
        let value: Value = serde_json::from_str(&self.text.lines().join("\n"))?;
        Ok((value.clone(), serde_json::from_value(value)?))
    }

    pub fn import_reply(&mut self, reply: &str) -> Result<()> {
        let spec = gewu_tools::workflow::spec::from_assistant_reply(reply)?;
        self.text = area(&serde_json::to_string_pretty(&spec)?);
        self.message = "Imported from the last reply · Ctrl+V validate · Ctrl+S save".into();
        Ok(())
    }

    fn checked(&self) -> Result<Value> {
        let (value, spec) = self.spec()?;
        spec.validate(&self.profiles.iter().map(String::as_str).collect::<Vec<_>>())
            .map_err(anyhow::Error::msg)?;
        Ok(value)
    }

    pub fn paste(&mut self, text: String) {
        if self.confirm.is_none() {
            self.new_step
                .as_mut()
                .unwrap_or(&mut self.text)
                .insert_str(text);
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Result<Effect> {
        if let Some(confirm) = self.confirm.take() {
            return Ok(match (k.code, confirm) {
                (KeyCode::Enter, Confirm::Close) => Effect::Close,
                (KeyCode::Enter, Confirm::Reload) => Effect::Reload,
                (KeyCode::Enter, Confirm::Run) => Effect::Run(self.checked()?),
                _ => Effect::Stay,
            });
        }
        if let Some(prompt) = &mut self.new_step {
            match k.code {
                KeyCode::Esc => self.new_step = None,
                KeyCode::Enter => {
                    let prompt = prompt.lines().join("\n");
                    ensure!(
                        !prompt.trim().is_empty(),
                        "Enter an instruction for this step"
                    );
                    let (mut value, spec) = self.spec()?;
                    let id = (1..)
                        .map(|i| format!("step{i}"))
                        .find(|id| !spec.nodes.iter().any(|n| &n.id == id))
                        .unwrap();
                    value["nodes"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"id":id,"agent":"explore","prompt":prompt}));
                    self.text = area(&serde_json::to_string_pretty(&value)?);
                    self.new_step = None;
                    self.message = if spec.edges.is_some() {
                        "Step added. Connect it in edges or conditional_edges."
                    } else {
                        "Step added to the sequence."
                    }
                    .into();
                }
                _ => {
                    prompt.input(k);
                }
            }
            return Ok(Effect::Stay);
        }
        match (k.code, k.modifiers) {
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                let (spec, _) = self.spec()?;
                return Ok(Effect::Save(DraftUpdate {
                    revision: self.saved.revision,
                    spec,
                }));
            }
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                let (_, spec) = self.spec()?;
                ensure!(
                    spec.nodes.is_empty(),
                    "The example opens in an empty workflow"
                );
                self.text = area(gewu_tools::workflow::spec::EXAMPLE.trim_end());
                self.message = "Example loaded · edit the input paths for your workspace".into();
            }
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.spec()?;
                self.new_step = Some(area(""));
            }
            (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                self.checked()?;
                self.message = "Graph validated".into();
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                self.checked()?;
                ensure!(
                    !self.dirty(),
                    "Save the draft with Ctrl+S before running it"
                );
                self.confirm = Some(Confirm::Run);
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                if !self.dirty() {
                    return Ok(Effect::Reload);
                }
                self.confirm = Some(Confirm::Reload);
            }
            (KeyCode::Esc, _) => {
                if !self.dirty() {
                    return Ok(Effect::Close);
                }
                self.confirm = Some(Confirm::Close);
            }
            _ => {
                self.text.input(k);
            }
        }
        Ok(Effect::Stay)
    }

    pub fn draw(&self, frame: &mut Frame) {
        let dim = Style::new().dim();
        let body = frame.area().inner(Margin::new(1, 1));
        let [header, content, prompt, status, keys] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(if self.new_step.is_some() { 3 } else { 0 }),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .areas(body);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Workflow  ", Style::new().bold()),
                Span::styled(
                    format!(
                        "shared revision {}{}",
                        self.saved.revision,
                        if self.dirty() { " · edited" } else { "" }
                    ),
                    dim,
                ),
            ])),
            header,
        );
        let width = if content.width >= 82 {
            content.width / 3
        } else {
            0
        };
        let [source, preview] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(width)]).areas(content);
        if !source.is_empty() {
            frame.render_widget(&self.text, source);
            if self.new_step.is_none() && self.confirm.is_none() {
                crate::view::place_caret(frame, &self.text, source);
            }
        }
        if width > 0 {
            let mut lines = vec![Line::styled("Graph", Style::new().bold())];
            if let Ok((_, spec)) = self.spec() {
                let edges = spec.resolved_edges();
                for node in &spec.nodes {
                    let mut targets = if let Some(route) = spec.conditional_edges.get(&node.id) {
                        route
                            .map
                            .iter()
                            .map(|(key, to)| format!("{key} → {to}"))
                            .collect()
                    } else {
                        edges
                            .iter()
                            .filter(|(from, _)| from == &node.id)
                            .map(|(_, to)| to.clone())
                            .collect::<Vec<_>>()
                    };
                    if targets.is_empty() {
                        targets.push("END".into());
                    }
                    lines.push(Line::from(node.id.clone()));
                    lines.push(Line::styled(
                        format!("  {} → {}", node.agent, targets.join(", ")),
                        dim,
                    ));
                }
            } else {
                lines.push(Line::styled("Editing JSON…", dim));
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), preview);
        }
        if let Some(field) = &self.new_step {
            let [label, input] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(prompt);
            frame.render_widget(
                Paragraph::new("New step instruction · Enter add · Esc cancel"),
                label,
            );
            if !input.is_empty() {
                frame.render_widget(field, input);
                crate::view::place_caret(frame, field, input);
            }
        }
        let message = match self.confirm {
            Some(Confirm::Run) => {
                "Start this workflow in the session workspace? Enter run · Esc cancel"
            }
            Some(Confirm::Close) => "Discard unsaved edits? Enter discard · Esc keep editing",
            Some(Confirm::Reload) => {
                "Replace local edits with the saved revision? Enter reload · Esc keep editing"
            }
            None => &self.message,
        };
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), status);
        frame.render_widget(Paragraph::new("Ctrl+E example · Ctrl+N add step · Ctrl+S save · Ctrl+V validate · Ctrl+R run · Ctrl+L reload · Esc close").wrap(Wrap{trim:true}).style(dim),keys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_example_validates_and_saves_as_an_unstarted_draft() {
        let mut designer = Designer::open(
            DraftUpdate::default(),
            vec!["explore".into(), "general".into()],
        );
        let source = gewu_tools::workflow::spec::EXAMPLE;
        designer
            .import_reply(&format!("Draft\n```json\n{source}```"))
            .unwrap();
        assert_eq!(designer.checked().unwrap()["nodes"][0]["id"], "read");
        let Effect::Save(draft) = designer
            .key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .unwrap()
        else {
            panic!("expected a draft save");
        };
        assert_eq!(draft.revision, 0);
        assert_eq!(draft.spec["edges"][0], json!(["read", "summary"]));
    }
}
