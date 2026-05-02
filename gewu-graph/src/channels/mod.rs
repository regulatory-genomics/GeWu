//! Communication channels for inter-node data flow.
//!
//! This module provides various channel implementations for managing data flow
//! between nodes in a graph. Channels are particularly useful for:
//!
//! - Building complex state management patterns
//! - Implementing accumulated updates
//! - FIFO queues for task processing
//! - Synchronization barriers
//!
//! ## Channel Types
//!
//! | Type | Behavior |
//! |------|----------|
//! | `LastValueChannel` | Keeps the most recently written value |
//! | `EphemeralValueChannel` | One-time read, returns Null after first read |
//! | `BinaryOperatorChannel` | Combines values using a binary operation |
//! | `AnyValueChannel` | First-wins channel, value persists after read |
//! | `NamedBarrierChannel` | Blocks until all named participants check in |
//! | `TopicChannel` | FIFO queue with maximum size, drops oldest on overflow |
//!
//! ## Example
//!
//! ```
//! use gewu_graph::channels::{BaseChannel, LastValueChannel};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let channel: LastValueChannel<i32> = LastValueChannel::new();
//! channel.write(serde_json::json!(42)).await;
//! let value = channel.read().await;
//! assert_eq!(value, serde_json::json!(42));
//! # }
//! ```

pub mod base;
#[cfg(test)]
mod tests;
pub mod topic;

pub use base::{
    AnyValueChannel, BaseChannel, BinaryOperatorChannel, EphemeralValueChannel, LastValueChannel,
    NamedBarrierChannel,
};
pub use topic::TopicChannel;
