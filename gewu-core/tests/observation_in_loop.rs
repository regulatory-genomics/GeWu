use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gewu_core::agent::AgentBuilder;
use gewu_core::observation::{ObservationEvent, ObservationSource};

struct MockObservationSource {
    events: Arc<Mutex<Vec<ObservationEvent>>>,
}

#[async_trait]
impl ObservationSource for MockObservationSource {
    async fn poll(&self, _cursor: Option<&str>) -> Vec<ObservationEvent> {
        let mut events = self.events.lock().unwrap();
        events.drain(..).collect()
    }

    fn source_name(&self) -> &str {
        "test"
    }
}

#[test]
fn agent_builder_accepts_observation_source() {
    let source = Arc::new(MockObservationSource {
        events: Arc::new(Mutex::new(vec![])),
    });

    let _builder = AgentBuilder::default().observation_source(source as Arc<dyn ObservationSource>);
}
