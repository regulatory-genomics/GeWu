//! TUI application state: pure logic with no terminal I/O, so behavior is
//! unit-testable. Rendering lives in [`crate::tui_view`].
//!
//! The conversation itself (transcript blocks, busy/idle phase, session id)
//! lives in [`crate::session::SessionModel`], shared with graphical clients.
//! [`App`] adds the terminal-specific input box and scroll state on top.

use std::collections::HashSet;

use gewu_protocol::AgentEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

pub use gewu_client::session::{Block, Phase, SessionModel};

/// What clicking a transcript line toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// A run of consecutive tool calls, keyed by the run's first block index.
    Group(usize),
    /// One tool call's full output, keyed by its block index.
    Output(usize),
}

/// A cached, width-wrapped Markdown render of one transcript block.
#[derive(Debug, Clone)]
pub struct RenderedBlock {
    /// Hash of the source text, to detect a block being rewritten in place.
    pub hash: u64,
    /// Width (in columns) the block was wrapped to.
    pub width: u16,
    /// Rendered lines, ready to append to the transcript.
    pub lines: Vec<Line<'static>>,
}

/// Terminal UI state for the chat client.
#[derive(Debug)]
pub struct App {
    /// Conversation state shared with other front-ends.
    pub model: SessionModel,
    /// Server model as `provider/model`, shown in the status line.
    pub model_label: Option<String>,
    /// Context usage `(used tokens, context limit)` for the current session,
    /// shown right-aligned in the status line. `None` hides it.
    pub context: Option<(usize, usize)>,
    pub input: String,
    /// Cursor position (in chars) within `input`.
    pub cursor: usize,
    /// Scroll offset from the bottom of the transcript (0 = follow).
    pub scroll: usize,
    /// Follow new output by keeping the transcript scrolled to the end.
    pub follow: bool,
    /// Rendered Markdown, indexed by transcript block. Invalidated by width
    /// changes (stored per entry) and by transcript replacement.
    pub rendered: Vec<Option<RenderedBlock>>,
    /// Tool runs the user expanded (keyed by the run's first block index).
    pub expanded_groups: HashSet<usize>,
    /// Tool calls whose full output the user expanded (block index).
    pub expanded_outputs: HashSet<usize>,
    /// Clickable transcript lines from the last frame (absolute line index →
    /// toggle target). Rebuilt on every draw.
    pub click_map: Vec<Option<Toggle>>,
    /// Screen area of the transcript widget in the last frame.
    pub transcript_area: Rect,
    /// Absolute index of the first transcript line shown in the last frame.
    pub viewport_top: usize,
    /// Line to keep at the same screen row across the next draw, set by a
    /// click that changes the transcript height: `(absolute line, row)`.
    pub anchor: Option<(usize, u16)>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            model: SessionModel::new(),
            model_label: None,
            context: None,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow: true,
            rendered: Vec::new(),
            expanded_groups: HashSet::new(),
            expanded_outputs: HashSet::new(),
            click_map: Vec::new(),
            transcript_area: Rect::default(),
            viewport_top: 0,
            anchor: None,
        }
    }

    // -- input editing ------------------------------------------------------

    /// Insert a character at the cursor.
    pub fn push_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Remove the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    /// Remove the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.input.chars().count() {
            self.input.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    // -- transcript ---------------------------------------------------------

    /// Apply one decoded agent event to the transcript.
    pub fn apply_event(&mut self, event: &AgentEvent) {
        self.model.apply_event(event);
    }

    /// Record a user prompt, reset the input box, and enter the busy phase.
    pub fn submit(&mut self, prompt: String) {
        self.model.submit(prompt);
        self.input.clear();
        self.cursor = 0;
        self.follow = true;
    }

    /// An ask finished (success or terminal error).
    pub fn finish_ask(&mut self) {
        self.model.finish_ask();
    }

    pub fn set_session(&mut self, session_id: Option<String>) {
        self.model.set_session(session_id);
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.model.note(message);
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.model.fail(message);
    }

    // -- scrolling ----------------------------------------------------------

    pub fn scroll_up(&mut self, amount: usize) {
        self.follow = false;
        self.scroll = self.scroll.saturating_add(amount);
    }

    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        if self.scroll == 0 {
            self.follow = true;
        }
    }

    /// Jump to the top of the transcript, unpinning follow.
    ///
    /// The exact offset depends on the rendered line count, which only the
    /// view knows, so this uses a saturating maximum and lets the view clamp
    /// it to the real number of hidden lines on the next draw.
    pub fn scroll_to_top(&mut self) {
        self.follow = false;
        self.scroll = usize::MAX;
    }

    /// Jump back to the newest output and resume following.
    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
        self.scroll = 0;
    }

    // -- tool disclosure ----------------------------------------------------

    /// Handle a left click at terminal coordinates `(column, row)`.
    ///
    /// Clicks inside the transcript toggle the tool output or tool run drawn
    /// on that line. Clicks elsewhere (input box, status line, border) are
    /// ignored. Returns `true` when something was toggled, so callers can
    /// avoid treating the click as a selection.
    pub fn click_at(&mut self, column: u16, row: u16) -> bool {
        let area = self.transcript_area;
        if area.width < 2 || area.height < 2 {
            return false;
        }
        let content = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width - 2,
            height: area.height - 2,
        };
        if column < content.x
            || column >= content.right()
            || row < content.y
            || row >= content.bottom()
        {
            return false;
        }

        let index = self.viewport_top + (row - content.y) as usize;
        let target = self.click_map.get(index).copied().flatten();
        match target {
            Some(Toggle::Group(first)) => toggle(&mut self.expanded_groups, first),
            Some(Toggle::Output(block)) => toggle(&mut self.expanded_outputs, block),
            None => return false,
        }
        // Expansion only adds lines below the clicked row, so pinning that
        // line keeps a scrolled-up viewport from jumping.
        if !self.follow {
            self.anchor = Some((index, row - content.y));
        }
        true
    }

    /// The prompt to send, if the input is non-empty.
    pub fn take_prompt(&mut self) -> Option<String> {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            None
        } else {
            Some(prompt)
        }
    }
}

/// Insert `value` into `set` if absent, remove it otherwise.
fn toggle(set: &mut HashSet<usize>, value: usize) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new()
    }

    #[test]
    fn input_editing_basics() {
        let mut a = app();
        a.push_char('h');
        a.push_char('i');
        assert_eq!(a.input, "hi");
        assert_eq!(a.cursor, 2);
        a.move_left();
        a.push_char('!');
        assert_eq!(a.input, "h!i");
        a.backspace();
        assert_eq!(a.input, "hi");
        a.move_end();
        a.delete();
        assert_eq!(a.input, "hi");
        a.move_home();
        a.delete();
        assert_eq!(a.input, "i");
    }

    #[test]
    fn submit_clears_input_and_sets_busy() {
        let mut a = app();
        a.push_char('h');
        a.push_char('i');
        let prompt = a.take_prompt().unwrap();
        a.submit(prompt);
        assert_eq!(a.model.blocks.len(), 1);
        assert_eq!(a.model.blocks[0], Block::User("hi".into()));
        assert_eq!(a.model.phase, Phase::Busy);
        assert_eq!(a.input, "");
    }

    #[test]
    fn take_prompt_rejects_blank_input() {
        let mut a = app();
        a.push_char(' ');
        assert!(a.take_prompt().is_none());
    }

    #[test]
    fn events_during_busy_then_finish() {
        let mut a = app();
        a.submit("hi".into());
        a.apply_event(&AgentEvent::ContentDelta {
            delta: "hello".into(),
        });
        a.finish_ask();
        assert_eq!(a.model.phase, Phase::Idle);
    }

    #[test]
    fn scroll_follow_semantics() {
        let mut a = app();
        assert!(a.follow);
        a.scroll_up(5);
        assert_eq!(a.scroll, 5);
        assert!(!a.follow);
        a.scroll_down(3);
        assert_eq!(a.scroll, 2);
        assert!(!a.follow);
        a.scroll_down(10);
        assert_eq!(a.scroll, 0);
        assert!(a.follow);
    }

    #[test]
    fn scroll_jumps_to_top_and_bottom() {
        let mut a = app();
        a.scroll_to_top();
        assert!(!a.follow);
        assert_eq!(a.scroll, usize::MAX);
        a.scroll_to_bottom();
        assert!(a.follow);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn scrolling_works_while_busy() {
        let mut a = app();
        a.submit("hi".into());
        assert_eq!(a.model.phase, Phase::Busy);
        a.scroll_up(3);
        assert_eq!(a.scroll, 3);
        assert!(!a.follow);
        a.scroll_to_bottom();
        assert!(a.follow);
    }

    #[test]
    fn session_only_set_when_present() {
        let mut a = app();
        a.set_session(None);
        assert_eq!(a.model.session_id, None);
        a.set_session(Some("abc".into()));
        assert_eq!(a.model.session_id.as_deref(), Some("abc"));
        a.set_session(None);
        assert_eq!(a.model.session_id.as_deref(), Some("abc"));
    }
}
