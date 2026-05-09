use crate::fs_utils::into_tool_error;
use anyhow::Result;
use async_trait::async_trait;
use gewu_core::traits::{Tool, ToolRegistry};
use gewu_core::types::{ToolContext, ToolExecutionPolicy, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;
use tokio::task::JoinSet;

const DISALLOWED: &[&str] = &["batch", "invalid", "patch"];
const MAX_TOOLS: usize = 25;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ToolCallInput {
    tool: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BatchArgs {
    tool_calls: Vec<ToolCallInput>,
}

pub struct BatchTool {
    pub registry: Arc<dyn ToolRegistry>,
}

impl BatchTool {
    pub fn new(registry: Arc<dyn ToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }

    fn description(&self) -> &str {
        "Execute multiple tools in parallel. Use this to run independent tool calls simultaneously for better performance."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(BatchArgs)
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let batch_args: BatchArgs = serde_json::from_value(args).map_err(into_tool_error)?;

        let tool_calls = batch_args
            .tool_calls
            .iter()
            .take(MAX_TOOLS)
            .collect::<Vec<_>>();
        let discarded = batch_args.tool_calls.len() - tool_calls.len();

        let available_tools = self.registry.list_tools();
        let allowed_tools = available_tools
            .iter()
            .filter(|t| !DISALLOWED.contains(&t.as_str()))
            .cloned()
            .collect::<std::collections::HashSet<_>>();

        let mut join_set = JoinSet::new();

        for call in &tool_calls {
            let tool_name = call.tool.clone();
            let params = call.parameters.clone();
            let registry = self.registry.clone();
            let allowed = allowed_tools.clone();

            if tool_name.is_empty() {
                let tool_name_clone = tool_name.clone();
                join_set.spawn(async move { (tool_name_clone, Err("Empty tool name".into())) });
                continue;
            }

            let ctx = ToolContext::new("batch", "batch");
            join_set.spawn(async move {
                let result = async {
                    if DISALLOWED.contains(&tool_name.as_str()) {
                        return Err(format!(
                            "Tool '{tool_name}' is not allowed in batch. Disallowed tools: {DISALLOWED:?}",
                        )
                        .into());
                    }

                    if !allowed.contains(&tool_name) {
                        return Err(format!(
                            "Tool '{}' not in registry. Available tools: {}",
                            tool_name,
                            allowed.iter().cloned().collect::<Vec<_>>().join(", ")
                        )
                        .into());
                    }

                    match registry.execution_policy(&tool_name) {
                        Some(ToolExecutionPolicy::ParallelSafe) => {}
                        Some(policy) => {
                            return Err(format!(
                                "Tool '{tool_name}' is not allowed in batch because its execution policy is {policy:?}; batch only runs ParallelSafe tools",
                            )
                            .into());
                        }
                        None => {
                            return Err(format!(
                                "Tool '{tool_name}' is not allowed in batch because its execution policy is unknown",
                            )
                            .into());
                        }
                    }

                    registry.execute(&tool_name, params, &ctx).await
                }
                .await;

                (tool_name, result)
            });
        }

        let mut results = Vec::new();
        while let Some(result) = join_set.join_next().await {
            if let Ok((tool_name, res)) = result {
                results.push((tool_name, res));
            }
        }

        let mut errors = Vec::new();
        let mut successes = Vec::new();

        for (tool_name, result) in &results {
            match result {
                Ok(output) => successes.push((tool_name.clone(), output.output.clone())),
                Err(e) => errors.push((tool_name.clone(), e.to_string())),
            }
        }

        let total = results.len();
        let successful = successes.len();
        let failed = errors.len();

        let mut output = String::new();

        if failed > 0 {
            output.push_str(&format!(
                "Executed {successful}/{total} tools successfully. {failed} failed.\n\n",
            ));
        } else {
            output.push_str(&format!(
                "All {successful} tools executed successfully.\n\n",
            ));
        }

        if !successes.is_empty() {
            output.push_str("Results:\n");
            for (tool_name, _) in &successes {
                output.push_str(&format!("- {tool_name}: success\n"));
            }
        }

        if !errors.is_empty() {
            output.push_str("\nErrors:\n");
            for (tool_name, err) in &errors {
                output.push_str(&format!("- {tool_name}: {err}\n"));
            }
        }

        if discarded > 0 {
            output.push_str(&format!(
                "\nNote: {discarded} tool(s) discarded (max {MAX_TOOLS} allowed)",
            ));
        }

        if successful == total {
            output.push_str(
                "\n\nKeep using the batch tool for optimal performance in your next response!",
            );
        }

        Ok(ToolResult::new("batch", output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestRegistry {
        policies: BTreeMap<String, ToolExecutionPolicy>,
        executions: AtomicUsize,
    }

    #[async_trait]
    impl ToolRegistry for TestRegistry {
        async fn execute(
            &self,
            name: &str,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::new(name, "ok"))
        }

        fn list_tools(&self) -> Vec<String> {
            self.policies.keys().cloned().collect()
        }

        fn execution_policy(&self, name: &str) -> Option<ToolExecutionPolicy> {
            self.policies.get(name).copied()
        }
    }

    #[tokio::test]
    async fn batch_rejects_non_parallel_safe_tools_without_executing_them() {
        let registry = Arc::new(TestRegistry {
            policies: BTreeMap::from([(
                "resource.register".to_string(),
                ToolExecutionPolicy::Serial,
            )]),
            executions: AtomicUsize::new(0),
        });
        let batch = BatchTool::new(registry.clone());

        let result = batch
            .execute(
                serde_json::json!({
                    "tool_calls": [{
                        "tool": "resource.register",
                        "parameters": {"dry_run": true}
                    }]
                }),
                &ToolContext::new("s1", "m1"),
            )
            .await
            .unwrap();

        assert_eq!(registry.executions.load(Ordering::SeqCst), 0);
        assert!(result.output.contains("0/1 tools successfully"));
        assert!(result.output.contains("resource.register"));
        assert!(result.output.contains("ParallelSafe"));
    }

    #[tokio::test]
    async fn batch_runs_parallel_safe_tools() {
        let registry = Arc::new(TestRegistry {
            policies: BTreeMap::from([(
                "resource.search".to_string(),
                ToolExecutionPolicy::ParallelSafe,
            )]),
            executions: AtomicUsize::new(0),
        });
        let batch = BatchTool::new(registry.clone());

        let result = batch
            .execute(
                serde_json::json!({
                    "tool_calls": [{
                        "tool": "resource.search",
                        "parameters": {"query": "docs"}
                    }]
                }),
                &ToolContext::new("s1", "m1"),
            )
            .await
            .unwrap();

        assert_eq!(registry.executions.load(Ordering::SeqCst), 1);
        assert!(result.output.contains("All 1 tools executed successfully"));
    }
}
