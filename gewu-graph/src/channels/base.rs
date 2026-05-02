//! Base channel trait and implementations.
//!
//! Provides the core channel types for inter-node communication.

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Base trait for all channel types.
///
/// All channels provide async read/write operations and can report
/// whether they contain values. This trait is implemented by all
/// built-in channel types.
#[async_trait]
pub trait BaseChannel: Send + Sync {
    /// Reads and consumes a value from the channel.
    ///
    /// Returns the value if present, or `serde_json::Value::Null` if empty.
    async fn read(&self) -> serde_json::Value;

    /// Writes a value to the channel.
    ///
    /// Returns `true` if the value was successfully written and accepted,
    /// `false` if the value was rejected (e.g., type mismatch).
    async fn write(&self, value: serde_json::Value) -> bool;

    /// Returns whether the channel contains values.
    ///
    /// Note: This may not accurately reflect internal state in all implementations.
    fn is_empty(&self) -> bool;
}

/// A channel that stores the most recently written value.
///
/// Each write overwrites the previous value. Reading does not consume the value,
/// so subsequent reads will return the same value until a new write occurs.
pub struct LastValueChannel<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> {
    value: Arc<RwLock<Option<T>>>,
}

impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> LastValueChannel<T> {
    /// Creates a new empty `LastValueChannel`.
    pub fn new() -> Self {
        Self {
            value: Arc::new(RwLock::new(None)),
        }
    }
}

impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> Default
    for LastValueChannel<T>
{
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> BaseChannel
    for LastValueChannel<T>
{
    async fn read(&self) -> serde_json::Value {
        let guard = self.value.read().await;
        match guard.as_ref() {
            Some(v) => serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        }
    }

    async fn write(&self, value: serde_json::Value) -> bool {
        if let Ok(t) = serde_json::from_value::<T>(value) {
            let mut guard = self.value.write().await;
            *guard = Some(t);
            true
        } else {
            false
        }
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// A channel that allows only a single read before returning Null.
///
/// After the first read, the internal value is consumed and subsequent
/// reads will return `serde_json::Value::Null` until a new value is written.
pub struct EphemeralValueChannel<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> {
    value: Arc<RwLock<Option<T>>>,
}

impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> EphemeralValueChannel<T> {
    /// Creates a new empty `EphemeralValueChannel`.
    pub fn new() -> Self {
        Self {
            value: Arc::new(RwLock::new(None)),
        }
    }
}

impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> Default
    for EphemeralValueChannel<T>
{
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> BaseChannel
    for EphemeralValueChannel<T>
{
    async fn read(&self) -> serde_json::Value {
        let mut guard = self.value.write().await;
        let value = guard.take();
        match value {
            Some(v) => serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        }
    }

    async fn write(&self, value: serde_json::Value) -> bool {
        if let Ok(t) = serde_json::from_value::<T>(value) {
            let mut guard = self.value.write().await;
            *guard = Some(t);
            true
        } else {
            false
        }
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// A channel that combines values using a binary operator.
///
/// Each write combines the new value with the existing value using the
/// provided operator. For example, with the sum operator, writes of
/// 1, 2, 3 would result in a value of 6.
pub struct BinaryOperatorChannel<T: Clone + Send + Sync + 'static> {
    value: Arc<RwLock<Option<T>>>,
    op: Arc<dyn Fn(T, T) -> T + Send + Sync>,
}

impl<T: Clone + Send + Sync + 'static> BinaryOperatorChannel<T> {
    /// Creates a new channel with the given binary operator.
    ///
    /// The operator is called as `op(existing_value, new_value)`.
    pub fn new<F>(op: F) -> Self
    where
        F: Fn(T, T) -> T + Send + Sync + 'static,
    {
        Self {
            value: Arc::new(RwLock::new(None)),
            op: Arc::new(op),
        }
    }

    /// Creates a channel that sums numeric values.
    ///
    /// Requires `T` to implement `std::ops::Add`.
    pub fn sum() -> Self
    where
        T: std::ops::Add<Output = T>,
    {
        Self::new(|a, b| a + b)
    }
}

impl<T: Clone + Send + Sync + 'static> Default for BinaryOperatorChannel<T> {
    fn default() -> Self {
        Self::new(|_, b| b)
    }
}

#[async_trait]
impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> BaseChannel
    for BinaryOperatorChannel<T>
{
    async fn read(&self) -> serde_json::Value {
        let guard = self.value.read().await;
        match guard.as_ref() {
            Some(v) => serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        }
    }

    async fn write(&self, value: serde_json::Value) -> bool {
        if let Ok(t) = serde_json::from_value::<T>(value) {
            let mut guard = self.value.write().await;
            match guard.take() {
                Some(existing) => {
                    let op = self.op.clone();
                    *guard = Some(op(existing, t));
                }
                None => {
                    *guard = Some(t);
                }
            }
            true
        } else {
            false
        }
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// A channel where the first write wins, ignoring subsequent writes.
///
/// Once a value is written, it persists until explicitly cleared.
/// Subsequent write attempts return `true` but do not update the stored value.
pub struct AnyValueChannel<T: Clone + Send + Sync + 'static> {
    value: Arc<RwLock<Option<T>>>,
    first_wins: Arc<RwLock<bool>>,
}

impl<T: Clone + Send + Sync + 'static> AnyValueChannel<T> {
    /// Creates a new empty `AnyValueChannel`.
    pub fn new() -> Self {
        Self {
            value: Arc::new(RwLock::new(None)),
            first_wins: Arc::new(RwLock::new(false)),
        }
    }

    /// Clears the channel, allowing the next write to be accepted.
    ///
    /// Note: Currently a no-op; the channel cannot be reset once written.
    pub fn clear(&self) {
        // Reset for next use
    }
}

impl<T: Clone + Send + Sync + 'static> Default for AnyValueChannel<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> BaseChannel
    for AnyValueChannel<T>
{
    async fn read(&self) -> serde_json::Value {
        let guard = self.value.read().await;
        match guard.as_ref() {
            Some(v) => serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        }
    }

    async fn write(&self, value: serde_json::Value) -> bool {
        let first_wins = self.first_wins.read().await;
        if *first_wins {
            return true;
        }
        drop(first_wins);

        if let Ok(t) = serde_json::from_value::<T>(value) {
            let mut guard = self.value.write().await;
            let mut first_wins = self.first_wins.write().await;
            if *first_wins {
                return true;
            }
            *guard = Some(t);
            *first_wins = true;
            true
        } else {
            false
        }
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// A synchronization barrier that waits for all named participants.
///
/// Callers register themselves using `wait_for()`, and the method returns
/// `true` only when all expected participants have checked in.
pub struct NamedBarrierChannel {
    waiters: Arc<RwLock<Vec<String>>>,
    expected: Arc<RwLock<HashSet<String>>>,
}

impl NamedBarrierChannel {
    /// Creates a new barrier that expects the given participant names.
    pub fn new(expected: Vec<String>) -> Self {
        Self {
            waiters: Arc::new(RwLock::new(Vec::new())),
            expected: Arc::new(RwLock::new(expected.into_iter().collect())),
        }
    }

    /// Registers a participant and checks if all expected participants have checked in.
    ///
    /// Returns `true` if all expected participants have called `wait_for()`,
    /// `false` otherwise. Participants can call this multiple times.
    pub async fn wait_for(&self, name: &str) -> bool {
        let mut waiters = self.waiters.write().await;
        waiters.push(name.to_string());

        drop(waiters);

        let expected = self.expected.read().await;
        let waiters = self.waiters.read().await;
        expected.iter().all(|e| waiters.contains(e))
    }
}

impl Default for NamedBarrierChannel {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

#[async_trait]
impl BaseChannel for NamedBarrierChannel {
    async fn read(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn write(&self, _value: serde_json::Value) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        false
    }
}
