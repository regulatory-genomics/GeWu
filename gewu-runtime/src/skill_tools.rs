use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::{JsonSchema, schema::RootSchema};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::error::Error;
use std::sync::Arc;

use crate::skill_registry::SkillRegistry;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadSkillArgs {
    pub name: String,
}

pub struct LoadSkillTool {
    registry: Arc<SkillRegistry>,
    allowed_names: Option<BTreeSet<String>>,
}

impl LoadSkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self {
            registry,
            allowed_names: None,
        }
    }

    pub fn new_scoped(registry: Arc<SkillRegistry>, allowed_names: Vec<String>) -> Self {
        Self {
            registry,
            allowed_names: Some(allowed_names.into_iter().collect()),
        }
    }

    fn available_names(&self) -> Vec<String> {
        match &self.allowed_names {
            Some(allowed) => allowed.iter().cloned().collect(),
            None => self
                .registry
                .summaries()
                .into_iter()
                .map(|summary| summary.name)
                .collect(),
        }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for an available GeWu skill by name. Use this before following a skill from <available_skills>."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(LoadSkillArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: LoadSkillArgs = serde_json::from_value(args)?;
        let name = input.name.trim().to_string();
        if let Some(allowed) = &self.allowed_names
            && !allowed.contains(&name)
        {
            return Err(format!(
                "Skill is not available to this specialist: {}. Available skills: {}",
                name,
                self.available_names().join(", ")
            )
            .into());
        }
        let skill = self.registry.get(&name).ok_or_else(|| {
            let available = self.available_names().join(", ");
            format!("Unknown skill: {name}. Available skills: {available}")
        })?;

        let output = format!(
            "<skill_content name=\"{}\">\n{}\n</skill_content>",
            skill.name, skill.content
        );
        Ok(ToolResult::new("load_skill", output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::skill::SkillManifest;
    use gewu_core::traits::Tool;
    use gewu_core::types::ToolContext;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_ctx() -> ToolContext {
        ToolContext::new("test-session", "test-message")
    }

    fn registry_with_skill(name: &str, content: &str) -> Arc<SkillRegistry> {
        registry_with_skills(&[(name, content)])
    }

    fn registry_with_skills(skills: &[(&str, &str)]) -> Arc<SkillRegistry> {
        let mut registry = SkillRegistry::new();
        for (name, content) in skills {
            registry.insert(SkillManifest {
                name: (*name).to_string(),
                description: format!("{name} description"),
                capabilities: vec![],
                specialists: vec![],
                content: (*content).to_string(),
                location: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
            });
        }
        Arc::new(registry)
    }

    #[tokio::test]
    async fn load_skill_returns_wrapped_skill_content() {
        let tool = LoadSkillTool::new(registry_with_skill("alpha", "# Alpha\nDo work."));
        let result = tool
            .execute(json!({"name": "  alpha  "}), &test_ctx())
            .await
            .unwrap();

        assert_eq!(result.title, "load_skill");
        assert_eq!(
            result.output,
            "<skill_content name=\"alpha\">\n# Alpha\nDo work.\n</skill_content>"
        );
        assert!(!result.output.contains("/tmp/"));
    }

    #[tokio::test]
    async fn load_skill_unknown_skill_lists_available_names() {
        let tool = LoadSkillTool::new(registry_with_skill("alpha", "# Alpha\nDo work."));
        let err = tool
            .execute(json!({"name": "missing"}), &test_ctx())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Unknown skill: missing"));
        assert!(msg.contains("alpha"));
    }

    #[tokio::test]
    async fn scoped_load_skill_rejects_unlisted_skill() {
        let registry = registry_with_skills(&[("allowed", "# Allowed"), ("hidden", "# Hidden")]);
        let tool = LoadSkillTool::new_scoped(registry, vec!["allowed".to_string()]);

        let err = tool
            .execute(json!({"name": "hidden"}), &test_ctx())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Skill is not available to this specialist: hidden"));
        assert!(msg.contains("allowed"));
        assert!(!msg.contains("# Hidden"));
    }
}
