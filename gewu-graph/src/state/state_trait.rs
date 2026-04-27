//! Core state management traits and built-in reducers.

use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Debug;

/// The core trait for graph state.
///
/// Implement this trait to define how your state is structured and how
/// partial updates are merged. The graph runtime uses partial updates to
/// allow nodes to update only specific fields of the state without
/// requiring the full state to be returned.
pub trait State: Serialize + DeserializeOwned + Default + Debug + Clone + Send + Sync {
    /// The type representing a partial (incremental) state update.
    ///
    /// Typically this is an optional version of each field, allowing
    /// nodes to update only the fields they care about.
    type Partial: Serialize + DeserializeOwned + Debug + Clone + Send + Sync + Default;

    /// Merges a partial update into the current state.
    ///
    /// This method is called after each node execution to incorporate
    /// the node's partial update into the running state. Typically,
    /// fields present in `update` override those in `self`, while
    /// `None` fields leave the current value unchanged.
    ///
    /// # Arguments
    /// * `update` - The partial update produced by a node
    ///
    /// # Returns
    /// A new state with the update applied
    fn merge(&self, update: Self::Partial) -> Self;
}

/// A trait for combining values using a binary operation.
///
/// Reducers are used by channels to determine how multiple values
/// should be combined into a single result.
pub trait Reducer<T>: Send + Sync + 'static {
    /// Combines two values into one.
    ///
    /// # Arguments
    /// * `current` - The existing accumulated value
    /// * `new` - The new value to combine
    ///
    /// # Returns
    /// The combined result
    fn reduce(&self, current: T, new: T) -> T;
}

/// A reducer that keeps the most recently provided value.
///
/// When combining values, only the new value is kept (last-write-wins).
#[derive(Debug, Clone, Default)]
pub struct LastValue;

impl<T: Clone + Send + Sync> Reducer<T> for LastValue {
    fn reduce(&self, _current: T, new: T) -> T {
        new
    }
}

/// A reducer that appends new items to a collection.
///
/// Useful for accumulating messages or log entries.
#[derive(Debug, Clone, Default)]
pub struct Append;

impl<T: Clone + Send + Sync> Reducer<Vec<T>> for Append {
    fn reduce(&self, mut current: Vec<T>, new: Vec<T>) -> Vec<T> {
        current.extend(new);
        current
    }
}

/// A reducer that sums numeric values.
///
/// Works with `i32`, `i64`, and `f64` types.
#[derive(Debug, Clone, Default)]
pub struct Sum;

impl Reducer<i32> for Sum {
    fn reduce(&self, current: i32, new: i32) -> i32 {
        current + new
    }
}

impl Reducer<i64> for Sum {
    fn reduce(&self, current: i64, new: i64) -> i64 {
        current + new
    }
}

impl Reducer<f64> for Sum {
    fn reduce(&self, current: f64, new: f64) -> f64 {
        current + new
    }
}
