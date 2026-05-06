//! State management traits and reducers.
//!
//! This module provides the core `State` trait for managing state in graphs,
//! along with built-in `Reducer` implementations for common patterns.
//!
//! ## The State Trait
//!
//! Your state type must implement `State` which defines:
//! - How partial updates are merged into the full state
//! - The type of partial updates (`State::Partial`)
//!
//! ## Reducers
//!
//! Reducers are used to combine values, commonly in channel implementations.
//! The built-in reducers are:
//!
//! - `LastValue` - Keeps the most recent value
//! - `Append` - Concatenates vectors
//! - `Sum` - Adds numeric values
//!
//! ## Example
//!
//! ```
//! use gewu_graph::state::{State, LastValue, Reducer};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Clone, Default, Serialize, Deserialize)]
//! struct MyState {
//!     count: i32,
//!     items: Vec<String>,
//! }
//!
//! #[derive(Debug, Clone, Default, Serialize, Deserialize)]
//! struct PartialMyState {
//!     count: Option<i32>,
//!     items: Option<Vec<String>>,
//! }
//!
//! impl State for MyState {
//!     type Partial = PartialMyState;
//!
//!     fn merge(&self, update: Self::Partial) -> Self {
//!         Self {
//!             count: update.count.unwrap_or(self.count),
//!             items: update.items.unwrap_or_else(|| self.items.clone()),
//!         }
//!     }
//! }
//! ```

pub mod blackboard;
pub mod state_trait;
#[cfg(test)]
mod tests;

pub use blackboard::Blackboard;
pub use state_trait::{Append, LastValue, Reducer, State, Sum};
