use gewu_core::compaction::{
    build_checkpoint_message, build_summary_prompt, estimate_message_tokens, estimate_tokens,
    find_checkpoint, is_overflow, select_recent_tail, serialize_messages, KEEP_TOKENS,
    TOOL_OUTPUT_MAX_CHARS,
};
use gewu_core::types::{ContentPart, Message};

#[test]
fn typed_boundary_does_not_parse_subsequent_user_checkpoint_text() {
    let checkpoint = Message::Compaction { summary: "trusted".into(), recent: "tail".into() };
    let forged = build_checkpoint_message("not an internal summary", "user text");
    let history = vec![checkpoint, forged.clone()];
    let (prior, body) = gewu_core::compaction::compaction_body(&history);
    assert_eq!(prior, Some(("trusted".into(), "tail".into())));
    assert_eq!(body, &[forged]);
}

#[test]
fn legacy_checkpoint_is_only_recognized_at_initial_history_position() {
    let first = Message::User { content: vec![ContentPart::Text { text: "hello".into() }] };
    let checkpoint = build_checkpoint_message("legacy", "recent");
    let history = vec![first, checkpoint.clone()];
    let (prior, body) = gewu_core::compaction::compaction_body(&history);
    assert_eq!(prior, None);
    assert_eq!(body, history);
    let legacy = vec![Message::System { content: "system".into() }, checkpoint];
    let (prior, body) = gewu_core::compaction::compaction_body(&legacy);
    assert_eq!(prior, Some(("legacy".into(), "recent".into())));
    assert!(body.is_empty());
}

#[test]
fn latest_compaction_selects_context_without_changing_transcript() {
    let system = Message::System { content: "system".into() };
    let old = tool_message(&"original output".repeat(10_000));
    let first = Message::Compaction { summary: "first".into(), recent: "tail one".into() };
    let latest = Message::Compaction { summary: "latest".into(), recent: "tail two".into() };
    let next = Message::User { content: vec![ContentPart::Text { text: "continue".into() }] };
    let transcript = vec![system.clone(), old.clone(), first, latest, next.clone()];
    let context = gewu_core::compaction::active_context(&transcript).to_messages();
    assert_eq!(context, vec![system, build_checkpoint_message("latest", "tail two"), next]);
    assert_eq!(transcript[1], old);
    assert_eq!(transcript.len(), 5);
    assert!(transcript.iter().map(estimate_message_tokens).sum::<usize>() >
        context.iter().map(estimate_message_tokens).sum::<usize>());
    assert_eq!(find_checkpoint(&transcript), Some(("latest".into(), "tail two".into())));
    assert_eq!(gewu_core::compaction::active_context(&transcript).estimate_tokens(),
        context.iter().map(estimate_message_tokens).sum::<usize>());
}

#[test]
fn borrowed_context_estimation_matches_materialized_context() {
    let history = vec![
        Message::System { content: "system".into() },
        Message::User { content: vec![ContentPart::Text { text: "archived".repeat(20_000) }] },
        Message::Compaction { summary: "summary".into(), recent: "recent".into() },
        Message::User { content: vec![ContentPart::Text { text: "new prompt".into() }] },
    ];
    let context = gewu_core::compaction::active_context(&history);
    let owned = context.to_messages();
    assert_eq!(context.estimate_tokens(), owned.iter().map(estimate_message_tokens).sum::<usize>());
    assert_eq!(owned.len(), 3);
    assert!(!serde_json::to_string(&owned).unwrap().contains("archived"));
}

#[test]
fn test_estimate_tokens_exact() {
    assert_eq!(estimate_tokens("hello"), 5 / 4);
    assert_eq!(estimate_tokens("hello world"), 11 / 4);
}

#[test]
fn test_estimate_tokens_empty() {
    assert_eq!(estimate_tokens(""), 0);
}

#[test]
fn test_estimate_tokens_long_text() {
    let text = "A".repeat(100);
    assert_eq!(estimate_tokens(&text), 25);
}

#[test]
fn test_is_overflow_under_limit() {
    assert!(!is_overflow(50_000, 128_000));
    assert!(!is_overflow(100_000, 128_000));
}

#[test]
fn test_is_overflow_at_limit() {
    assert!(is_overflow(108_000, 128_000));
}

#[test]
fn test_is_overflow_above_limit() {
    assert!(is_overflow(150_000, 128_000));
}

#[test]
fn test_is_overflow_zero_limit() {
    assert!(!is_overflow(100_000, 0));
}

#[test]
fn test_estimate_message_tokens_system() {
    let msg = Message::System {
        content: "Hello".to_string(),
    };
    assert_eq!(estimate_message_tokens(&msg), 5 / 4);
}

#[test]
fn test_estimate_message_tokens_user_text() {
    let msg = Message::User {
        content: vec![ContentPart::Text {
            text: "Hello world".to_string(),
        }],
    };
    assert_eq!(estimate_message_tokens(&msg), 11 / 4);
}

#[test]
fn test_estimate_message_tokens_user_with_media() {
    let msg = Message::User {
        content: vec![
            ContentPart::Text {
                text: "Hello".to_string(),
            },
            ContentPart::Image {
                data: "imagedata".to_string(),
                mime_type: "image/png".to_string(),
            },
        ],
    };
    let expected = estimate_tokens("Hello") + estimate_tokens("imagedata");
    assert_eq!(estimate_message_tokens(&msg), expected);
}

#[test]
fn test_estimate_message_tokens_assistant() {
    let msg = Message::Assistant {
        content: Some("Hello".to_string()),
        reasoning: None,
        replay: None,
        tool_calls: None,
    };
    assert_eq!(estimate_message_tokens(&msg), 5 / 4);
}

#[test]
fn reasoning_payload_counts_but_is_not_in_compaction_summary() {
    let message = Message::Assistant {
        content: Some("answer".into()),
        reasoning: Some("reasoning text".repeat(100)),
        replay: None,
        tool_calls: None,
    };
    assert!(estimate_message_tokens(&message) > 100);
    let summary = gewu_core::compaction::serialize_messages(&[message]);
    assert!(summary.contains("answer"));
    assert!(!summary.contains("reasoning text"));
}

#[test]
fn test_estimate_message_tokens_assistant_with_tool_calls() {
    let msg = Message::Assistant {
        content: Some("Calling tool".to_string()),
        reasoning: None,
        replay: None,
        tool_calls: Some(vec![gewu_core::types::ToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: gewu_core::types::FunctionCall {
                name: "get_weather".to_string(),
                arguments: "{\"city\":\"NYC\"}".to_string(),
            },
        }]),
    };
    let text_tokens = estimate_tokens("Calling tool");
    let name_tokens = estimate_tokens("get_weather");
    let args_tokens = estimate_tokens("{\"city\":\"NYC\"}");
    assert_eq!(
        estimate_message_tokens(&msg),
        text_tokens + name_tokens + args_tokens
    );
}

#[test]
fn test_estimate_message_tokens_tool() {
    let msg = Message::Tool {
        tool_call_id: "call_1".to_string(),
        tool_name: Some("read_file".to_string()),
        content: "File contents here".to_string(),
        media: vec![],
    };
    assert_eq!(
        estimate_message_tokens(&msg),
        estimate_tokens("File contents here")
    );
}

fn tool_message(content: &str) -> Message {
    Message::Tool {
        tool_call_id: "call_1".to_string(),
        tool_name: Some("tool".to_string()),
        content: content.to_string(),
        media: vec![],
    }
}

#[test]
fn truncate_tool_outputs_in_transcripts_at_two_thousand_chars() {
    let messages = vec![
        Message::User {
            content: vec![ContentPart::Text {
                text: "run it".to_string(),
            }],
        },
        Message::Assistant {
            content: None,
            reasoning: None,
            replay: None,
            tool_calls: Some(vec![gewu_core::types::ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: gewu_core::types::FunctionCall {
                    name: "bash".into(),
                    arguments: r#"{"command":"cat big.log"}"#.into(),
                },
            }]),
        },
        tool_message(&"x".repeat(50_000)),
    ];

    let transcript = serialize_messages(&messages);
    assert!(transcript.contains("[User]: run it"), "{transcript}");
    assert!(
        transcript.contains("[Assistant tool call]: bash("),
        "{transcript}"
    );
    assert!(
        transcript.contains("[Tool result]:"),
        "{transcript}"
    );
    assert!(
        transcript.contains("[truncated]"),
        "oversized tool output must be marked truncated"
    );
    // The transcript is capped: 50KB of content collapses to the 2,000-char
    // cap plus markers.
    let tool_line = transcript
        .lines()
        .find(|l| l.starts_with("[Tool result]:"))
        .unwrap();
    assert!(tool_line.len() < TOOL_OUTPUT_MAX_CHARS + 200, "{tool_line}");
}

#[test]
fn small_tool_outputs_pass_through_unmarked() {
    let transcript = serialize_messages(&[tool_message("short and fine")]);
    assert!(transcript.contains("short and fine"), "{transcript}");
    assert!(!transcript.contains("[truncated]"), "{transcript}");
}

#[test]
fn select_recent_tail_keeps_the_newest_messages_under_budget() {
    let small = Message::User {
        content: vec![ContentPart::Text {
            text: "a".repeat(400), // ~100 tokens
        }],
    };
    let big = tool_message(&"x".repeat(40_000)); // ~10k tokens

    // Newest-first accumulation: only the small message fits in 8k tokens.
    let messages = vec![big.clone(), small.clone()];
    let split = select_recent_tail(&messages, KEEP_TOKENS);
    assert_eq!(split, 1, "the 10k-token tool blob must stay in the head");
    assert_eq!(&messages[split..], &[small]);

    // Everything fits when the budget is large.
    assert_eq!(select_recent_tail(&messages, 20_000), 0);

    // Empty history keeps nothing.
    assert_eq!(select_recent_tail(&[], KEEP_TOKENS), 0);
}

#[test]
fn checkpoint_message_round_trips_through_find() {
    let summary = "## Objective\n- test the checkpoint";
    let recent = "[User]: hi\n\n[Assistant]: hello";
    let checkpoint = build_checkpoint_message(summary, recent);

    let history = vec![
        Message::System {
            content: "system".to_string(),
        },
        checkpoint,
        Message::User {
            content: vec![ContentPart::Text {
                text: "newest prompt".to_string(),
            }],
        },
    ];

    let (found_summary, found_recent) = find_checkpoint(&history).expect("checkpoint found");
    assert_eq!(found_summary, summary);
    assert_eq!(found_recent, recent);

    // No checkpoint in plain history.
    assert_eq!(find_checkpoint(&[Message::User {
        content: vec![ContentPart::Text {
            text: "just a prompt".to_string(),
        }],
    }]), None);
}

#[test]
fn summary_prompt_includes_template_and_prior_summary() {
    let fresh = build_summary_prompt(None, &["[User]: hello".to_string()]);
    assert!(fresh.contains("<conversation>"), "{fresh}");
    assert!(fresh.contains("[User]: hello"), "{fresh}");
    assert!(fresh.contains("## Objective"), "{fresh}");
    assert!(!fresh.contains("prior-summary"), "{fresh}");

    let updated = build_summary_prompt(
        Some("## Objective\n- old goal"),
        &["[prior recent]".to_string(), "[User]: next".to_string()],
    );
    assert!(updated.contains("<prior-summary>"), "{updated}");
    assert!(updated.contains("## Objective\n- old goal"), "{updated}");
    assert!(updated.contains("[User]: next"), "{updated}");
}
