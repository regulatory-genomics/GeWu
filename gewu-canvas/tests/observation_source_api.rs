use gewu_canvas::observation::TachyonObservationSource;
use gewu_canvas::subscription::InMemorySubscriptionStore;
use gewu_core::observation::{EventPattern, ObservationSource, SubscriptionStore};

#[test]
fn subscription_store_registers_matches_and_deactivates() {
    let store = InMemorySubscriptionStore::new();
    let id = store.register("agent_1", vec![EventPattern::ByKind(vec!["data_ingested".to_string()])]);

    let matches = store.match_event("data_ingested", &serde_json::Value::Null);
    assert_eq!(matches, vec!["agent_1".to_string()]);

    store.set_active(&id, false);
    assert!(store.match_event("data_ingested", &serde_json::Value::Null).is_empty());
}

#[test]
fn subscription_store_lists_only_active_agent_subscriptions() {
    let store = InMemorySubscriptionStore::new();
    let active = store.register("agent_1", vec![EventPattern::ByKind(vec!["data_ingested".to_string()])]);
    let inactive = store.register("agent_1", vec![EventPattern::ByKind(vec!["artifact_produced".to_string()])]);
    store.register("agent_2", vec![EventPattern::ByKind(vec!["data_ingested".to_string()])]);
    store.set_active(&inactive, false);

    let subs = store.list_for_agent("agent_1");

    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].id, active);
}

#[tokio::test]
async fn tachyon_observation_source_exposes_canvas_source_name_and_empty_on_error() {
    let source = TachyonObservationSource::new("http://127.0.0.1:9/");

    assert_eq!(source.source_name(), "canvas");
    assert!(source.poll(None).await.is_empty());
}
