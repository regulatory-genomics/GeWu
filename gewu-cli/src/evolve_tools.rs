use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{ToolContext, ToolResult};
use gewu_evolve::{PythonSandbox, SkillLibrary, ToolBuilder};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PythonExecuteArgs {
    pub code: String,
}

pub struct PythonExecuteTool {
    sandbox: PythonSandbox,
}

impl PythonExecuteTool {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            sandbox: PythonSandbox::new(timeout_secs),
        }
    }
}

#[async_trait]
impl Tool for PythonExecuteTool {
    fn name(&self) -> &str {
        "python_execute"
    }

    fn description(&self) -> &str {
        "Execute Python code in a sandboxed environment. Input is passed as JSON to stdin. Returns stdout, stderr, and result as JSON."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(PythonExecuteArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: PythonExecuteArgs = serde_json::from_value(args)
            .map_err(|e| format!("Failed to parse arguments: {}", e))?;

        let result = self.sandbox.execute(&input.code, serde_json::Value::Null).await?;

        let output = serde_json::json!({
            "stdout": result.stdout,
            "stderr": result.stderr,
            "result": result.result,
            "error": result.error,
            "timed_out": result.timed_out,
        });

        Ok(ToolResult::new("python_execute", output.to_string()))
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SkillCreateArgs {
    pub description: String,
}

pub struct SkillCreateTool {
    skill_dir: PathBuf,
    llm: Arc<dyn LlmProvider>,
}

impl SkillCreateTool {
    pub fn new(skill_dir: PathBuf, llm: Arc<dyn LlmProvider>) -> Self {
        Self { skill_dir, llm }
    }
}

#[async_trait]
impl Tool for SkillCreateTool {
    fn name(&self) -> &str {
        "skill_create"
    }

    fn description(&self) -> &str {
        "Create a new skill (tool) from a natural language description. The skill will be saved to the skill library."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(SkillCreateArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: SkillCreateArgs = serde_json::from_value(args)
            .map_err(|e| format!("Failed to parse arguments: {}", e))?;

        let generated = ToolBuilder::generate(&input.description, self.llm.as_ref())
            .await
            .map_err(|e| e.to_string())?;

        let library = SkillLibrary::new(self.skill_dir.clone())
            .map_err(|e| e.to_string())?;

        let skill = gewu_evolve::tool_builder::ToolBuilder::to_skill(&generated);
        library.save_skill(&skill)
            .await
            .map_err(|e| e.to_string())?;

        let output = serde_json::json!({
            "success": true,
            "skill_name": generated.name,
            "description": generated.description,
        });

        Ok(ToolResult::new("skill_create", output.to_string()))
    }
}

pub struct SkillListTool {
    skill_dir: PathBuf,
}

impl SkillListTool {
    pub fn new(skill_dir: PathBuf) -> Self {
        Self { skill_dir }
    }
}

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }

    fn description(&self) -> &str {
        "List all available skills in the skill library."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(())
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let library = SkillLibrary::new(self.skill_dir.clone())
            .map_err(|e| e.to_string())?;

        let skills = library.list_skills()
            .map_err(|e| e.to_string())?;

        let output = serde_json::json!({
            "skills": skills,
        });

        Ok(ToolResult::new("skill_list", output.to_string()))
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SkillLoadArgs {
    pub name: String,
}

pub struct SkillLoadTool {
    skill_dir: PathBuf,
}

impl SkillLoadTool {
    pub fn new(skill_dir: PathBuf) -> Self {
        Self { skill_dir }
    }
}

#[async_trait]
impl Tool for SkillLoadTool {
    fn name(&self) -> &str {
        "skill_load"
    }

    fn description(&self) -> &str {
        "Load a skill from the skill library by name."
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(SkillLoadArgs)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: SkillLoadArgs = serde_json::from_value(args)
            .map_err(|e| format!("Failed to parse arguments: {}", e))?;

        let library = SkillLibrary::new(self.skill_dir.clone())
            .map_err(|e| e.to_string())?;

        let skill = library.load_skill(&input.name)
            .await
            .map_err(|e| e.to_string())?;

        match skill {
            Some(s) => {
                let output = serde_json::json!({
                    "skill": s,
                });
                Ok(ToolResult::new("skill_load", output.to_string()))
            }
            None => Err(format!("Skill not found: {}", input.name).into()),
        }
    }
}