//! The Composer specialist: a built-in specialist whose purpose is to emit
//! new `WorkflowManifest`s.
//!
//! Three tools:
//!
//! - `list_specialists` — snapshot of all loaded specialists (id, description,
//!   capabilities).
//! - `list_capabilities` — snapshot of all registered capabilities (name,
//!   kind, description).
//! - `write_workflow` — validates a YAML manifest against the current
//!   snapshot and persists it to a workflows directory (default
//!   `~/.gewu/workflows/<id>.yaml`).
//!
//! The composer specialist itself is defined via a YAML manifest shipped
//! under `manifests/composer.yaml` (loaded via the plugin-style
//! `composer_manifest_dir` helper below).

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use gewu_core::traits::{AssemblyContext, Tool, ToolFactory};
use gewu_core::types::{CapabilityDescriptor, SpecialistDescriptor, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use crate::command_tool::CommandToolDef;
use crate::command_tool_registry::CommandToolRegistry;
use crate::manifest::workflow::WorkflowManifest;
use crate::workflow;

// ---------------------------------------------------------------------------
// Shared snapshot context
// ---------------------------------------------------------------------------

/// Immutable snapshot of the runtime state the composer tools read from.
#[derive(Clone)]
pub struct ComposerContext {
    pub specialists: Arc<Vec<SpecialistDescriptor>>,
    pub capabilities: Arc<Vec<CapabilityDescriptor>>,
    pub workflows: Arc<HashMap<String, WorkflowManifest>>,
    pub output_dir: PathBuf,
}

impl ComposerContext {
    pub fn specialist_ids(&self) -> HashSet<String> {
        self.specialists.iter().map(|s| s.id.clone()).collect()
    }

    pub fn capability_names(&self) -> HashSet<String> {
        self.capabilities.iter().map(|c| c.name.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// Tool: list_specialists
// ---------------------------------------------------------------------------

pub struct ListSpecialistsTool {
    ctx: ComposerContext,
}

impl ListSpecialistsTool {
    pub fn new(ctx: ComposerContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for ListSpecialistsTool {
    fn name(&self) -> &str {
        "list_specialists"
    }
    fn description(&self) -> &str {
        "List every specialist available to the composer. Returns an array \
         of { id, description, capabilities }. Call this before emitting a \
         workflow so you know which specialists you can reference."
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
        .unwrap()
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let items: Vec<Value> = self
            .ctx
            .specialists
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "description": s.description,
                    "capabilities": s.capabilities,
                })
            })
            .collect();
        Ok(ToolResult::new(
            "list_specialists",
            serde_json::to_string(&items).unwrap_or_default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: list_capabilities
// ---------------------------------------------------------------------------

pub struct ListCapabilitiesTool {
    ctx: ComposerContext,
}

impl ListCapabilitiesTool {
    pub fn new(ctx: ComposerContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for ListCapabilitiesTool {
    fn name(&self) -> &str {
        "list_capabilities"
    }
    fn description(&self) -> &str {
        "List every capability (tool) available to the composer. Returns an \
         array of { name, description, provider_kind }. Use this to decide \
         which capabilities a workflow's `kind: tool` nodes may reference."
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
        .unwrap()
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let items: Vec<Value> = self
            .ctx
            .capabilities
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "description": c.description,
                    "provider_kind": c.provider_kind,
                })
            })
            .collect();
        Ok(ToolResult::new(
            "list_capabilities",
            serde_json::to_string(&items).unwrap_or_default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: write_workflow
// ---------------------------------------------------------------------------

pub struct WriteWorkflowTool {
    ctx: ComposerContext,
}

impl WriteWorkflowTool {
    pub fn new(ctx: ComposerContext) -> Self {
        Self { ctx }
    }
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    id: String,
    #[serde(default)]
    manifest_yaml: Option<String>,
    #[serde(default)]
    manifest_json: Option<Value>,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for WriteWorkflowTool {
    fn name(&self) -> &str {
        "write_workflow"
    }
    fn description(&self) -> &str {
        "Validate and persist a workflow manifest. Provide either \
         `manifest_yaml` (string, preferred) or `manifest_json` (object). \
         The `id` must match the manifest's own `id`. On success the file is \
         written to the composer's output directory and its path is \
         returned. The manifest must use `entry: <node_id>`, map-shaped \
         `nodes:`, `finish: [<node_id>]`, and `edges: [[from, to]]`. On \
         validation failure a structured error is returned so you can revise \
         one specific issue and retry."
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Workflow id (must match manifest.id)" },
                "manifest_yaml": { "type": "string", "description": "Full YAML source of the manifest" },
                "manifest_json": { "type": "object", "description": "Structured manifest object (alternative to manifest_yaml)" },
                "overwrite": { "type": "boolean", "description": "Allow overwriting an existing workflow file (default false)" }
            },
            "required": ["id"]
        }))
        .unwrap()
    }
    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let args: WriteArgs =
            serde_json::from_value(args).map_err(|e| format!("invalid args: {}", e))?;

        let result = do_write(&self.ctx, args).await;
        match result {
            Ok(out) => Ok(ToolResult::new(
                "write_workflow",
                serde_json::to_string(&out).unwrap_or_default(),
            )),
            Err(e) => {
                let err_out = json!({
                    "ok": false,
                    "error": e.to_string(),
                });
                Ok(ToolResult::new(
                    "write_workflow",
                    serde_json::to_string(&err_out).unwrap_or_default(),
                ))
            }
        }
    }
}

async fn do_write(ctx: &ComposerContext, args: WriteArgs) -> Result<Value> {
    let manifest: WorkflowManifest = if let Some(yaml) = args.manifest_yaml.as_deref() {
        serde_yaml::from_str(yaml).map_err(|e| anyhow!("parse manifest_yaml: {}", e))?
    } else if let Some(obj) = args.manifest_json {
        serde_json::from_value(obj).map_err(|e| anyhow!("parse manifest_json: {}", e))?
    } else {
        return Err(anyhow!(
            "must provide either `manifest_yaml` or `manifest_json`"
        ));
    };

    if manifest.id != args.id {
        return Err(anyhow!(
            "id mismatch: tool arg `{}` vs manifest `{}`",
            args.id,
            manifest.id
        ));
    }

    let spec_ids = ctx.specialist_ids();
    let cap_names = ctx.capability_names();
    workflow::validate_with_ids(&manifest, &spec_ids, &cap_names, &ctx.workflows)?;

    std::fs::create_dir_all(&ctx.output_dir)
        .map_err(|e| anyhow!("create output dir {}: {}", ctx.output_dir.display(), e))?;
    let path = ctx.output_dir.join(format!("{}.yaml", manifest.id));
    if path.exists() && !args.overwrite {
        return Err(anyhow!(
            "workflow file exists (pass overwrite=true to replace): {}",
            path.display()
        ));
    }
    let yaml = serde_yaml::to_string(&manifest).map_err(|e| anyhow!("serialize: {}", e))?;
    std::fs::write(&path, yaml).map_err(|e| anyhow!("write {}: {}", path.display(), e))?;

    Ok(json!({
        "ok": true,
        "id": manifest.id,
        "path": path.to_string_lossy(),
        "node_count": manifest.nodes.len(),
        "edge_count": manifest.edges.len(),
    }))
}

// ---------------------------------------------------------------------------
// Tool: register_tool
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RegisterToolArgs {
    name: String,
    description: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    working_dir: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    input_schema: Option<Value>,
}

fn default_timeout() -> u64 {
    30_000
}

pub struct RegisterToolTool {
    tools_dir: PathBuf,
}

impl RegisterToolTool {
    pub fn new(tools_dir: PathBuf) -> Self {
        Self { tools_dir }
    }
}

#[async_trait]
impl Tool for RegisterToolTool {
    fn name(&self) -> &str {
        "register_tool"
    }
    fn description(&self) -> &str {
        "Register an external command as a GeWu tool capability. The command \
         must exist and be executable. The tool is persisted to \
         ~/.gewu/tools/<name>.yaml and will be available in subsequent \
         workflow runs. Returns { ok, name, path } on success."
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Unique tool name (e.g. compute_bazi_chart)" },
                "description": { "type": "string", "description": "What this tool does" },
                "command": { "type": "string", "description": "Absolute path to the executable" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "CLI arguments appended after command" },
                "working_dir": { "type": "string", "description": "Working directory for the process" },
                "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default 30000)" },
                "input_schema": { "type": "object", "description": "Optional JSON Schema for the tool's input" }
            },
            "required": ["name", "description", "command"]
        }))
        .unwrap()
    }
    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let a: RegisterToolArgs =
            serde_json::from_value(args).map_err(|e| format!("invalid args: {}", e))?;

        let def = CommandToolDef {
            name: a.name.clone(),
            description: a.description,
            command: a.command,
            args: a.args,
            working_dir: a.working_dir,
            env: Default::default(),
            timeout_ms: a.timeout_ms,
            input_schema: a.input_schema,
        };

        let mut registry = CommandToolRegistry::new(&self.tools_dir)
            .map_err(|e| format!("open tool registry: {}", e))?;

        match registry.register(def) {
            Ok(()) => {
                let path = self.tools_dir.join(format!("{}.yaml", a.name));
                let out = json!({
                    "ok": true,
                    "name": a.name,
                    "path": path.to_string_lossy(),
                });
                Ok(ToolResult::new(
                    "register_tool",
                    serde_json::to_string(&out).unwrap_or_default(),
                ))
            }
            Err(e) => {
                let out = json!({
                    "ok": false,
                    "error": e.to_string(),
                });
                Ok(ToolResult::new(
                    "register_tool",
                    serde_json::to_string(&out).unwrap_or_default(),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: write_specialist
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WriteSpecialistArgs {
    id: String,
    description: String,
    capabilities: Vec<String>,
    system_prompt: String,
    #[serde(default)]
    overwrite: bool,
}

pub struct WriteSpecialistTool {
    specialists_dir: PathBuf,
}

impl WriteSpecialistTool {
    pub fn new(specialists_dir: PathBuf) -> Self {
        Self { specialists_dir }
    }
}

#[async_trait]
impl Tool for WriteSpecialistTool {
    fn name(&self) -> &str {
        "write_specialist"
    }
    fn description(&self) -> &str {
        "Create and persist a new specialist definition. The specialist is saved \
         to ~/.gewu/specialists/<id>.yaml and will be available in subsequent \
         runtime builds. Use this to create focused specialists with specific \
         capabilities and system prompts."
    }
    fn schema(&self) -> RootSchema {
        serde_json::from_value(json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Unique specialist ID" },
                "description": { "type": "string", "description": "What this specialist does" },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of capability names this specialist can use"
                },
                "system_prompt": { "type": "string", "description": "System prompt for the specialist" },
                "overwrite": { "type": "boolean", "description": "Allow overwriting existing specialist (default false)" }
            },
            "required": ["id", "description", "capabilities", "system_prompt"]
        }))
        .unwrap()
    }
    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let a: WriteSpecialistArgs =
            serde_json::from_value(args).map_err(|e| format!("invalid args: {}", e))?;

        if a.id.is_empty() {
            return Ok(ToolResult::new(
                "write_specialist",
                serde_json::to_string(&json!({"ok": false, "error": "id cannot be empty"}))
                    .unwrap(),
            ));
        }

        std::fs::create_dir_all(&self.specialists_dir)
            .map_err(|e| format!("create specialists dir: {}", e))?;

        let path = self.specialists_dir.join(format!("{}.yaml", a.id));
        if path.exists() && !a.overwrite {
            return Ok(ToolResult::new("write_specialist",
                serde_json::to_string(&json!({
                    "ok": false,
                    "error": format!("specialist '{}' already exists (pass overwrite=true to replace)", a.id)
                })).unwrap()));
        }

        let manifest = json!({
            "id": a.id,
            "description": a.description,
            "prompt": { "inline": a.system_prompt },
            "capabilities": a.capabilities,
        });
        let yaml = serde_yaml::to_string(&manifest).map_err(|e| format!("serialize: {}", e))?;
        std::fs::write(&path, yaml).map_err(|e| format!("write {}: {}", path.display(), e))?;

        let out = json!({
            "ok": true,
            "id": a.id,
            "path": path.to_string_lossy(),
            "capabilities": a.capabilities,
        });
        Ok(ToolResult::new(
            "write_specialist",
            serde_json::to_string(&out).unwrap_or_default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// ToolFactory wrapping the composer tools as capabilities.
// ---------------------------------------------------------------------------

pub const COMPOSER_CAPABILITIES: &[&str] =
    &["list_specialists", "list_capabilities", "write_workflow"];

pub struct ComposerToolFactory {
    ctx: ComposerContext,
    tools_dir: PathBuf,
    specialists_dir: PathBuf,
}

impl ComposerToolFactory {
    pub fn new(ctx: ComposerContext) -> Self {
        let tools_dir = crate::command_tool_registry::CommandToolRegistry::default_dir();
        let specialists_dir = default_specialists_dir();
        Self {
            ctx,
            tools_dir,
            specialists_dir,
        }
    }

    pub fn with_tools_dir(mut self, dir: PathBuf) -> Self {
        self.tools_dir = dir;
        self
    }
}

pub fn default_specialists_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gewu")
        .join("specialists")
}

impl ToolFactory for ComposerToolFactory {
    fn capabilities(&self) -> Vec<String> {
        COMPOSER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
    fn create_tools(
        &self,
        capability: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        Ok(match capability {
            "list_specialists" => vec![Box::new(ListSpecialistsTool::new(self.ctx.clone()))],
            "list_capabilities" => vec![Box::new(ListCapabilitiesTool::new(self.ctx.clone()))],
            "write_workflow" => vec![Box::new(WriteWorkflowTool::new(self.ctx.clone()))],
            "register_tool" => vec![Box::new(RegisterToolTool::new(self.tools_dir.clone()))],
            "write_specialist" => vec![Box::new(WriteSpecialistTool::new(
                self.specialists_dir.clone(),
            ))],
            _ => vec![],
        })
    }
}

// ---------------------------------------------------------------------------
// Default composer specialist descriptor.
// ---------------------------------------------------------------------------

pub fn composer_specialist_descriptor() -> SpecialistDescriptor {
    SpecialistDescriptor {
        id: "composer".to_string(),
        description: "Self-assembly composer: inspects available specialists \
                      and capabilities, emits a WorkflowManifest that \
                      achieves a user-supplied goal, and persists it via \
                      write_workflow."
            .to_string(),
        capabilities: COMPOSER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        skills: Vec::new(),
        system_prompt: include_str!("composer_prompt.md").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::CapabilityDescriptor;

    fn mk_ctx(dir: PathBuf) -> ComposerContext {
        ComposerContext {
            specialists: Arc::new(vec![SpecialistDescriptor {
                id: "bazi".into(),
                description: "b".into(),
                capabilities: vec!["fw_bazi_task".into()],
                skills: Vec::new(),
                system_prompt: "".into(),
            }]),
            capabilities: Arc::new(vec![
                CapabilityDescriptor::new("fw_bazi_task", "plugin"),
                CapabilityDescriptor::new("fw_intent", "plugin"),
            ]),
            workflows: Arc::new(HashMap::new()),
            output_dir: dir,
        }
    }

    #[tokio::test]
    async fn list_specialists_returns_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mk_ctx(dir.path().to_path_buf());
        let tool = ListSpecialistsTool::new(ctx);
        let r = tool
            .execute(json!({}), &ToolContext::new("s", "m"))
            .await
            .unwrap();
        let v: Vec<Value> = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["id"], "bazi");
    }

    #[tokio::test]
    async fn list_capabilities_returns_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mk_ctx(dir.path().to_path_buf());
        let tool = ListCapabilitiesTool::new(ctx);
        let r = tool
            .execute(json!({}), &ToolContext::new("s", "m"))
            .await
            .unwrap();
        let v: Vec<Value> = serde_json::from_str(&r.output).unwrap();
        let names: Vec<&str> = v.iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"fw_bazi_task"));
        assert!(names.contains(&"fw_intent"));
    }

    #[tokio::test]
    async fn write_workflow_persists_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mk_ctx(dir.path().to_path_buf());
        let tool = WriteWorkflowTool::new(ctx);
        let yaml = r#"
id: good
entry: a
finish: [a]
nodes:
  a: { kind: tool, tool: fw_intent }
edges: []
"#;
        let r = tool
            .execute(
                json!({ "id": "good", "manifest_yaml": yaml }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(dir.path().join("good.yaml").exists());
    }

    #[tokio::test]
    async fn write_workflow_rejects_unknown_specialist() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mk_ctx(dir.path().to_path_buf());
        let tool = WriteWorkflowTool::new(ctx);
        let yaml = r#"
id: bad
entry: a
finish: [a]
nodes:
  a: { kind: specialist, specialist: nonexistent }
edges: []
"#;
        let r = tool
            .execute(
                json!({ "id": "bad", "manifest_yaml": yaml }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert!(
            v["error"].as_str().unwrap().contains("unknown specialist"),
            "error: {}",
            v["error"]
        );
        assert!(!dir.path().join("bad.yaml").exists());
    }

    #[tokio::test]
    async fn write_workflow_rejects_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mk_ctx(dir.path().to_path_buf());
        let tool = WriteWorkflowTool::new(ctx);
        let yaml = r#"
id: actual
entry: a
finish: [a]
nodes: { a: { kind: tool, tool: fw_intent } }
edges: []
"#;
        let r = tool
            .execute(
                json!({ "id": "claimed", "manifest_yaml": yaml }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("id mismatch"));
    }

    #[tokio::test]
    async fn write_workflow_refuses_overwrite_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mk_ctx(dir.path().to_path_buf());
        let tool = WriteWorkflowTool::new(ctx);
        let yaml = "id: w\nentry: a\nfinish: [a]\nnodes: { a: { kind: tool, tool: fw_intent } }\nedges: []\n";

        // First write: succeeds
        let r1 = tool
            .execute(
                json!({ "id": "w", "manifest_yaml": yaml }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        let v1: Value = serde_json::from_str(&r1.output).unwrap();
        assert_eq!(v1["ok"], json!(true));

        // Second write without overwrite: rejected
        let r2 = tool
            .execute(
                json!({ "id": "w", "manifest_yaml": yaml }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        let v2: Value = serde_json::from_str(&r2.output).unwrap();
        assert_eq!(v2["ok"], json!(false));
        assert!(v2["error"].as_str().unwrap().contains("exists"));

        // Third with overwrite=true: succeeds
        let r3 = tool
            .execute(
                json!({ "id": "w", "manifest_yaml": yaml, "overwrite": true }),
                &ToolContext::new("s", "m"),
            )
            .await
            .unwrap();
        let v3: Value = serde_json::from_str(&r3.output).unwrap();
        assert_eq!(v3["ok"], json!(true));
    }
}
