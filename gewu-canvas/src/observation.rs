use async_trait::async_trait;
use gewu_core::observation::{ObservationEvent, ObservationSource};
use reqwest::Client;
use serde::Deserialize;

/// Polls tachyon-canvas workflow events via HTTP.
pub struct TachyonObservationSource {
    client: Client,
    base_url: String,
}

impl TachyonObservationSource {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Deserialize)]
struct EventsResponse {
    events: Vec<RawEvent>,
}

#[derive(Deserialize)]
struct RawEvent {
    id: String,
    kind: String,
    payload: serde_json::Value,
    timestamp: String,
}

#[async_trait]
impl ObservationSource for TachyonObservationSource {
    async fn poll(&self, cursor: Option<&str>) -> Vec<ObservationEvent> {
        let url = match cursor {
            Some(cursor) => format!("{}/api/workflow/events?since={}", self.base_url, cursor),
            None => format!("{}/api/workflow/events", self.base_url),
        };

        let response = match self.client.get(&url).send().await {
            Ok(response) => response,
            Err(_) => return Vec::new(),
        };

        let body: EventsResponse = match response.json().await {
            Ok(body) => body,
            Err(_) => return Vec::new(),
        };

        body.events
            .into_iter()
            .map(|event| ObservationEvent {
                id: event.id,
                source: "workflow".to_string(),
                kind: event.kind,
                payload: event.payload,
                timestamp: event.timestamp,
            })
            .collect()
    }

    fn source_name(&self) -> &str {
        "canvas"
    }
}
