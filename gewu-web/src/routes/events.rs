use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use gewu_runtime::subscribe_background_events;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::routes::sessions::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    session_id: Option<String>,
}

pub async fn handle_events(
    State(_app): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    // Auth is enforced by middleware; cookies are sent automatically by
    // EventSource for same-origin requests.

    let session_filter = query.session_id.clone();
    let stream = BroadcastStream::new(subscribe_background_events()).filter_map(move |event| {
        let session_filter = session_filter.clone();
        match event {
            Ok(ev) => {
                if session_filter
                    .as_deref()
                    .is_some_and(|sid| sid != ev.parent_session_id)
                {
                    return None;
                }
                let data = serde_json::to_string(&ev).unwrap_or_default();
                Some(Ok(Event::default()
                    .event("background_group.completed")
                    .data(data)))
            }
            Err(_) => None,
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
