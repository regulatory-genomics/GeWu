use gewu_core::observation::{EventPattern, Subscription, SubscriptionStore};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct InMemorySubscriptionStore {
    subscriptions: Arc<Mutex<HashMap<String, Subscription>>>,
    counter: Arc<Mutex<u64>>,
}

impl InMemorySubscriptionStore {
    pub fn new() -> Self {
        Self {
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(Mutex::new(0)),
        }
    }
}

impl Default for InMemorySubscriptionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionStore for InMemorySubscriptionStore {
    fn register(&self, agent_id: &str, patterns: Vec<EventPattern>) -> String {
        let mut counter = self.counter.lock().expect("subscription counter lock poisoned");
        *counter += 1;
        let id = format!("sub_{}", *counter);
        let subscription = Subscription {
            id: id.clone(),
            agent_id: agent_id.to_string(),
            patterns,
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_cursor: None,
        };
        self.subscriptions
            .lock()
            .expect("subscription map lock poisoned")
            .insert(id.clone(), subscription);
        id
    }

    fn unregister(&self, subscription_id: &str) {
        self.subscriptions
            .lock()
            .expect("subscription map lock poisoned")
            .remove(subscription_id);
    }

    fn list_for_agent(&self, agent_id: &str) -> Vec<Subscription> {
        self.subscriptions
            .lock()
            .expect("subscription map lock poisoned")
            .values()
            .filter(|subscription| subscription.agent_id == agent_id && subscription.active)
            .cloned()
            .collect()
    }

    fn match_event(&self, event_kind: &str, payload: &serde_json::Value) -> Vec<String> {
        self.subscriptions
            .lock()
            .expect("subscription map lock poisoned")
            .values()
            .filter(|subscription| subscription.active && matches_any_pattern(&subscription.patterns, event_kind, payload))
            .map(|subscription| subscription.agent_id.clone())
            .collect()
    }

    fn set_active(&self, subscription_id: &str, active: bool) {
        if let Some(subscription) = self
            .subscriptions
            .lock()
            .expect("subscription map lock poisoned")
            .get_mut(subscription_id)
        {
            subscription.active = active;
        }
    }
}

fn matches_any_pattern(patterns: &[EventPattern], event_kind: &str, payload: &serde_json::Value) -> bool {
    patterns.iter().any(|pattern| matches_pattern(pattern, event_kind, payload))
}

fn matches_pattern(pattern: &EventPattern, event_kind: &str, payload: &serde_json::Value) -> bool {
    match pattern {
        EventPattern::ByKind(kinds) => kinds.iter().any(|kind| kind == event_kind),
        EventPattern::ByFormat(format) => payload
            .get("format")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == format),
        EventPattern::ByActor(actor) => payload
            .get("actor")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == actor),
        EventPattern::ByNodeId(node_id) => payload_mentions_node(payload, node_id),
        EventPattern::Compound(patterns) => patterns.iter().all(|pattern| matches_pattern(pattern, event_kind, payload)),
    }
}

fn payload_mentions_node(payload: &serde_json::Value, node_id: &str) -> bool {
    const DIRECT_FIELDS: &[&str] = &["data_id", "artifact_id", "step_id", "target_id", "from_id", "to_id"];
    DIRECT_FIELDS
        .iter()
        .any(|field| payload.get(field).and_then(serde_json::Value::as_str) == Some(node_id))
        || payload
            .get("target_nodes")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|nodes| nodes.iter().any(|node| node.as_str() == Some(node_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_match() {
        let store = InMemorySubscriptionStore::new();
        store.register(
            "agent_1",
            vec![EventPattern::ByKind(vec!["data_ingested".to_string()])],
        );
        let matches = store.match_event("data_ingested", &serde_json::Value::Null);
        assert_eq!(matches, vec!["agent_1".to_string()]);
    }

    #[test]
    fn test_inactive_does_not_match() {
        let store = InMemorySubscriptionStore::new();
        let id = store.register(
            "agent_1",
            vec![EventPattern::ByKind(vec!["data_ingested".to_string()])],
        );
        store.set_active(&id, false);
        let matches = store.match_event("data_ingested", &serde_json::Value::Null);
        assert!(matches.is_empty());
    }
}
