use gewu_cli::{find_descriptor, resolve_tools, specialist_descriptors};
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent};

use async_trait::async_trait;
use futures::stream::BoxStream;
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::sync::Arc;

// Minimal mock LLM — never called, only needed to satisfy resolve_tools for Task capability
struct StubLlm;

#[async_trait]
impl LlmProvider for StubLlm {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        unimplemented!("stub")
    }
    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

#[test]
fn test_explore_descriptor_exists_in_registry() {
    let desc = find_descriptor("explore");
    assert!(desc.is_some(), "explore must be in the descriptor registry");
    let desc = desc.unwrap();
    assert_eq!(desc.id, "explore");
    assert!(desc.capabilities.contains(&"bash_safe".to_string()));
    assert!(!desc.capabilities.contains(&"write_file".to_string()));
    assert!(!desc.capabilities.contains(&"edit_file".to_string()));
    assert!(!desc.capabilities.contains(&"task".to_string()));
}

#[test]
fn test_all_specialists_resolvable_from_descriptors() {
    for desc in specialist_descriptors() {
        assert!(
            !desc.capabilities.is_empty(),
            "{} has no capabilities",
            desc.id
        );
        assert!(
            !desc.system_prompt.is_empty(),
            "{} has no system prompt",
            desc.id
        );
    }
}

#[tokio::test]
async fn test_resolve_tools_produces_correct_tool_names() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);
    let wd = std::env::current_dir().unwrap();

    let desc = find_descriptor("explore").unwrap();
    let tools = resolve_tools(&desc.capabilities, &wd, provider, pool);

    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();

    // Explore should have these tools
    assert!(
        names.contains(&"read_file"),
        "missing read_file, got: {:?}",
        names
    );
    assert!(names.contains(&"glob"), "missing glob, got: {:?}", names);
    assert!(names.contains(&"bash"), "missing bash, got: {:?}", names);
    assert!(names.contains(&"grep"), "missing grep, got: {:?}", names);

    // Explore should NOT have these tools
    assert!(
        !names.contains(&"write_file"),
        "explore should not have write_file"
    );
    assert!(
        !names.contains(&"edit_file"),
        "explore should not have edit_file"
    );
    assert!(!names.contains(&"task"), "explore should not have task");
}

#[tokio::test]
async fn test_subset_capabilities_produce_subset_tools() {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);
    let wd = std::env::current_dir().unwrap();

    // Only two capabilities
    let caps: Vec<String> = vec!["read_file".into(), "glob".into()];
    let tools = resolve_tools(&caps, &wd, provider, pool);

    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"glob"));
}
