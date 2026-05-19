use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;

const MAX_MAILBOX_EVENTS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEvent {
    pub id: String,
    pub timestamp: f64,
    pub event: Value,
}

#[derive(Default)]
pub struct MailboxBuffer {
    events: VecDeque<MailboxEvent>,
    next_id: u64,
}

impl MailboxBuffer {
    pub fn push(&mut self, event: Value) -> MailboxEvent {
        let id = self.next_id;
        self.next_id += 1;
        let entry = MailboxEvent {
            id: format!("mb_{id}"),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            event,
        };
        self.events.push_back(entry.clone());
        if self.events.len() > MAX_MAILBOX_EVENTS {
            self.events.pop_front();
        }
        entry
    }

    pub fn poll_since(&self, since_id: Option<&str>) -> Vec<&MailboxEvent> {
        match since_id {
            None => self.events.iter().collect(),
            Some(cursor) => {
                let mut found = false;
                self.events
                    .iter()
                    .filter(move |event| {
                        if found {
                            return true;
                        }
                        if event.id == cursor {
                            found = true;
                        }
                        false
                    })
                    .collect()
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PollQuery {
    pub since: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PostMailboxBody {
    One(Value),
    Many(Vec<Value>),
}

pub async fn post_mailbox(
    Path(workspace_id): Path<String>,
    State(app): State<Arc<AppState>>,
    Json(body): Json<PostMailboxBody>,
) -> impl IntoResponse {
    let events = match body {
        PostMailboxBody::One(event) => vec![event],
        PostMailboxBody::Many(events) => events,
    };
    let mailbox = app.get_or_create_mailbox(&workspace_id).await;
    let mut buffer = mailbox.lock().await;
    let ids: Vec<String> = events
        .into_iter()
        .map(|event| buffer.push(event).id)
        .collect();

    (StatusCode::OK, Json(json!({ "ok": true, "ids": ids })))
}

pub async fn poll_mailbox(
    Path(workspace_id): Path<String>,
    Query(query): Query<PollQuery>,
    State(app): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mailbox = app.get_or_create_mailbox(&workspace_id).await;
    let buffer = mailbox.lock().await;
    let events: Vec<MailboxEvent> = buffer
        .poll_since(query.since.as_deref())
        .into_iter()
        .cloned()
        .collect();
    let cursor = events.last().map(|event| event.id.clone());

    Json(json!({ "events": events, "cursor": cursor }))
}
