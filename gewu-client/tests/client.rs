//! End-to-end client tests against a fake in-process server speaking the
//! real protocol over TCP.

mod fake;

use fake::{FakeServer, Step};
use gewu_client::{Client, Error};
use gewu_protocol::{AgentEvent, ServerMessage};
use std::collections::VecDeque;

async fn connect(server: &FakeServer) -> Client {
    Client::tcp("127.0.0.1", server.port)
        .await
        .expect("client connects")
}

#[tokio::test]
async fn ping_round_trip() {
    let server = fake::spawn(|| VecDeque::from([Step::Reply(vec![fake::pong_line("1")])])).await;
    let client = connect(&server).await;

    client.ping().await.expect("pong");

    client.close().await;
    server.stop().await;
}

/// Waits until `client.is_closed()` flips — the actor exits one poll after
/// the failing reply is delivered, so the flag is only eventually consistent.
async fn eventually_closed(client: &Client) -> bool {
    for _ in 0..100 {
        if client.is_closed() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    client.is_closed()
}

#[tokio::test]
async fn is_closed_tracks_connection_death() {
    let server = fake::spawn(|| VecDeque::from([Step::Reply(vec![fake::pong_line("1")])])).await;
    let client = connect(&server).await;
    assert!(!client.is_closed());

    client.ping().await.expect("pong");

    // The script is exhausted: the fake now closes the connection the way a
    // server restart does to its clients.
    assert!(client.ping().await.is_err());
    assert!(eventually_closed(&client).await, "client must be closed");

    server.stop().await;
}

#[tokio::test]
async fn close_marks_all_clones_closed() {
    let server = fake::spawn(|| VecDeque::from([Step::Reply(vec![fake::pong_line("1")])])).await;
    let client = connect(&server).await;
    let clone = client.clone();

    client.close().await;
    assert!(
        eventually_closed(&client).await && eventually_closed(&clone).await,
        "closing one handle must close every clone"
    );

    server.stop().await;
}

#[tokio::test]
async fn question_reply_and_reject_round_trip() {
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![fake::question_replied_line(
                "1",
                "s1",
                "que_1",
                Some(serde_json::json!([["SQLite"]])),
            )]),
            Step::Reply(vec![fake::question_replied_line("2", "s1", "que_1", None)]),
        ])
    })
    .await;
    let client = connect(&server).await;

    client
        .reply_question("que_1", vec![vec!["SQLite".to_string()]])
        .await
        .expect("question reply");
    client
        .reject_question("que_1")
        .await
        .expect("question rejection");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn usage_round_trip() {
    let server =
        fake::spawn(|| VecDeque::from([Step::Reply(vec![fake::usage_line("1", 34_000, 128_000)])]))
            .await;
    let client = connect(&server).await;

    let usage = client.usage(Some("s1")).await.expect("usage");
    assert_eq!(usage.used_tokens, 34_000);
    assert_eq!(usage.context_limit, 128_000);

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn delete_session_round_trip() {
    let server =
        fake::spawn(|| VecDeque::from([Step::Reply(vec![fake::deleted_line("1", "s1")])])).await;
    let client = connect(&server).await;

    client.delete_session("s1").await.expect("deleted");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn info_round_trip() {
    let server = fake::spawn(|| {
        VecDeque::from([Step::Reply(vec![fake::info_line("1", "gpt-4o", "openai")])])
    })
    .await;
    let client = connect(&server).await;

    let info = client.info().await.expect("info");
    assert_eq!(info.model, "gpt-4o");
    assert_eq!(info.provider, "openai");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn list_tools_and_agents() {
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![fake::tools_line("1", &["read_file", "bash"])]),
            Step::Reply(vec![fake::agents_line("2", &["build", "explore"])]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools, vec!["read_file".to_string(), "bash".to_string()]);

    let agents = client.list_agents().await.unwrap();
    assert_eq!(agents, vec!["build".to_string(), "explore".to_string()]);

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn list_and_load_sessions() {
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![fake::sessions_line(
                "1",
                serde_json::json!([
                    {"session_id": "s1", "message_count": 1, "updated_at": "now"}
                ]),
            )]),
            Step::Reply(vec![fake::session_line(
                "2",
                "s1",
                serde_json::json!([
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ]),
            )]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let sessions = client.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "s1");
    assert_eq!(sessions[0].message_count, 1);

    let messages = client.get_session("s1").await.unwrap();
    assert_eq!(messages.len(), 1);

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn ask_streams_events_then_result() {
    let server = fake::spawn(|| {
        VecDeque::from([Step::Reply(vec![
            fake::ready_line("1", "req-1", "sess-1"),
            fake::event_line(
                "req-1",
                serde_json::json!({"type": "content_delta", "delta": "Hello"}),
            ),
            fake::event_line(
                "req-1",
                serde_json::json!({"type": "tool_start", "tool": "bash", "input": "{}"}),
            ),
            fake::result_line("req-1", "sess-1", "Hello, world!"),
        ])])
    })
    .await;
    let client = connect(&server).await;

    let mut ask = client.ask("hi").send().await.unwrap();
    let mut events = Vec::new();
    let mut final_response = None;
    while let Some(frame) = ask.next_frame().await {
        match frame.unwrap() {
            ServerMessage::Event { event, .. } => {
                let event: AgentEvent = serde_json::from_value(event).unwrap();
                events.push(event);
            }
            ServerMessage::Result { response, .. } => final_response = Some(response),
            other => panic!("unexpected frame: {}", other.kind()),
        }
    }
    assert_eq!(final_response.unwrap(), "Hello, world!");
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], AgentEvent::ContentDelta { .. }));
    assert!(matches!(events[1], AgentEvent::ToolStart { .. }));

    // Stream is exhausted.
    assert!(ask.next_frame().await.is_none());

    // Session id was tracked from the ready/result frames.
    assert_eq!(client.session_id().await.as_deref(), Some("sess-1"));

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn final_response_short_circuits_streaming() {
    let server = fake::spawn(|| {
        VecDeque::from([Step::Reply(vec![
            fake::ready_line("1", "req-1", "sess-1"),
            fake::event_line(
                "req-1",
                serde_json::json!({"type": "content_delta", "delta": "Hi"}),
            ),
            fake::result_line("req-1", "sess-1", "Hi there"),
        ])])
    })
    .await;
    let client = connect(&server).await;

    let response = client
        .ask("hi")
        .send()
        .await
        .unwrap()
        .final_response()
        .await
        .unwrap();
    assert_eq!(response, "Hi there");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn server_error_frame_surfaces_as_error() {
    let server = fake::spawn(|| {
        VecDeque::from([
            // First request: ask fails after ready (mid-ask terminal error).
            Step::Reply(vec![
                fake::ready_line("1", "req-1", "sess-1"),
                fake::error_line(None, "ask ended without a final response"),
            ]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let mut ask = client.ask("hi").send().await.unwrap();
    let mut saw_terminal_error = false;
    while let Some(frame) = ask.next_frame().await {
        if let ServerMessage::Error { error, .. } = frame.unwrap() {
            assert!(error.contains("without a final response"));
            saw_terminal_error = true;
        }
    }
    assert!(saw_terminal_error, "expected a terminal error frame");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn ask_rejected_before_ready_surfaces_as_server_error() {
    let server = fake::spawn(|| {
        VecDeque::from([Step::Reply(vec![fake::error_line(
            Some("1"),
            "an ask is already in progress; send 'interrupt' first",
        )])])
    })
    .await;
    let client = connect(&server).await;

    let err = client.ask("hi").send().await.unwrap_err();
    assert!(matches!(err, Error::Server(ref m) if m.contains("already in progress")));

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn interrupt_aborts_running_ask() {
    // Server: ready, then a slow event stream. The interrupt is answered
    // with an `interrupted` frame on its own correlation id.
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![
                fake::ready_line("1", "req-1", "sess-1"),
                fake::event_line(
                    "req-1",
                    serde_json::json!({"type": "content_delta", "delta": "working..."}),
                ),
            ]),
            Step::Reply(vec![
                serde_json::json!({
                    "type": "interrupted",
                    "id": 2,
                    "request_id": "req-1",
                })
                .to_string(),
            ]),
            // After interrupt, the ask's stream is closed by the server
            // (aborted without a terminal frame) — the client treats a
            // subsequent request normally.
            Step::Reply(vec![fake::pong_line("3")]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let mut ask = client.ask("long running").send().await.unwrap();
    let first = ask.next_frame().await.unwrap().unwrap();
    assert!(matches!(first, ServerMessage::Event { .. }));

    client.interrupt().await.expect("interrupted");

    // The connection remains usable afterwards.
    client.ping().await.expect("ping after interrupt");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn steer_injects_message_into_running_ask() {
    // Server: ready, one event, then — in reply to the steer request — the
    // `steered` ack on the steer's own correlation id plus the remaining
    // ask-stream frames (steered agent event, final result).
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![
                fake::ready_line("1", "req-1", "sess-1"),
                fake::event_line(
                    "req-1",
                    serde_json::json!({"type": "content_delta", "delta": "working..."}),
                ),
            ]),
            Step::Reply(vec![
                serde_json::json!({
                    "type": "steered",
                    "id": 2,
                    "session_id": "sess-1",
                    "text": "focus on tests",
                })
                .to_string(),
                fake::event_line(
                    "req-1",
                    serde_json::json!({"type": "steered", "text": "focus on tests"}),
                ),
                fake::event_line(
                    "req-1",
                    serde_json::json!({"type": "content_delta", "delta": "steered"}),
                ),
                fake::result_line("req-1", "sess-1", "steered"),
            ]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let mut ask = client.ask("long running").send().await.unwrap();
    let first = ask.next_frame().await.unwrap().unwrap();
    assert!(matches!(first, ServerMessage::Event { .. }));

    client.steer("focus on tests").await.expect("steered");

    // The steered message rides the ask stream and the run completes.
    let mut saw_steered_event = false;
    let mut saw_result = false;
    while let Some(frame) = ask.next_frame().await {
        match frame.unwrap() {
            ServerMessage::Event { event, .. } => {
                if event["type"] == "steered" {
                    assert_eq!(event["text"], "focus on tests");
                    saw_steered_event = true;
                }
            }
            ServerMessage::Result { .. } => {
                saw_result = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_steered_event, "expected a steered agent event");
    assert!(saw_result, "expected the ask to finish");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn second_ask_while_running_is_busy() {
    // Server only answers the first ask; the second ask should be rejected
    // client-side without hitting the wire.
    let server = fake::spawn(|| {
        VecDeque::from([Step::Reply(vec![
            fake::ready_line("1", "req-1", "sess-1"),
            // No terminal frame: the ask stays "running" from the client's
            // perspective until the stream finishes.
        ])])
    })
    .await;
    let client = connect(&server).await;

    let _ask = client.ask("first").send().await.unwrap();
    let err = client.ask("second").send().await.unwrap_err();
    assert!(matches!(err, Error::Busy));

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn connection_close_fails_in_flight_calls() {
    let server = fake::spawn(|| {
        VecDeque::from([Step::ReplyThenClose(vec![fake::ready_line(
            "1", "req-1", "sess-1",
        )])])
    })
    .await;
    let client = connect(&server).await;

    let mut ask = client.ask("hi").send().await.unwrap();

    // The server closed right after `ready`; the ask stream observes the
    // disconnect as a connection error instead of hanging forever.
    let frame = ask.next_frame().await;
    assert!(frame.is_some(), "stream should observe the disconnect");
    assert!(frame.unwrap().is_err());

    // Subsequent requests also fail.
    let err = client.ping().await.unwrap_err();
    assert!(matches!(err, Error::Connection(_)));

    server.stop().await;
}

#[tokio::test]
async fn session_id_is_none_before_any_ask() {
    let server = fake::spawn(|| VecDeque::from([Step::Reply(vec![fake::pong_line("1")])])).await;
    let client = connect(&server).await;

    assert_eq!(client.session_id().await, None);

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn malformed_server_frame_is_skipped() {
    // The server sends garbage between valid frames; the client must skip
    // it and still deliver the valid reply.
    let server = fake::spawn(|| {
        VecDeque::from([Step::Reply(vec![
            "not json at all".to_string(),
            fake::pong_line("1"),
        ])])
    })
    .await;
    let client = connect(&server).await;

    client.ping().await.expect("valid reply after garbage");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn second_ask_works_after_completed_ask() {
    // Regression: the actor must clear the active ask once its terminal
    // `result` frame is routed, otherwise every later ask reports Busy.
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![
                fake::ready_line("1", "req-1", "sess-1"),
                fake::result_line("req-1", "sess-1", "first answer"),
            ]),
            Step::Reply(vec![
                fake::ready_line("2", "req-2", "sess-1"),
                fake::result_line("req-2", "sess-1", "second answer"),
            ]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let first = client
        .ask("first")
        .send()
        .await
        .unwrap()
        .final_response()
        .await
        .unwrap();
    assert_eq!(first, "first answer");

    let second = client
        .ask("second")
        .send()
        .await
        .unwrap()
        .final_response()
        .await
        .unwrap();
    assert_eq!(second, "second answer");

    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn interrupted_ask_stream_ends_and_next_ask_works() {
    // Regression: the server aborts an interrupted ask without a terminal
    // frame; the actor must end the stream on the `interrupted` reply and
    // accept a new ask afterwards.
    let server = fake::spawn(|| {
        VecDeque::from([
            Step::Reply(vec![
                fake::ready_line("1", "req-1", "sess-1"),
                fake::event_line(
                    "req-1",
                    serde_json::json!({"type": "content_delta", "delta": "working..."}),
                ),
            ]),
            Step::Reply(vec![
                serde_json::json!({
                    "type": "interrupted",
                    "id": 2,
                    "request_id": "req-1",
                })
                .to_string(),
            ]),
            Step::Reply(vec![
                fake::ready_line("3", "req-2", "sess-1"),
                fake::result_line("req-2", "sess-1", "after interrupt"),
            ]),
        ])
    })
    .await;
    let client = connect(&server).await;

    let mut ask = client.ask("long running").send().await.unwrap();
    let first = ask.next_frame().await.unwrap().unwrap();
    assert!(matches!(first, ServerMessage::Event { .. }));

    client.interrupt().await.expect("interrupted");

    // The aborted ask's stream ends with the Interrupted error.
    let frame = ask.next_frame().await.unwrap().unwrap_err();
    assert!(matches!(frame, Error::Interrupted));
    assert!(ask.next_frame().await.is_none(), "stream is finished");

    // The connection accepts a new ask immediately.
    let answer = client
        .ask("next")
        .send()
        .await
        .unwrap()
        .final_response()
        .await
        .unwrap();
    assert_eq!(answer, "after interrupt");

    client.close().await;
    server.stop().await;
}
