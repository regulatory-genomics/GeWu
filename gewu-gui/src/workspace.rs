//! Tab state for the web GUI.
//!
//! One tab per conversation session. The conversation itself lives in
//! `gewu_client::session::SessionModel`; this module adds only the tab list
//! and the per-tab view state.

use std::collections::HashSet;

use gewu_client::session::{Block, Phase, SessionModel};
use gewu_protocol::{Message, TokenUsage};

use crate::protocol::TabInfo;

/// Longest tab label before truncation.
const MAX_LABEL: usize = 16;

/// One tab: a conversation plus its view state.
#[derive(Debug)]
pub struct Tab {
    pub agent: String,
    /// Stable id, unaffected by closing other tabs.
    pub id: u64,
    pub model: SessionModel,
    /// Server model name, shown in the status line.
    pub model_label: Option<String>,
    /// The tab's model selector (`endpoint/model`) into the server's model
    /// catalog; `None` until a snapshot reveals it (means "server default").
    pub model_selector: Option<String>,
    /// Context usage `(used tokens, context limit)`.
    pub context: Option<(u64, u64)>,
    /// Provider-reported token accounting accumulated across the session,
    /// from which the status line derives the cache hit rate.
    pub cache: Option<TokenUsage>,
    /// Tool runs the user expanded (keyed by the run's first block index).
    pub expanded_groups: HashSet<usize>,
    /// Workflow tool runs the user collapsed; workflow runs default to
    /// expanded, so collapse is the explicit state worth remembering.
    pub collapsed_groups: HashSet<usize>,
    /// Tool calls whose full output the user expanded (block index).
    pub expanded_outputs: HashSet<usize>,
    /// Thought blocks whose reasoning text the user expanded (block index).
    pub expanded_thoughts: HashSet<usize>,
    /// Steered messages displayed at submit time and not yet confirmed by
    /// the agent's `steered` event: `(text, block index)` in submit order.
    pub pending_steers: Vec<(String, usize)>,
}

impl Tab {
    fn new(id: u64) -> Self {
        Self {
            agent: "build".into(),
            id,
            model: SessionModel::new(),
            model_label: None,
            model_selector: None,
            context: None,
            cache: None,
            expanded_groups: HashSet::new(),
            collapsed_groups: HashSet::new(),
            expanded_outputs: HashSet::new(),
            expanded_thoughts: HashSet::new(),
            pending_steers: Vec::new(),
        }
    }

    pub fn busy(&self) -> bool {
        self.model.phase == Phase::Busy
    }

    pub fn info(&self) -> TabInfo {
        TabInfo {
            id: self.id,
            label: tab_title(self),
            busy: self.busy(),
            session_id: self.model.session_id.clone(),
        }
    }
}

/// The set of open sessions.
#[derive(Debug, Default)]
pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_id: u64,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an empty tab and focus it.
    pub fn open_blank(&mut self) -> u64 {
        let id = self.alloc_id();
        self.tabs.push(Tab::new(id));
        self.active = self.tabs.len() - 1;
        id
    }

    /// Append a tab replaying a stored session and focus it.
    pub fn open_session(&mut self, session_id: &str, messages: &[Message]) -> u64 {
        let id = self.open_blank();
        let tab = self.tab_mut(id).expect("just added");
        tab.model.load_history(messages);
        tab.model.set_session(Some(session_id.to_string()));
        tab.model.note(format!(
            "resumed session {} ({} stored message{})",
            short_id(session_id),
            messages.len(),
            if messages.len() == 1 { "" } else { "s" }
        ));
        id
    }

    /// Remove a tab by id, returning it. The active index follows the removal.
    pub fn close(&mut self, id: u64) -> Option<Tab> {
        let pos = self.tabs.iter().position(|t| t.id == id)?;
        let removed = self.tabs.remove(pos);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if pos < self.active {
            self.active -= 1;
        }
        Some(removed)
    }

    pub fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// The focused tab. Callers only invoke this on a non-empty workspace.
    pub fn active_tab(&mut self) -> &mut Tab {
        let index = self.active.min(self.tabs.len().saturating_sub(1));
        &mut self.tabs[index]
    }

    pub fn active_id(&self) -> u64 {
        self.tabs[self.active.min(self.tabs.len().saturating_sub(1))].id
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Focus a tab by its stable id. Returns whether it existed.
    pub fn switch_to_id(&mut self, id: u64) -> bool {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            self.active = pos;
            true
        } else {
            false
        }
    }

    pub fn infos(&self) -> Vec<TabInfo> {
        self.tabs.iter().map(Tab::info).collect()
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// The tab's title: its first user prompt, falling back to the short session
/// id, then `new` for a tab that never sent anything.
pub fn tab_title(tab: &Tab) -> String {
    if let Some(Block::User(text)) = tab
        .model
        .blocks
        .iter()
        .find(|b| matches!(b, Block::User(_)))
    {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut label: String = flat.chars().take(MAX_LABEL).collect();
        if flat.chars().count() > MAX_LABEL {
            label.push('…');
        }
        return label;
    }
    match &tab.model.session_id {
        Some(id) => short_id(id),
        None => "new".to_string(),
    }
}

/// First eight characters of a session id.
pub fn short_id(session_id: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    if session_id.chars().count() > 8 {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_protocol::ContentPart;

    fn user_message(text: &str) -> Message {
        Message::User {
            content: vec![ContentPart::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn open_blank_focuses_new_tab() {
        let mut ws = Workspace::new();
        let first = ws.open_blank();
        let second = ws.open_blank();
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.active, 1);
        assert_eq!(ws.active_id(), second);
        assert_ne!(first, second);
    }

    #[test]
    fn open_session_replays_history_and_notes() {
        let mut ws = Workspace::new();
        let id = ws.open_session("abcdef123456", &[user_message("old question")]);
        let tab = ws.tab(id).unwrap();
        assert_eq!(tab.model.session_id.as_deref(), Some("abcdef123456"));
        assert_eq!(tab.model.blocks[0], Block::User("old question".into()));
        assert!(
            tab.model
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Info(text) if text.contains("abcdef12")))
        );
    }

    #[test]
    fn close_keeps_selection_with_neighbors() {
        let mut ws = Workspace::new();
        let a = ws.open_blank();
        let b = ws.open_blank();
        let c = ws.open_blank();
        assert_eq!(ws.active_id(), c);

        ws.close(a);
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.active_id(), c);

        ws.close(c);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.active_id(), b);

        assert!(ws.close(b).is_some());
        assert!(ws.is_empty());
        assert!(ws.close(999).is_none());
    }

    #[test]
    fn switches_by_stable_id() {
        let mut ws = Workspace::new();
        let a = ws.open_blank();
        let b = ws.open_blank();
        assert!(ws.switch_to_id(a));
        assert_eq!(ws.active_id(), a);
        assert!(!ws.switch_to_id(999));
        assert_eq!(ws.active_id(), a);
        assert!(ws.switch_to_id(b));
    }

    #[test]
    fn title_prefers_first_prompt_then_session_then_new() {
        let mut ws = Workspace::new();
        let id = ws.open_blank();
        assert_eq!(tab_title(ws.tab(id).unwrap()), "new");

        ws.tab_mut(id)
            .unwrap()
            .model
            .set_session(Some("abcdef123456".into()));
        assert_eq!(tab_title(ws.tab(id).unwrap()), "abcdef12…");

        ws.tab_mut(id)
            .unwrap()
            .model
            .blocks
            .push(Block::User("  hello   world  ".into()));
        assert_eq!(tab_title(ws.tab(id).unwrap()), "hello world");
    }

    #[test]
    fn tab_info_reports_busy_and_session() {
        let mut ws = Workspace::new();
        let id = ws.open_blank();
        let tab = ws.tab_mut(id).unwrap();
        tab.model.set_session(Some("s1".into()));
        tab.model.submit("hi".into());
        let info = tab.info();
        assert_eq!(info.id, id);
        assert!(info.busy);
        assert_eq!(info.session_id.as_deref(), Some("s1"));
    }
}
