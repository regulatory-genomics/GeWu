//! Topic channel implementation for FIFO queue behavior.

use serde::{Serialize, de::DeserializeOwned};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A FIFO (first-in-first-out) queue channel with a maximum size.
///
/// Messages are written to the back and read from the front. When the
/// queue exceeds `max_size`, the oldest message is dropped to make room.
/// This implements a sliding window over a stream of values.
pub struct TopicChannel<T: Clone + Send + Sync + Serialize + DeserializeOwned + 'static> {
    inner: Arc<RwLock<VecDeque<T>>>,
    max_size: usize,
}

impl<T: Clone + Send + Sync + Serialize + DeserializeOwned + 'static> TopicChannel<T> {
    /// Creates a new `TopicChannel` with the specified maximum size.
    ///
    /// # Arguments
    /// * `max_size` - Maximum number of messages to retain. When exceeded,
    ///   the oldest message is dropped.
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(VecDeque::new())),
            max_size,
        }
    }

    /// Reads the oldest message from the queue without consuming it.
    ///
    /// Returns `Some(value)` if the queue is non-empty, `None` otherwise.
    /// Multiple calls to `read()` without writing will return the same value.
    pub async fn read(&self) -> Option<T> {
        let guard = self.inner.read().await;
        guard.front().cloned()
    }

    /// Writes a message to the back of the queue.
    ///
    /// If the queue is at capacity, the oldest message is dropped.
    pub async fn write(&self, value: T) {
        let mut guard = self.inner.write().await;
        guard.push_back(value);
        while guard.len() > self.max_size {
            guard.pop_front();
        }
    }
}

impl<T: Clone + Send + Sync + Serialize + DeserializeOwned + 'static> Default for TopicChannel<T> {
    fn default() -> Self {
        Self::new(100)
    }
}
