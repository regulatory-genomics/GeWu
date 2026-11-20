//! Conversation-turn-based mode reminders, independent of tool-call rounds.

use crate::types::{ContentPart, Message};
use serde::{Deserialize, Serialize};

/// Full instructions on entry, sparse reminders every five user turns, and
/// full refreshes every twenty-five turns. A missing sparse reminder makes
/// this a one-shot notice (for example, when leaving plan mode). Applied
/// reminders are appended to conversation history so they stay visible to
/// every later provider request.
///
/// The state is serializable so the server can persist it per session and
/// restore it across reverts and reconnects.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Reminder {
    full: String,
    sparse: Option<String>,
    user_turns: usize,
    pending: Option<String>,
}

impl Reminder {
    /// Create a new mode reminder, resetting the conversation-turn cadence.
    pub fn switch(full: impl Into<String>, sparse: Option<String>) -> Self {
        let full = full.into();
        Self {
            pending: Some(full.clone()),
            full,
            sparse,
            user_turns: 0,
        }
    }

    /// Rebuild state for history that already carries a reminder: the
    /// cadence continues from `user_turns` and nothing is pending.
    pub fn continue_at(full: impl Into<String>, sparse: Option<String>, user_turns: usize) -> Self {
        Self {
            full: full.into(),
            sparse,
            user_turns,
            pending: None,
        }
    }

    /// The configured full and sparse reminder texts.
    pub fn texts(&self) -> (String, Option<String>) {
        (self.full.clone(), self.sparse.clone())
    }

    /// The reminder waiting to be applied, if any.
    pub fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// How many user turns this reminder has counted.
    pub fn user_turns(&self) -> usize {
        self.user_turns
    }

    /// Whether any user message in `messages` carries a reminder part.
    pub fn in_history(messages: &[Message]) -> bool {
        messages.iter().any(|message| {
            matches!(message, Message::User { content }
                if content.iter().any(|part| matches!(part, ContentPart::Reminder { .. })))
        })
    }

    /// Advance once per user prompt, never once per provider/tool iteration.
    pub fn on_user_turn(&mut self) {
        self.user_turns = self.user_turns.saturating_add(1);
        if self.pending.is_none()
            && self.user_turns.is_multiple_of(5)
            && let Some(sparse) = &self.sparse
        {
            self.pending = Some(if self.user_turns.is_multiple_of(25) {
                self.full.clone()
            } else {
                sparse.clone()
            });
        }
    }

    /// Append a due reminder to a user message. The reminder becomes part of
    /// the message before it is stored, so every later provider request —
    /// including further tool rounds within the same turn — still carries it.
    /// Non-user messages are left untouched and keep the reminder pending for
    /// the next user turn.
    pub fn apply_to(&mut self, message: &mut Message) {
        let Message::User { content } = message else {
            return;
        };
        if let Some(text) = self.pending.take() {
            content.push(ContentPart::Reminder { text });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Message {
        Message::User { content: vec![] }
    }

    #[test]
    fn cadence_applies_to_the_turn_message_and_persists() {
        let mut reminder = Reminder::switch("full", Some("sparse".into()));
        let mut history: Vec<Message> = Vec::new();
        for turn in 1..=50 {
            reminder.on_user_turn();
            let mut message = user();
            reminder.apply_to(&mut message);
            // A second application in the same turn (the next tool round)
            // must not duplicate the reminder: pending is consumed on first
            // application.
            reminder.apply_to(&mut message);
            let expected = if turn == 1 || turn % 25 == 0 {
                Some("full")
            } else if turn % 5 == 0 {
                Some("sparse")
            } else {
                None
            };
            let Message::User { content } = &message else {
                unreachable!()
            };
            assert_eq!(
                content.first(),
                expected
                    .map(|text| ContentPart::Reminder { text: text.into() })
                    .as_ref(),
                "turn {turn}"
            );
            history.push(message);
            // Applied reminders stay in history for later requests: the
            // turn-1 switch reminder must still be present in later turns.
            if turn > 1 {
                let Message::User { content } = &history[0] else {
                    unreachable!()
                };
                assert!(!content.is_empty(), "switch reminder lost by turn {turn}");
            }
        }
    }

    #[test]
    fn non_user_messages_keep_the_reminder_pending() {
        let mut reminder = Reminder::switch("full", None);
        reminder.on_user_turn();
        let mut system = Message::System {
            content: "prompt".into(),
        };
        reminder.apply_to(&mut system);
        assert_eq!(reminder.pending(), Some("full"));
        let mut message = user();
        reminder.apply_to(&mut message);
        assert_eq!(reminder.pending(), None);
        assert_ne!(message, user());
        // With no sparse reminder configured, later turns stay silent.
        for _ in 0..30 {
            reminder.on_user_turn();
            let mut message = user();
            reminder.apply_to(&mut message);
            assert_eq!(message, user());
        }
    }

    #[test]
    fn default_is_silent() {
        let mut reminder = Reminder::default();
        for _ in 0..30 {
            reminder.on_user_turn();
            let mut message = user();
            reminder.apply_to(&mut message);
            assert_eq!(message, user());
        }
    }
}
