use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Pattern for matching workflow events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPattern {
    ByKind(Vec<String>),
    ByNodeId(String),
    ByFormat(String),
    ByActor(String),
    Compound(Vec<EventPattern>),
}

/// A subscription record for an agent observing workflow events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub agent_id: String,
    pub patterns: Vec<EventPattern>,
    pub active: bool,
    pub created_at: String,
    pub last_cursor: Option<String>,
}

/// Trait for managing agent subscriptions to workflow events.
#[async_trait]
pub trait SubscriptionStore: Send + Sync {
    fn register(&self, agent_id: &str, patterns: Vec<EventPattern>) -> String;
    fn unregister(&self, subscription_id: &str);
    fn list_for_agent(&self, agent_id: &str) -> Vec<Subscription>;
    fn match_event(&self, event_kind: &str, payload: &serde_json::Value) -> Vec<String>;
    fn set_active(&self, subscription_id: &str, active: bool);
}

/// Source of observations for the agent.
#[async_trait]
pub trait ObservationSource: Send + Sync {
    /// Poll for new observations since the given cursor.
    async fn poll(&self, cursor: Option<&str>) -> Vec<ObservationEvent>;

    /// Get the source name, for example "canvas" or "workflow".
    fn source_name(&self) -> &str;
}

/// A raw observation event from an external source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub id: String,
    pub source: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub timestamp: String,
}
