use gewu_core::types::{
    AgentEvent, Capabilities, ContentPart, FileAttachment, FunctionCall, InputCapabilities,
    Message, OutputCapabilities, ProviderEvent, TokenUsage, ToolCall, ToolContext, ToolResult,
    TruncationResult,
};

#[test]
fn test_content_part_mime_type_text() {
    let part = ContentPart::Text {
        text: "Hello".to_string(),
    };
    assert_eq!(part.mime_type(), "text/plain");
}

#[test]
fn test_content_part_mime_type_image() {
    let part = ContentPart::Image {
        data: "abc".to_string(),
        mime_type: "image/png".to_string(),
    };
    assert_eq!(part.mime_type(), "image/png");
}

#[test]
fn test_content_part_mime_type_audio() {
    let part = ContentPart::Audio {
        data: "abc".to_string(),
        mime_type: "audio/mp3".to_string(),
    };
    assert_eq!(part.mime_type(), "audio/mp3");
}

#[test]
fn test_content_part_mime_type_video() {
    let part = ContentPart::Video {
        data: "abc".to_string(),
        mime_type: "video/mp4".to_string(),
    };
    assert_eq!(part.mime_type(), "video/mp4");
}

#[test]
fn test_content_part_mime_type_file() {
    let part = ContentPart::File {
        data: "abc".to_string(),
        mime_type: "application/pdf".to_string(),
    };
    assert_eq!(part.mime_type(), "application/pdf");
}

#[test]
fn test_tool_result_new() {
    let result = ToolResult::new("test_tool", "output text");

    assert_eq!(result.title, "test_tool");
    assert_eq!(result.output, "output text");
    assert!(!result.truncated);
    assert!(result.output_path.is_none());
    assert!(result.metadata.is_empty());
    assert!(result.attachments.is_empty());
}

#[test]
fn test_tool_context_new() {
    let ctx = ToolContext::new("session-123", "msg-456");

    assert_eq!(ctx.session_id, "session-123");
    assert_eq!(ctx.message_id, "msg-456");
    assert!(!ctx.is_aborted());
}

#[test]
fn test_tool_context_abort() {
    let ctx = ToolContext::new("session-123", "msg-456");

    ctx.abort();
    assert!(ctx.is_aborted());
}

#[test]
fn test_tool_context_clone_shares_abort() {
    let ctx = ToolContext::new("session-123", "msg-456");
    let ctx2 = ctx.clone();

    ctx.abort();
    assert!(ctx.is_aborted());
    assert!(ctx2.is_aborted());
}

#[test]
fn test_truncation_result() {
    use std::path::PathBuf;

    let result = TruncationResult {
        content: "Truncated content".to_string(),
        truncated: true,
        output_path: Some(PathBuf::from("/tmp/output.txt")),
    };

    assert!(result.truncated);
    assert!(result.output_path.is_some());
}

#[test]
fn test_capabilities_default() {
    let caps = Capabilities::default();

    assert!(!caps.input.text);
    assert!(!caps.input.image);
    assert!(!caps.output.text);
    assert!(!caps.output.image);
}

#[test]
fn test_input_capabilities() {
    let caps = InputCapabilities {
        text: true,
        image: true,
        audio: false,
        video: false,
        pdf: true,
    };

    assert!(caps.text);
    assert!(caps.image);
    assert!(caps.pdf);
    assert!(!caps.audio);
    assert!(!caps.video);
}

#[test]
fn test_output_capabilities() {
    let caps = OutputCapabilities {
        text: true,
        image: false,
        audio: false,
        video: false,
        pdf: false,
    };

    assert!(caps.text);
    assert!(!caps.image);
}

#[test]
fn test_function_call() {
    let fc = FunctionCall {
        name: "get_weather".to_string(),
        arguments: "{\"city\":\"NYC\"}".to_string(),
    };

    assert_eq!(fc.name, "get_weather");
    assert_eq!(fc.arguments, "{\"city\":\"NYC\"}");
}

#[test]
fn test_tool_call() {
    let tc = ToolCall {
        id: "call_123".to_string(),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: "get_weather".to_string(),
            arguments: "{}".to_string(),
        },
    };

    assert_eq!(tc.id, "call_123");
    assert_eq!(tc.r#type, "function");
    assert_eq!(tc.function.name, "get_weather");
}

#[test]
fn test_message_system() {
    let msg = Message::System {
        content: "You are helpful".to_string(),
    };

    assert!(matches!(msg, Message::System { .. }));
}

#[test]
fn test_message_user() {
    let msg = Message::User {
        content: vec![
            ContentPart::Text {
                text: "Hello".to_string(),
            },
            ContentPart::Image {
                data: "base64".to_string(),
                mime_type: "image/png".to_string(),
            },
        ],
    };

    if let Message::User { content } = msg {
        assert_eq!(content.len(), 2);
    } else {
        panic!("Expected User message");
    }
}

#[test]
fn test_message_assistant_with_content() {
    let msg = Message::Assistant {
        content: Some("I can help".to_string()),
        reasoning: Some("thinking".to_string()),
        replay: None,
        tool_calls: None,
    };

    if let Message::Assistant {
        content,
        reasoning,
        replay: _,
        tool_calls,
    } = msg
    {
        assert_eq!(content.unwrap(), "I can help");
        assert_eq!(reasoning.unwrap(), "thinking");
        assert!(tool_calls.is_none());
    }
}

#[test]
fn test_message_assistant_with_tool_calls() {
    let msg = Message::Assistant {
        content: None,
        reasoning: None,
        replay: None,
        tool_calls: Some(vec![ToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: FunctionCall {
                name: "bash".to_string(),
                arguments: "{\"cmd\":\"ls\"}".to_string(),
            },
        }]),
    };

    if let Message::Assistant {
        content,
        reasoning,
        replay: _,
        tool_calls,
    } = msg
    {
        assert!(content.is_none());
        assert!(reasoning.is_none());
        let calls = tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "bash");
    }
}

#[test]
fn test_message_tool() {
    let msg = Message::Tool {
        tool_call_id: "call_123".to_string(),
        tool_name: Some("read_file".to_string()),
        content: "File contents".to_string(),
        media: vec![ContentPart::Image {
            data: "img".to_string(),
            mime_type: "image/png".to_string(),
        }],
    };

    if let Message::Tool {
        tool_call_id,
        tool_name,
        content,
        media,
    } = msg
    {
        assert_eq!(tool_call_id, "call_123");
        assert_eq!(tool_name.unwrap(), "read_file");
        assert_eq!(content, "File contents");
        assert_eq!(media.len(), 1);
    }
}

#[test]
fn test_provider_event_content_delta() {
    let event = ProviderEvent::ContentDelta {
        delta: "Hello".to_string(),
    };

    assert!(matches!(event, ProviderEvent::ContentDelta { .. }));
}

#[test]
fn test_provider_event_tool_call_start() {
    let event = ProviderEvent::ToolCallStart {
        id: "call_1".to_string(),
        name: "bash".to_string(),
    };

    if let ProviderEvent::ToolCallStart { id, name } = event {
        assert_eq!(id, "call_1");
        assert_eq!(name, "bash");
    }
}

#[test]
fn test_provider_event_finish() {
    let event = ProviderEvent::Finish {
        reason: Some("stop".to_string()),
    };

    if let ProviderEvent::Finish { reason } = event {
        assert_eq!(reason.unwrap(), "stop");
    }
}

#[test]
fn test_provider_event_error() {
    let event = ProviderEvent::Error {
        error: "Something went wrong".to_string(),
    };

    if let ProviderEvent::Error { error } = event {
        assert_eq!(error, "Something went wrong");
    }
}

#[test]
fn test_agent_event_content_delta() {
    let event = AgentEvent::ContentDelta {
        delta: "Hello".to_string(),
    };

    assert!(matches!(event, AgentEvent::ContentDelta { .. }));
}

#[test]
fn test_agent_event_tool_start() {
    let event = AgentEvent::ToolStart {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        input: "{}".to_string(),
    };

    if let AgentEvent::ToolStart { call_id, tool, input } = event {
        assert_eq!(call_id, "call-1");
        assert_eq!(tool, "bash");
        assert_eq!(input, "{}");
    }
}

#[test]
fn test_agent_event_tool_finish() {
    let event = AgentEvent::ToolFinish {
        call_id: "call-1".to_string(),
        tool: "bash".to_string(),
        title: "Directory listing".to_string(),
        output: "files...".to_string(),
        metadata: std::collections::HashMap::from([("count".to_string(), serde_json::json!(3))]),
    };

    if let AgentEvent::ToolFinish {
        call_id,
        tool,
        title,
        output,
        metadata,
    } = event
    {
        assert_eq!(call_id, "call-1");
        assert_eq!(tool, "bash");
        assert_eq!(title, "Directory listing");
        assert_eq!(output, "files...");
        assert_eq!(metadata["count"], serde_json::json!(3));
    }
}

#[test]
fn test_agent_event_tool_finish_deserializes_legacy_shape() {
    let event: AgentEvent = serde_json::from_value(serde_json::json!({
        "type": "tool_finish",
        "tool": "bash",
        "output": "files..."
    }))
    .unwrap();

    match event {
        AgentEvent::ToolFinish {
            call_id,
            tool,
            title,
            output,
            metadata,
        } => {
            assert!(call_id.is_empty(), "legacy payloads carry no call id");
            assert_eq!(tool, "bash");
            assert!(title.is_empty());
            assert_eq!(output, "files...");
            assert!(metadata.is_empty());
        }
        _ => panic!("Expected ToolFinish"),
    }
}

#[test]
fn test_agent_event_finish() {
    let event = AgentEvent::Finish {
        response: "Done".to_string(),
    };

    if let AgentEvent::Finish { response } = event {
        assert_eq!(response, "Done");
    }
}

#[test]
fn test_agent_event_usage_round_trip() {
    let event = AgentEvent::Usage {
        used_tokens: 34_000,
        context_limit: 128_000,
        provider: Some(TokenUsage {
            input_tokens: 1_000,
            cached_tokens: 30_000,
            cache_write_tokens: 3_000,
            output_tokens: 500,
            reasoning_tokens: 120,
        }),
    };

    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "usage");
    assert_eq!(json["used_tokens"], 34_000);
    assert_eq!(json["context_limit"], 128_000);
    // Provider usage serializes inline (not skipped) when present.
    assert_eq!(json["provider"]["cached_tokens"], 30_000);

    let decoded: AgentEvent = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, event);

    // Provider usage is optional: older event payloads (or providers that
    // report nothing) decode with `provider: None` and round-trip without
    // the field on the wire.
    let legacy: AgentEvent = serde_json::from_str(
        r#"{"type":"usage","used_tokens":10,"context_limit":100}"#,
    )
    .unwrap();
    assert_eq!(
        legacy,
        AgentEvent::Usage {
            used_tokens: 10,
            context_limit: 100,
            provider: None,
        }
    );
    let encoded = serde_json::to_string(&legacy).unwrap();
    assert!(!encoded.contains("provider"), "{encoded}");
}

#[test]
fn test_agent_event_error() {
    let event = AgentEvent::Error {
        error: "Failed".to_string(),
    };

    if let AgentEvent::Error { error } = event {
        assert_eq!(error, "Failed");
    }
}

#[test]
fn test_file_attachment() {
    use std::path::PathBuf;

    let attachment = FileAttachment {
        path: PathBuf::from("/tmp/file.txt"),
        mime: Some("text/plain".to_string()),
        size: 1024,
    };

    assert_eq!(attachment.path, PathBuf::from("/tmp/file.txt"));
    assert_eq!(attachment.mime.unwrap(), "text/plain");
    assert_eq!(attachment.size, 1024);
}
