use crate::channels::{
    AnyValueChannel, BaseChannel, BinaryOperatorChannel, EphemeralValueChannel, LastValueChannel,
    NamedBarrierChannel, TopicChannel,
};

#[tokio::test]
async fn test_last_value_channel() {
    let channel: LastValueChannel<i32> = LastValueChannel::new();

    channel.write(serde_json::json!(10)).await;
    let val1 = channel.read().await;
    assert_eq!(val1, serde_json::json!(10));

    channel.write(serde_json::json!(20)).await;
    let val2 = channel.read().await;
    assert_eq!(val2, serde_json::json!(20));
}

#[tokio::test]
async fn test_ephemeral_value_channel() {
    let channel: EphemeralValueChannel<String> = EphemeralValueChannel::new();

    channel.write(serde_json::json!("first")).await;
    let val1 = channel.read().await;
    assert_eq!(val1, serde_json::json!("first"));

    let val2 = channel.read().await;
    assert_eq!(val2, serde_json::Value::Null);
}

#[tokio::test]
async fn test_binary_operator_channel_sum() {
    let channel: BinaryOperatorChannel<i32> = BinaryOperatorChannel::sum();

    channel.write(serde_json::json!(10)).await;
    channel.write(serde_json::json!(20)).await;
    channel.write(serde_json::json!(30)).await;

    let val = channel.read().await;
    assert_eq!(val, serde_json::json!(60));
}

#[tokio::test]
async fn test_binary_operator_channel_last_wins() {
    let channel: BinaryOperatorChannel<i32> = BinaryOperatorChannel::default();

    channel.write(serde_json::json!(10)).await;
    channel.write(serde_json::json!(20)).await;

    let val = channel.read().await;
    assert_eq!(val, serde_json::json!(20));
}

#[tokio::test]
async fn test_any_value_channel_first_wins() {
    let channel: AnyValueChannel<i32> = AnyValueChannel::new();

    channel.write(serde_json::json!(10)).await;
    channel.write(serde_json::json!(20)).await;
    channel.write(serde_json::json!(30)).await;

    let val = channel.read().await;
    assert_eq!(val, serde_json::json!(10));
}

#[tokio::test]
async fn test_named_barrier_channel() {
    let channel = NamedBarrierChannel::new(vec!["a".to_string(), "b".to_string()]);

    let result1 = channel.wait_for("a").await;
    assert!(!result1);

    let result2 = channel.wait_for("b").await;
    assert!(result2);
}

#[tokio::test]
async fn test_topic_channel() {
    let channel: TopicChannel<String> = TopicChannel::new(3);

    channel.write("msg1".to_string()).await;
    channel.write("msg2".to_string()).await;
    channel.write("msg3".to_string()).await;
    channel.write("msg4".to_string()).await;

    let val1 = channel.read().await;
    assert_eq!(val1, Some("msg2".to_string()));

    let val2 = channel.read().await;
    assert_eq!(val2, Some("msg2".to_string()));
}

#[tokio::test]
async fn test_topic_channel_empty() {
    let channel: TopicChannel<String> = TopicChannel::new(3);

    let val = channel.read().await;
    assert_eq!(val, None);
}

#[tokio::test]
async fn test_channel_is_empty() {
    let last_value: LastValueChannel<i32> = LastValueChannel::new();
    let ephemeral: EphemeralValueChannel<i32> = EphemeralValueChannel::new();
    let binary: BinaryOperatorChannel<i32> = BinaryOperatorChannel::new(|a, b| a + b);
    let any: AnyValueChannel<i32> = AnyValueChannel::new();
    let barrier: NamedBarrierChannel = NamedBarrierChannel::new(vec![]);

    assert!(!last_value.is_empty());
    assert!(!ephemeral.is_empty());
    assert!(!binary.is_empty());
    assert!(!any.is_empty());
    assert!(!barrier.is_empty());
}

#[tokio::test]
async fn test_channel_write_returns_bool() {
    let channel: LastValueChannel<i32> = LastValueChannel::new();

    let result = channel.write(serde_json::json!(42)).await;
    assert!(result);

    let bad_result = channel.write(serde_json::json!("not an i32")).await;
    assert!(!bad_result);
}

#[tokio::test]
async fn test_topic_channel_overflow_drops_oldest() {
    let channel: TopicChannel<String> = TopicChannel::new(2);

    channel.write("msg1".to_string()).await;
    channel.write("msg2".to_string()).await;
    channel.write("msg3".to_string()).await;

    let val = channel.read().await;
    assert_eq!(val, Some("msg2".to_string()));

    let val2 = channel.read().await;
    assert_eq!(val2, Some("msg2".to_string()));
}

#[tokio::test]
async fn test_topic_channel_max_size_one() {
    let channel: TopicChannel<i32> = TopicChannel::new(1);

    channel.write(10).await;
    channel.write(20).await;
    channel.write(30).await;

    let val = channel.read().await;
    assert_eq!(val, Some(30));
}

#[tokio::test]
async fn test_topic_channel_read_after_write_sequence() {
    let channel: TopicChannel<String> = TopicChannel::new(5);

    channel.write("first".to_string()).await;
    let val1 = channel.read().await;
    assert_eq!(val1, Some("first".to_string()));

    channel.write("second".to_string()).await;
    let val2 = channel.read().await;
    assert_eq!(val2, Some("first".to_string()));
}

#[tokio::test]
async fn test_any_value_channel_clear_noop() {
    let channel: AnyValueChannel<i32> = AnyValueChannel::new();

    channel.write(serde_json::json!(10)).await;
    channel.write(serde_json::json!(20)).await;

    let val1 = channel.read().await;
    assert_eq!(val1, serde_json::json!(10));

    channel.clear();

    let val2 = channel.read().await;
    assert_eq!(val2, serde_json::json!(10));
}

#[tokio::test]
async fn test_any_value_channel_write_invalid_type() {
    let channel: AnyValueChannel<i32> = AnyValueChannel::new();

    let result = channel.write(serde_json::json!("string")).await;
    assert!(!result);

    let result2 = channel.write(serde_json::json!(null)).await;
    assert!(!result2);
}

#[tokio::test]
async fn test_any_value_channel_multiple_reads_same_value() {
    let channel: AnyValueChannel<String> = AnyValueChannel::new();

    channel.write(serde_json::json!("first")).await;

    let val1 = channel.read().await;
    let val2 = channel.read().await;
    let val3 = channel.read().await;

    assert_eq!(val1, serde_json::json!("first"));
    assert_eq!(val2, serde_json::json!("first"));
    assert_eq!(val3, serde_json::json!("first"));
}

#[tokio::test]
async fn test_named_barrier_channel_single_waiter() {
    let channel = NamedBarrierChannel::new(vec!["only".to_string()]);

    let result = channel.wait_for("only").await;
    assert!(result);
}

#[tokio::test]
async fn test_named_barrier_channel_three_waiters() {
    let channel = NamedBarrierChannel::new(vec!["a".to_string(), "b".to_string(), "c".to_string()]);

    let result1 = channel.wait_for("a").await;
    assert!(!result1);

    let result2 = channel.wait_for("b").await;
    assert!(!result2);

    let result3 = channel.wait_for("c").await;
    assert!(result3);
}

#[tokio::test]
async fn test_named_barrier_channel_unknown_waiter() {
    let channel = NamedBarrierChannel::new(vec!["known".to_string()]);

    let result = channel.wait_for("unknown").await;
    assert!(!result);
}

#[tokio::test]
async fn test_named_barrier_channel_waiters_not_reset() {
    let channel = NamedBarrierChannel::new(vec!["a".to_string(), "b".to_string()]);

    let _ = channel.wait_for("a").await;
    let _ = channel.wait_for("b").await;

    let result = channel.wait_for("a").await;
    assert!(result);
}

#[tokio::test]
async fn test_ephemeral_value_channel_type_mismatch() {
    let channel: EphemeralValueChannel<i32> = EphemeralValueChannel::new();

    let result = channel.write(serde_json::json!("string")).await;
    assert!(!result);

    let result2 = channel
        .write(serde_json::json!({"object": "not_i32"}))
        .await;
    assert!(!result2);
}

#[tokio::test]
async fn test_binary_operator_channel_multi_write() {
    let channel: BinaryOperatorChannel<i32> = BinaryOperatorChannel::new(|a, b| a * b);

    channel.write(serde_json::json!(2)).await;
    channel.write(serde_json::json!(3)).await;
    channel.write(serde_json::json!(4)).await;

    let val = channel.read().await;
    assert_eq!(val, serde_json::json!(24));
}

#[tokio::test]
async fn test_binary_operator_channel_type_mismatch() {
    let channel: BinaryOperatorChannel<i32> = BinaryOperatorChannel::sum();

    let result = channel.write(serde_json::json!("not_a_number")).await;
    assert!(!result);
}

#[tokio::test]
async fn test_last_value_channel_multiple_writes() {
    let channel: LastValueChannel<String> = LastValueChannel::new();

    channel.write(serde_json::json!("first")).await;
    channel.write(serde_json::json!("second")).await;
    channel.write(serde_json::json!("third")).await;

    let val = channel.read().await;
    assert_eq!(val, serde_json::json!("third"));
}

#[tokio::test]
async fn test_last_value_channel_read_before_write() {
    let channel: LastValueChannel<i32> = LastValueChannel::new();

    let val = channel.read().await;
    assert_eq!(val, serde_json::json!(null));
}
