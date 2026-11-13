use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundNotification {
    pub source: String,
    pub id: String,
    pub status: String,
    pub output: String,
}

impl BackgroundNotification {
    pub fn text(&self) -> String {
        format!(
            "Background {} {}: {}\n{}",
            self.source, self.id, self.status, self.output
        )
    }
}

#[derive(Clone, Debug)]
pub enum InboxMessage {
    User(String),
    Background(BackgroundNotification),
}

#[derive(Clone)]
pub struct Steering {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Queue {
    users: VecDeque<String>,
    background: VecDeque<BackgroundNotification>,
    paused: bool,
}

struct Inner {
    queue: Mutex<Queue>,
    wake: Notify,
    watched: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Steering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Steering")
            .field("pending", &self.is_pending())
            .finish()
    }
}

impl Default for Steering {
    fn default() -> Self {
        Self::new()
    }
}

impl Steering {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(Queue::default()),
                wake: Notify::new(),
                watched: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub fn send(&self, message: impl Into<String>) -> Result<(), mpsc::error::SendError<String>> {
        let mut queue = self.inner.queue.lock().unwrap();
        queue.users.push_back(message.into());
        queue.paused = false;
        self.wake();
        Ok(())
    }

    pub fn notify(&self, mut notification: BackgroundNotification) {
        const MAX_OUTPUT: usize = 16 * 1024;
        if notification.output.len() > MAX_OUTPUT {
            let mut end = MAX_OUTPUT;
            while !notification.output.is_char_boundary(end) {
                end -= 1;
            }
            notification.output.truncate(end);
            notification
                .output
                .push_str("\n[truncated; use workflow_status for full details]");
        }
        let mut queue = self.inner.queue.lock().unwrap();
        if let Some(existing) = queue
            .background
            .iter_mut()
            .find(|entry| entry.source == notification.source && entry.id == notification.id)
        {
            *existing = notification;
            self.wake();
            return;
        }
        if queue.background.len() >= 32 {
            queue.background.pop_front();
        }
        queue.background.push_back(notification);
        self.wake();
    }

    pub fn drain_messages(&self) -> Vec<InboxMessage> {
        let mut queue = self.inner.queue.lock().unwrap();
        let mut messages: Vec<_> = queue.users.drain(..).map(InboxMessage::User).collect();
        messages.extend(queue.background.drain(..).map(InboxMessage::Background));
        messages
    }

    pub fn drain(&self) -> Vec<String> {
        self.drain_messages()
            .into_iter()
            .map(|message| match message {
                InboxMessage::User(text) => text,
                InboxMessage::Background(notification) => notification.text(),
            })
            .collect()
    }

    pub fn is_pending(&self) -> bool {
        let queue = self.inner.queue.lock().unwrap();
        !queue.users.is_empty() || !queue.background.is_empty()
    }

    pub fn should_resume(&self) -> bool {
        let queue = self.inner.queue.lock().unwrap();
        !queue.paused && (!queue.users.is_empty() || !queue.background.is_empty())
    }

    pub fn pause(&self) {
        let mut queue = self.inner.queue.lock().unwrap();
        queue.users.clear();
        queue.paused = true;
    }

    pub fn unpause(&self) {
        self.inner.queue.lock().unwrap().paused = false;
    }

    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    pub async fn notified(&self) {
        self.inner.wake.notified().await;
    }

    pub fn claim_watcher(&self) -> bool {
        !self
            .inner
            .watched
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_drain_and_is_pending() {
        let steering = Steering::new();
        assert!(!steering.is_pending());
        steering.send("first").unwrap();
        steering.send("second").unwrap();
        assert!(steering.is_pending());
        assert_eq!(steering.drain(), vec!["first", "second"]);
        assert!(!steering.is_pending());
        assert!(steering.drain().is_empty());
    }

    #[test]
    fn clones_share_one_buffer() {
        let steering = Steering::new();
        steering.clone().send("via clone").unwrap();
        assert_eq!(steering.drain(), vec!["via clone"]);
    }

    #[test]
    fn background_is_separate_bounded_and_session_local() {
        let inbox = Steering::new();
        let other = Steering::new();
        inbox.notify(BackgroundNotification {
            source: "workflow".into(),
            id: "run".into(),
            status: "completed".into(),
            output: "界".repeat(10000),
        });
        inbox.send("user").unwrap();
        let messages = inbox.drain_messages();
        assert!(matches!(&messages[0], InboxMessage::User(text) if text == "user"));
        assert!(
            matches!(&messages[1], InboxMessage::Background(n) if n.output.len() < 17000 && n.output.contains("truncated"))
        );
        assert!(!other.is_pending());
    }

    #[tokio::test]
    async fn wakes_coalesce_and_pause_retains_background() {
        let inbox = Steering::new();
        inbox.send("one").unwrap();
        inbox.send("two").unwrap();
        inbox.notified().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), inbox.notified())
                .await
                .is_err()
        );
        inbox.pause();
        inbox.notify(BackgroundNotification {
            source: "workflow".into(),
            id: "paused".into(),
            status: "completed".into(),
            output: "done".into(),
        });
        assert!(!inbox.should_resume());
        assert!(inbox.is_pending());
        assert_eq!(inbox.drain_messages().len(), 1);
        inbox.send("three").unwrap();
        assert!(inbox.should_resume());
        assert_eq!(inbox.drain(), vec!["three"]);
    }
}
