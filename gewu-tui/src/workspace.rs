//! Multi-tab workspace: one tab per conversation session.
//!
//! The server pins one session per connection, so every tab that chats gets
//! its own lazily-created [`Client`] connection (managed by the event loop in
//! `main.rs`). This module holds only the pure, terminal-free state — tab
//! list, which tab is active, tab-bar layout and hit-testing — so it is unit
//! testable without a server.

use gewu_client::session::Block;
use gewu_protocol::Message;

use crate::tui::App;

/// Width of a `…` overflow marker, in columns.
pub const ELLIPSIS_WIDTH: usize = 1;
/// The thin `│` rule drawn between tabs (and before `+`).
pub const SEPARATOR: &str = "│";
/// Width of [`SEPARATOR`], in columns.
pub const SEPARATOR_WIDTH: usize = 1;
/// The `+` (new tab) button, rendered right after the last visible tab.
pub const NEW_BUTTON: &str = " + ";
/// Longest tab label before truncation.
const MAX_LABEL: usize = 16;

/// One tab: a conversation and its per-tab UI state.
#[derive(Debug)]
pub struct TabState {
    /// Stable id, unaffected by closing other tabs.
    pub id: u64,
    pub app: App,
}

/// What a left click on the tab bar row selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabClick {
    Nothing,
    /// Focus the tab at this index (into the current `tabs` vec).
    Switch(usize),
    /// Close (and delete from the store) the tab with this id.
    Close(u64),
    /// The `+` button: open a fresh session tab.
    New,
}

/// A clickable region of the tab bar, in tab-bar row columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub x0: usize,
    pub x1: usize,
    pub kind: HitKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Tab(usize),
    Close(u64),
    New,
}

/// Display metadata for one tab, as laid out for the tab bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabMeta {
    pub id: u64,
    /// Final label text (truncation and busy marker already applied).
    pub label: String,
}

/// The tab bar as rendered in the last frame: metadata plus hit regions.
#[derive(Debug, Clone, Default)]
pub struct TabStrip {
    pub metas: Vec<TabMeta>,
    pub active: usize,
    /// Inclusive range of tab indices drawn between (possible) ellipses.
    pub window: (usize, usize),
    pub hits: Vec<Hit>,
}

/// The set of open sessions.
#[derive(Debug, Default)]
pub struct Workspace {
    pub tabs: Vec<TabState>,
    pub active: usize,
    next_id: u64,
    /// Hit regions from the last `draw`; replaced on every frame.
    pub strip: TabStrip,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an empty tab (fresh server-side session on first ask) and
    /// focus it.
    pub fn open_blank(&mut self) -> u64 {
        let id = self.alloc_id();
        self.tabs.push(TabState {
            id,
            app: App::new(),
        });
        self.active = self.tabs.len() - 1;
        id
    }

    /// Append a tab replaying a stored session and focus it.
    pub fn open_session(&mut self, session_id: &str, messages: &[Message]) -> u64 {
        let id = self.open_blank();
        let tab = self.tab_mut(id).expect("just added");
        tab.app.model.load_history(messages);
        tab.app.set_session(Some(session_id.to_string()));
        let short = short_id(session_id);
        tab.app.note(format!(
            "resumed session {short} ({} stored message{})",
            messages.len(),
            if messages.len() == 1 { "" } else { "s" }
        ));
        id
    }

    /// Remove a tab by id, returning it. The active index follows the
    /// removal so the selection stays with the neighboring tab.
    pub fn close(&mut self, id: u64) -> Option<TabState> {
        let pos = self.tabs.iter().position(|t| t.id == id)?;
        let removed = self.tabs.remove(pos);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if pos < self.active {
            self.active -= 1;
        }
        Some(removed)
    }

    /// Look up a tab by its stable id.
    pub fn tab(&self, id: u64) -> Option<&TabState> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: u64) -> Option<&mut TabState> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// The focused tab.
    pub fn active_tab(&mut self) -> &mut TabState {
        let index = self.active.min(self.tabs.len().saturating_sub(1));
        &mut self.tabs[index]
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Focus the tab holding `session_id`, if one is open.
    pub fn focus_session(&mut self, session_id: &str) -> bool {
        if let Some(pos) = self
            .tabs
            .iter()
            .position(|t| t.app.model.session_id.as_deref() == Some(session_id))
        {
            self.active = pos;
            true
        } else {
            false
        }
    }

    pub fn switch_next(&mut self) {
        if self.tabs.len() > 1 {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    pub fn switch_prev(&mut self) {
        if self.tabs.len() > 1 {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn switch_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    /// Route a left click on the tab bar row using the last drawn strip.
    pub fn click(&self, column: u16, row: u16) -> TabClick {
        if row != 0 {
            return TabClick::Nothing;
        }
        let column = column as usize;
        for hit in &self.strip.hits {
            if column >= hit.x0 && column < hit.x1 {
                return match hit.kind {
                    HitKind::Tab(index) => TabClick::Switch(index),
                    HitKind::Close(id) => TabClick::Close(id),
                    HitKind::New => TabClick::New,
                };
            }
        }
        TabClick::Nothing
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// The tab's title: its first user prompt, falling back to the short session
/// id, then `new` for a tab that never sent anything.
pub fn tab_title(app: &App) -> String {
    if let Some(Block::User(text)) = app
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
    match &app.model.session_id {
        Some(id) => short_id(id),
        None => "new".to_string(),
    }
}

/// First eight characters of a session id, like the status line uses.
pub fn short_id(session_id: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    if session_id.chars().count() > 8 {
        format!("{short}…")
    } else {
        short
    }
}

/// Lay out the tab bar: fit every tab, or — when they do not fit — a window
/// of tabs around the active one, marked with `… ` ellipses. The `+` button
/// follows the last visible tab (OpenCode-style), never pinned to the edge.
pub fn layout_strip(metas: &[TabMeta], active: usize, width: u16) -> TabStrip {
    let width = width as usize;
    let plus_w = NEW_BUTTON.chars().count();
    let avail = width.saturating_sub(plus_w);
    // " label × " — padding, label, space, close mark, trailing space.
    let widths: Vec<usize> = metas.iter().map(|m| m.label.chars().count() + 4).collect();
    let n = metas.len();

    let span_used = |lo: usize, hi: usize| -> usize {
        widths[lo..=hi].iter().sum::<usize>()
            + (hi - lo + 1) * SEPARATOR_WIDTH
            + if lo > 0 { ELLIPSIS_WIDTH } else { 0 }
            + if hi + 1 < n { ELLIPSIS_WIDTH } else { 0 }
    };
    let (lo, hi) = if n == 0 {
        (0, 0)
    } else {
        let mut lo = active.min(n - 1);
        let mut hi = lo;
        loop {
            let mut improved = false;
            if lo > 0 && span_used(lo - 1, hi) <= avail {
                lo -= 1;
                improved = true;
            }
            if hi + 1 < n && span_used(lo, hi + 1) <= avail {
                hi += 1;
                improved = true;
            }
            if !improved {
                break;
            }
        }
        (lo, hi)
    };

    let mut hits = Vec::new();
    let mut x = 0usize;
    if n > 0 {
        if lo > 0 {
            x += ELLIPSIS_WIDTH;
        }
        for i in lo..=hi {
            let label_w = metas[i].label.chars().count();
            let close_x0 = x + 1 + label_w + 1;
            hits.push(Hit {
                x0: x,
                x1: close_x0,
                kind: HitKind::Tab(i),
            });
            hits.push(Hit {
                x0: close_x0,
                x1: x + widths[i],
                kind: HitKind::Close(metas[i].id),
            });
            // The `│` rule after each tab is drawn but not clickable.
            x += widths[i] + SEPARATOR_WIDTH;
        }
        if hi + 1 < n {
            x += ELLIPSIS_WIDTH;
        }
    }
    // The `+` button sits directly after the last rendered tab (or the right
    // overflow marker); the window budget above reserved its width.
    if width >= plus_w && x + plus_w <= width {
        hits.push(Hit {
            x0: x,
            x1: x + plus_w,
            kind: HitKind::New,
        });
    }

    TabStrip {
        metas: metas.to_vec(),
        active,
        window: (lo, hi),
        hits,
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

    fn meta(id: u64, label: &str) -> TabMeta {
        TabMeta {
            id,
            label: label.to_string(),
        }
    }

    #[test]
    fn open_blank_focuses_new_tab() {
        let mut ws = Workspace::new();
        let first = ws.open_blank();
        let second = ws.open_blank();
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.active, 1);
        assert_ne!(first, second, "ids are unique");
    }

    #[test]
    fn open_session_replays_history_and_notes() {
        let mut ws = Workspace::new();
        let messages = vec![user_message("old question")];
        ws.open_session("abcdef123456", &messages);
        let tab = ws.active_tab();
        assert_eq!(tab.app.model.session_id.as_deref(), Some("abcdef123456"));
        assert_eq!(tab.app.model.blocks[0], Block::User("old question".into()));
        assert!(
            tab.app
                .model
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Info(text) if text.contains("abcdef12")))
        );
    }

    #[test]
    fn close_keeps_selection_with_neighbors() {
        let mut ws = Workspace::new();
        let a = ws.open_blank();
        let _b = ws.open_blank();
        let c = ws.open_blank();
        ws.switch_prev();
        assert_eq!(ws.active, 1);

        // Closing a tab left of the active one shifts the selection with it.
        ws.close(a);
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.active, 0);
        assert_eq!(ws.tabs[0].id, _b);

        // Closing the active (last) tab clamps to the new last.
        ws.close(c);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.active, 0);
        assert_eq!(ws.tabs[0].id, _b);

        assert!(ws.close(_b).is_some());
        assert!(ws.is_empty());
        assert!(ws.close(999).is_none(), "unknown id");
    }

    #[test]
    fn switching_wraps_and_clamps() {
        let mut ws = Workspace::new();
        ws.open_blank();
        ws.switch_next();
        assert_eq!(ws.active, 0, "single tab does not move");
        let _ = ws.open_blank();
        let _ = ws.open_blank();
        assert_eq!(ws.active, 2, "new tabs are focused");
        ws.switch_next();
        assert_eq!(ws.active, 0, "wraps to the first tab");
        ws.switch_prev();
        assert_eq!(ws.active, 2, "wraps to the last tab");
        ws.switch_to(0);
        assert_eq!(ws.active, 0);
        ws.switch_to(99);
        assert_eq!(ws.active, 0, "out-of-range is ignored");
    }

    #[test]
    fn focus_session_selects_the_right_tab() {
        let mut ws = Workspace::new();
        ws.open_session("aaa", &[]);
        ws.open_session("bbb", &[]);
        assert!(ws.focus_session("aaa"));
        assert_eq!(ws.active, 0);
        assert!(!ws.focus_session("missing"));
        assert_eq!(ws.active, 0);
    }

    #[test]
    fn title_prefers_first_prompt_then_session_then_new() {
        let mut app = App::new();
        assert_eq!(tab_title(&app), "new");
        app.set_session(Some("abcdef123456".into()));
        assert_eq!(tab_title(&app), "abcdef12…");
        app.model
            .blocks
            .push(Block::User("  hello   world  ".into()));
        assert_eq!(tab_title(&app), "hello world");

        app.model.blocks[0] = Block::User("abcdefghijklmnopqrstuvwxyz".into());
        assert_eq!(
            tab_title(&app),
            "abcdefghijklmnop…",
            "truncated to 16 chars"
        );
    }

    #[test]
    fn strip_fits_all_tabs_with_plus_button() {
        let metas = vec![meta(1, "alpha"), meta(2, "beta")];
        let strip = layout_strip(&metas, 0, 80);
        assert_eq!(strip.window, (0, 1));
        // Tab 0: cols 0..9 (" alpha × "), then `│` at 9; tab 1: 10..18, `│`
        // at 18. Tabs are `label + 4` wide plus one separator column.
        assert_eq!(
            strip.hits[0],
            Hit {
                x0: 0,
                x1: 7,
                kind: HitKind::Tab(0)
            }
        );
        assert_eq!(
            strip.hits[1],
            Hit {
                x0: 7,
                x1: 9,
                kind: HitKind::Close(1)
            }
        );
        assert_eq!(
            strip.hits[2],
            Hit {
                x0: 10,
                x1: 16,
                kind: HitKind::Tab(1)
            }
        );
        assert_eq!(
            strip.hits[3],
            Hit {
                x0: 16,
                x1: 18,
                kind: HitKind::Close(2)
            }
        );
        // `+` directly after the last tab's separator, not at the edge.
        assert_eq!(
            strip.hits[4],
            Hit {
                x0: 19,
                x1: 22,
                kind: HitKind::New
            }
        );
    }

    #[test]
    fn strip_windows_around_the_active_tab() {
        let metas: Vec<TabMeta> = (0..10).map(|i| meta(i, &format!("session-{i}"))).collect();
        let strip = layout_strip(&metas, 5, 80);
        let (lo, hi) = strip.window;
        assert!(lo <= 5 && hi >= 5, "active tab is visible: {strip:?}");
        assert!(lo > 0 && hi < 9, "hidden tabs are elided: {strip:?}");
        // The active tab's hit region exists.
        assert!(strip.hits.iter().any(|h| matches!(h.kind, HitKind::Tab(5))),);
        // `+` sits right after the right overflow marker and stays on-screen.
        let plus = strip.hits.last().expect("plus hit");
        assert_eq!(plus.kind, HitKind::New);
        assert!(plus.x0 > 0 && plus.x1 <= 80, "plus fits: {plus:?}");
    }

    #[test]
    fn strip_empty_bar_still_has_the_plus_button() {
        let strip = layout_strip(&[], 0, 80);
        assert_eq!(strip.window, (0, 0));
        assert_eq!(
            strip.hits,
            vec![Hit {
                x0: 0,
                x1: 3,
                kind: HitKind::New
            }]
        );
    }

    #[test]
    fn click_routes_tab_close_and_new() {
        let metas = vec![meta(1, "alpha"), meta(2, "beta")];
        let mut ws = Workspace::new();
        ws.open_blank();
        ws.open_blank();
        ws.strip = layout_strip(&metas, 0, 80);

        assert_eq!(ws.click(2, 0), TabClick::Switch(0));
        assert_eq!(ws.click(8, 0), TabClick::Close(1), "on the ×");
        assert_eq!(ws.click(9, 0), TabClick::Nothing, "on the │ rule");
        assert_eq!(ws.click(19, 0), TabClick::New);
        assert_eq!(ws.click(2, 5), TabClick::Nothing, "below the bar");
        assert_eq!(ws.click(76, 0), TabClick::Nothing, "past the bar");
    }
}
