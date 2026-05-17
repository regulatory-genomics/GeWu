use async_trait::async_trait;
use gewu_core::observation::{EventPattern, ObservationEvent, ObservationSource, SubscriptionStore};
use gewu_core::traits::{AgentContext, ContextAssembler, DataSummary, InvocationTrigger, LineageStep, NodeSummary};
use gewu_core::types::AgentEvent;
use serde_json::json;

struct EmptySource;

#[async_trait]
impl ObservationSource for EmptySource {
    async fn poll(&self, _cursor: Option<&str>) -> Vec<ObservationEvent> {
        Vec::new()
    }

    fn source_name(&self) -> &str {
        "test"
    }
}

struct EmptyAssembler;

#[async_trait]
impl ContextAssembler for EmptyAssembler {
    async fn assemble(&self, trigger: InvocationTrigger) -> AgentContext {
        AgentContext {
            trigger,
            selection: vec![NodeSummary {
                id: "n1".to_string(),
                kind: "artifact".to_string(),
                format: Some("bed".to_string()),
                summary: "node".to_string(),
                uri: Some("/tmp/a.bed".to_string()),
            }],
            neighbors: Vec::new(),
            lineage: vec![LineageStep {
                node_id: "n1".to_string(),
                step_id: None,
                description: "root".to_string(),
                depth: 0,
            }],
            recent_events: Vec::new(),
            user_intent: Some("inspect".to_string()),
            available_data: vec![DataSummary {
                id: "d1".to_string(),
                format: "bed".to_string(),
                uri: "/tmp/a.bed".to_string(),
                description: "input".to_string(),
            }],
        }
    }
}

#[test]
fn agent_event_observation_serializes_as_snake_case_variant() {
    let event = AgentEvent::Observation {
        source: "canvas".to_string(),
        kind: "data_ingested".to_string(),
        payload: json!({ "data_id": "d1" }),
        timestamp: "2026-05-17T00:00:00Z".to_string(),
    };

    let value = serde_json::to_value(event).expect("serialize observation event");

    assert_eq!(value["type"], "observation");
    assert_eq!(value["source"], "canvas");
    assert_eq!(value["kind"], "data_ingested");
}

#[tokio::test]
async fn observation_source_and_context_assembler_are_usable_traits() {
    let source = EmptySource;
    assert_eq!(source.source_name(), "test");
    assert!(source.poll(None).await.is_empty());

    let assembler = EmptyAssembler;
    let trigger = InvocationTrigger::Explicit {
        prompt: "inspect".to_string(),
        selected_nodes: vec!["n1".to_string()],
    };
    let context = assembler.assemble(trigger).await;

    assert_eq!(context.selection[0].id, "n1");
    assert_eq!(context.available_data[0].format, "bed");
}

#[allow(dead_code)]
fn subscription_store_trait_mentions_patterns<T: SubscriptionStore>(store: &T) {
    let _id = store.register("agent", vec![EventPattern::ByKind(vec!["data_ingested".to_string()])]);
}
