use gewu_providers::llm::anthropic::parse_fixture_events as parse_anthropic_events;
use gewu_providers::llm::openai::parse_fixture_events as parse_openai_events;

#[test]
fn openai_stream_fixture_maps_content_and_finish_events() {
    let fixture = include_str!("fixtures/openai_stream.sse");
    let events = parse_openai_events(fixture).expect("parse openai fixture");
    assert_eq!(events, vec!["content:hello", "finish:stop"]);
}

#[test]
fn anthropic_stream_fixture_maps_text_and_finish_events() {
    let fixture = include_str!("fixtures/anthropic_stream.sse");
    let events = parse_anthropic_events(fixture).expect("parse anthropic fixture");
    assert_eq!(events, vec!["content:hello", "finish:stop"]);
}
