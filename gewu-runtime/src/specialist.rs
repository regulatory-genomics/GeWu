//! Specialist descriptors and deterministic assembly.

use std::collections::HashMap;
use std::sync::Arc;

use gewu_core::traits::{LlmProvider, Memory, Tool};
use gewu_core::{Agent, RunOptions};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum SpecialistError {
    #[error("specialist {specialist:?} requires tool {tool:?}, which is not registered")]
    UnknownTool { specialist: String, tool: String },
    #[error("tool {tool:?} is already registered")]
    DuplicateTool { tool: String },
    #[error("specialist {specialist:?} lists the same tool more than once: {tool:?}")]
    RepeatedTool { specialist: String, tool: String },
    #[error("specialist id must not be empty")]
    EmptyId,
    #[error("agent construction failed: {0}")]
    Build(String),
}

/// A specialist declaration.
///
/// `tools` is an exact allowlist, not a capability filter: a tool that is
/// registered but not listed is not offered to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpecialistDescriptor {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// System prompt. Loaded by the host from a file when it prefers.
    pub prompt: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default)]
    pub max_tool_calls: Option<usize>,
    /// Model context window used for compaction. `None` keeps GeWu's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<usize>,
}

impl SpecialistDescriptor {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn to_run_options(&self) -> RunOptions {
        RunOptions {
            max_turns: self.max_turns,
            max_tool_calls: self.max_tool_calls,
            deadline: None,
        }
    }
}

/// Tools the host has made available, keyed by name.
#[derive(Default)]
pub struct SpecialistRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl SpecialistRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one tool. Duplicate names are an error: silently replacing a
    /// tool would make the model's call mean something different than it did
    /// before.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), SpecialistError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(SpecialistError::DuplicateTool { tool: name });
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn register_all(
        &mut self,
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> Result<(), SpecialistError> {
        for tool in tools {
            self.register(tool)?;
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Registered tool names, sorted so the output is stable for assertions.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Resolve a descriptor's allowlist to tool instances.
    pub fn resolve(
        &self,
        descriptor: &SpecialistDescriptor,
    ) -> Result<Vec<Arc<dyn Tool>>, SpecialistError> {
        let mut seen = std::collections::HashSet::new();
        let mut tools = Vec::with_capacity(descriptor.tools.len());
        for name in &descriptor.tools {
            if !seen.insert(name.as_str()) {
                return Err(SpecialistError::RepeatedTool {
                    specialist: descriptor.id.clone(),
                    tool: name.clone(),
                });
            }
            let tool = self.tools.get(name).ok_or_else(|| SpecialistError::UnknownTool {
                specialist: descriptor.id.clone(),
                tool: name.clone(),
            })?;
            tools.push(tool.clone());
        }
        Ok(tools)
    }
}

/// Everything the host supplies that is not part of the declaration.
pub struct SpecialistBuild {
    pub llm: Arc<dyn LlmProvider>,
    pub memory: Option<Box<dyn Memory>>,
}

impl SpecialistRegistry {
    /// Assembles an agent from a declaration. The resulting agent can only call
    /// the tools the descriptor lists: no shell, filesystem or sub-agent tool is
    /// added implicitly.
    ///
    /// Budgets are per run, so they are applied by
    /// [`run_with`](Agent::run_with)`(message, descriptor.to_run_options())`,
    /// not here; `max_turns` is the one exception because the builder owns it.
    pub fn build(
        &self,
        descriptor: &SpecialistDescriptor,
        build: SpecialistBuild,
    ) -> Result<Agent, SpecialistError> {
        if descriptor.id.trim().is_empty() {
            return Err(SpecialistError::EmptyId);
        }
        let tools = self.resolve(descriptor)?;
        let mut builder = Agent::builder()
            .llm(build.llm)
            .with_tools(tools)
            .system_prompt(&descriptor.prompt)
            .max_turns(descriptor.max_turns);
        if let Some(memory) = build.memory {
            builder = builder.memory(memory);
        }
        if let Some(limit) = descriptor.context_limit {
            builder = builder.context_limit(limit);
        }
        builder.build().map_err(|error| SpecialistError::Build(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_round_trips_through_json() {
        let json = r#"{
            "id": "browser",
            "description": "Tachyon browser assistant",
            "prompt": "You drive a genome browser.",
            "tools": ["locus_set", "track_add"],
            "max_tool_calls": 12
        }"#;

        let descriptor = SpecialistDescriptor::from_json(json).expect("parse");

        assert_eq!(descriptor.id, "browser");
        assert_eq!(descriptor.tools, vec!["locus_set", "track_add"]);
        assert_eq!(descriptor.max_tool_calls, Some(12));
        assert_eq!(descriptor.to_run_options().max_tool_calls, Some(12));
    }

    #[test]
    fn an_empty_id_is_rejected() {
        let registry = SpecialistRegistry::new();
        let descriptor = SpecialistDescriptor {
            id: "  ".into(),
            description: String::new(),
            prompt: String::new(),
            tools: Vec::new(),
            max_turns: None,
            max_tool_calls: None,
            context_limit: None,
        };

        let error = registry
            .build(
                &descriptor,
                SpecialistBuild {
                    llm: unreachable_llm(),
                    memory: None,
                },
            )
            .err()
            .expect("empty id must be rejected");
        assert!(matches!(error, SpecialistError::EmptyId));
    }

    #[test]
    fn resolve_rejects_an_unregistered_tool() {
        let registry = SpecialistRegistry::new();
        let descriptor = SpecialistDescriptor {
            id: "browser".into(),
            description: String::new(),
            prompt: String::new(),
            tools: vec!["nope".into()],
            max_turns: None,
            max_tool_calls: None,
            context_limit: None,
        };

        let error = registry
            .resolve(&descriptor)
            .err()
            .expect("unknown tool must be rejected");
        assert!(matches!(error, SpecialistError::UnknownTool { .. }));
    }

    fn unreachable_llm() -> Arc<dyn LlmProvider> {
        Arc::new(PanicLlm)
    }

    struct PanicLlm;

    #[async_trait::async_trait]
    impl LlmProvider for PanicLlm {
        fn build(
            &self,
            _messages: &[gewu_core::Message],
            _tools: &[Arc<dyn Tool>],
        ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
            unimplemented!("not called in these tests")
        }

        async fn send<'a>(
            &'a self,
            _request: serde_json::Value,
        ) -> Result<
            futures::stream::BoxStream<'a, Result<gewu_core::ProviderEvent, Box<dyn std::error::Error + Send + Sync>>>,
            Box<dyn std::error::Error + Send + Sync>,
        > {
            unimplemented!("not called in these tests")
        }

        async fn list_models(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Vec::new())
        }
    }
}
