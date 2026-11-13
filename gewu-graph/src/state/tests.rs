use crate::state::{Append, LastValue, Reducer, State, Sum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TestState {
    count: i32,
    value: String,
    items: Vec<i32>,
}

impl State for TestState {
    type Partial = PartialTestState;

    fn merge(&self, update: Self::Partial) -> Self {
        Self {
            count: update.count.unwrap_or(self.count),
            value: update.value.unwrap_or_else(|| self.value.clone()),
            items: update.items.unwrap_or_else(|| self.items.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PartialTestState {
    count: Option<i32>,
    value: Option<String>,
    items: Option<Vec<i32>>,
}

#[test]
fn test_state_merge_all_none() {
    let state = TestState {
        count: 10,
        value: "hello".to_string(),
        items: vec![1, 2, 3],
    };

    let partial = PartialTestState {
        count: None,
        value: None,
        items: None,
    };

    let result = state.merge(partial);
    assert_eq!(result.count, 10);
    assert_eq!(result.value, "hello");
    assert_eq!(result.items, vec![1, 2, 3]);
}

#[test]
fn test_state_merge_all_some() {
    let state = TestState {
        count: 10,
        value: "hello".to_string(),
        items: vec![1, 2, 3],
    };

    let partial = PartialTestState {
        count: Some(20),
        value: Some("world".to_string()),
        items: Some(vec![4, 5, 6]),
    };

    let result = state.merge(partial);
    assert_eq!(result.count, 20);
    assert_eq!(result.value, "world");
    assert_eq!(result.items, vec![4, 5, 6]);
}

#[test]
fn test_state_merge_mixed() {
    let state = TestState {
        count: 10,
        value: "hello".to_string(),
        items: vec![1, 2, 3],
    };

    let partial = PartialTestState {
        count: Some(20),
        value: None,
        items: Some(vec![4, 5]),
    };

    let result = state.merge(partial);
    assert_eq!(result.count, 20);
    assert_eq!(result.value, "hello");
    assert_eq!(result.items, vec![4, 5]);
}

#[test]
fn test_last_value_reducer() {
    let reducer = LastValue;
    let result = reducer.reduce(10, 20);
    assert_eq!(result, 20);
}

#[test]
fn test_append_reducer() {
    let reducer = Append;
    let result = reducer.reduce(vec![1, 2], vec![3, 4]);
    assert_eq!(result, vec![1, 2, 3, 4]);
}

#[test]
fn test_sum_reducer_i32() {
    let reducer = Sum;
    let result = reducer.reduce(10, 20);
    assert_eq!(result, 30);
}

#[test]
fn test_sum_reducer_i64() {
    let reducer = Sum;
    let result = reducer.reduce(10i64, 20i64);
    assert_eq!(result, 30i64);
}

#[test]
fn test_sum_reducer_f64() {
    let reducer = Sum;
    let result = reducer.reduce(10.5, 20.5);
    assert_eq!(result, 31.0);
}

#[test]
fn test_last_value_default() {
    let reducer = LastValue;
    let result = reducer.reduce(5, 10);
    assert_eq!(result, 10);
}

#[test]
fn test_append_default() {
    let reducer = Append;
    let result = reducer.reduce(vec![], vec![1, 2]);
    assert_eq!(result, vec![1, 2]);
}

#[test]
fn test_sum_default() {
    let reducer = Sum;
    let result = reducer.reduce(5, 3);
    assert_eq!(result, 8);
}

#[test]
fn test_reducer_send_sync_bounds() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<LastValue>();
    assert_send_sync::<Append>();
    assert_send_sync::<Sum>();
}

#[test]
fn test_state_trait_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<TestState>();
    assert_send_sync::<PartialTestState>();
}

#[test]
fn test_last_value_reducer_identity() {
    let reducer = LastValue;
    let result = reducer.reduce(42, 100);
    assert_eq!(result, 100);
    let result2 = reducer.reduce(42, 0);
    assert_eq!(result2, 0);
}

#[test]
fn test_append_reducer_empty_initial() {
    let reducer = Append;
    let result = reducer.reduce(Vec::<i32>::new(), vec![1, 2, 3]);
    assert_eq!(result, vec![1, 2, 3]);
}

#[test]
fn test_append_reducer_empty_new() {
    let reducer = Append;
    let result = reducer.reduce(vec![1, 2, 3], Vec::<i32>::new());
    assert_eq!(result, vec![1, 2, 3]);
}

#[test]
fn test_sum_reducer_zero() {
    let reducer = Sum;
    let result = reducer.reduce(0, 0);
    assert_eq!(result, 0);
}

#[test]
fn test_sum_reducer_negative() {
    let reducer = Sum;
    let result = reducer.reduce(-10, 5);
    assert_eq!(result, -5);
}

#[test]
fn test_partial_state_serialization() {
    let partial = PartialTestState {
        count: Some(42),
        value: Some("hello".to_string()),
        items: Some(vec![1, 2, 3]),
    };

    let json = serde_json::to_string(&partial).unwrap();
    let deserialized: PartialTestState = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.count, Some(42));
    assert_eq!(deserialized.value, Some("hello".to_string()));
    assert_eq!(deserialized.items, Some(vec![1, 2, 3]));
}

#[test]
fn test_full_state_serialization() {
    let state = TestState {
        count: 100,
        value: "test".to_string(),
        items: vec![4, 5, 6],
    };

    let json = serde_json::to_string(&state).unwrap();
    let deserialized: TestState = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.count, 100);
    assert_eq!(deserialized.value, "test");
    assert_eq!(deserialized.items, vec![4, 5, 6]);
}
