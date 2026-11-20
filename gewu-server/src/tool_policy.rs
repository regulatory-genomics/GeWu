//! Per-mode execution policies for the shared tool set.
//!
//! Modes share one registry so a request's tool prefix — and any provider
//! prompt cache keyed on it — stays identical across switches. A mode's
//! restrictions live here as data: which advertised tools may not run, which
//! subagents `task` may spawn, and whether external (MCP) tools are refused.

use std::sync::Arc;

use gewu_core::agent::ToolPolicy;
use gewu_core::traits::Tool;

/// How one mode uses the shared tool set.
pub struct ModeToolPolicy {
    /// Mode name, used for documentation and diagnostics.
    pub mode: &'static str,
    /// Built-ins advertised but refused at execution.
    pub denied: &'static [&'static str],
    /// Subagents the `task` tool may spawn; `None` allows every advertised
    /// subagent.
    pub task_allowlist: Option<&'static [&'static str]>,
    /// Whether externally supplied (MCP/additional) tools are refused.
    pub deny_external: bool,
    /// Model-facing refusal message.
    pub reason: &'static str,
}

/// Build/general: everything advertised may run.
pub const UNRESTRICTED: ModeToolPolicy = ModeToolPolicy {
    mode: "build",
    denied: &[],
    task_allowlist: None,
    deny_external: false,
    reason: "",
};

/// Plan: read-only. Editing, workflow control, and external tools stay
/// advertised for prefix stability but refuse to execute.
pub const PLAN: ModeToolPolicy = ModeToolPolicy {
    mode: "plan",
    denied: &[
        "apply_patch",
        "write_file",
        "edit_file",
        "workflow_start",
        "workflow_status",
        "workflow_cancel",
    ],
    task_allowlist: Some(&["explore"]),
    deny_external: true,
    reason: "Plan mode is read-only; switch to build to make changes.",
};

/// The policy for a profile or mode name.
pub fn policy_for(mode: &str) -> &'static ModeToolPolicy {
    match mode {
        "plan" => &PLAN,
        _ => &UNRESTRICTED,
    }
}

impl ModeToolPolicy {
    /// Whether the task tool may spawn `name`.
    pub fn task_allows(&self, name: &str) -> bool {
        self.task_allowlist
            .is_none_or(|allowed| allowed.contains(&name))
    }

    /// Converts this mode policy into the agent's execution policy, adding
    /// external tool names when the mode refuses them.
    pub fn execution_policy(&self, external: &[Arc<dyn Tool>]) -> ToolPolicy {
        let mut denied: Vec<String> = self.denied.iter().map(|name| name.to_string()).collect();
        if self.deny_external {
            denied.extend(external.iter().map(|tool| tool.name().to_string()));
        }
        if denied.is_empty() {
            ToolPolicy::allow_all()
        } else {
            ToolPolicy::deny(denied, self.reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_mode_denies_nothing() {
        let policy = policy_for("build");
        assert_eq!(policy.mode, "build");
        assert!(policy.task_allows("general"));
        assert_eq!(policy.execution_policy(&[]), ToolPolicy::allow_all());
    }

    #[test]
    fn plan_denies_edits_workflows_and_external_tools() {
        let policy = policy_for("plan");
        assert!(policy.task_allows("explore"));
        assert!(!policy.task_allows("general"));

        let external: Vec<Arc<dyn Tool>> = vec![Arc::new(NamedTool("mcp_write"))];
        let execution = policy.execution_policy(&external);
        for name in [
            "apply_patch",
            "write_file",
            "edit_file",
            "workflow_start",
            "workflow_status",
            "workflow_cancel",
            "mcp_write",
        ] {
            assert_eq!(
                execution.check(name),
                Err(policy.reason.to_string()),
                "{name} must be refused"
            );
        }
        assert_eq!(execution.check("read_file"), Ok(()));
        assert_eq!(execution.check("bash"), Ok(()));
    }

    /// A future mode only needs new data; the wrapper mechanism is shared.
    #[test]
    fn a_new_mode_is_just_data() {
        let policy = ModeToolPolicy {
            mode: "review",
            denied: &["write_file"],
            task_allowlist: Some(&["explore"]),
            deny_external: false,
            reason: "Review mode is read-only.",
        };
        let external: Vec<Arc<dyn Tool>> = vec![Arc::new(NamedTool("mcp_write"))];
        let execution = policy.execution_policy(&external);

        assert_eq!(
            execution.check("write_file"),
            Err(policy.reason.to_string())
        );
        assert_eq!(execution.check("mcp_write"), Ok(()));
        assert_eq!(execution.check("read_file"), Ok(()));
    }

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema_for!(serde_json::Value)
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &gewu_core::types::ToolContext,
        ) -> Result<gewu_core::types::ToolResult, Box<dyn std::error::Error + Send + Sync>>
        {
            Ok(gewu_core::types::ToolResult::new(self.0, "ok"))
        }
    }
}
