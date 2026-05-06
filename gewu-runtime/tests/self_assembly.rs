//! Phase 8 — **Self-assembly validation**.
//!
//! The critical go/no-go for this project: the composer's tools are
//! sufficient for a (human or LLM) agent to emit a `WorkflowManifest`
//! equivalent to FateWeaver's SOP, persist it, and re-run it later with
//! the same behavior as the Rust-imperative reference.
//!
//! This test drives the composer tools directly — no LLM — to prove the
//! MECHANISM is complete. Driving the same tools with a live LLM validates
//! the POLICY; that's covered elsewhere (by the `composer` specialist
//! running against real API credentials).
//!
//! The sequence exercised here is:
//!
//!   1. Composer calls `list_specialists` / `list_capabilities` to discover
//!      what's available in the runtime.
//!   2. Composer drafts a YAML workflow manifest that references only those
//!      ids, with the FateWeaver SOP topology.
//!   3. Composer calls `write_workflow`; it validates and persists.
//!   4. The persisted file is loaded back via `WorkflowManifestLoader` and
//!      compiled via `WorkflowCompiler`.
//!   5. Invoking the compiled graph produces a blackboard structurally
//!      identical (modulo `_errors` / `_loops`) to the Rust imperative
//!      reference implementation of the same SOP.
//!
//! Additional assertions cover L3c (anti-hallucination): when a required
//! specialist is removed from the snapshot the composer's validator refuses
//! a manifest that references it.

use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_core::traits::{AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory};
use gewu_core::types::{
    CapabilityDescriptor, Message, ProviderEvent, SpecialistDescriptor, ToolContext, ToolResult,
};
use gewu_graph::Blackboard;
use gewu_runtime::composer::WriteWorkflowTool;
use gewu_runtime::manifest::workflow::WorkflowManifest;
use gewu_runtime::manifest::workflow_loader::WorkflowManifestLoader;
use gewu_runtime::{
    ComposerContext, DiffOptions, GeWuRuntime, WorkflowCompiler, diff_blackboards, format_diff,
};
use schemars::schema::RootSchema;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Minimal LLM stub.
// ---------------------------------------------------------------------------

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
        unimplemented!("stub llm")
    }
    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// FateWeaver-shape tools (same behavior as in fateweaver_shape.rs but
// redeclared here to keep this test self-contained — integration test crates
// don't share private modules).
// ---------------------------------------------------------------------------

macro_rules! make_tool {
    ($name:ident, $id:literal, $body:expr) => {
        struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $id
            }
            fn description(&self) -> &str {
                concat!("fw-shape test tool: ", $id)
            }
            fn schema(&self) -> RootSchema {
                RootSchema::default()
            }
            async fn execute(
                &self,
                args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
                let f = $body;
                let out: Value = f(args);
                Ok(ToolResult::new(
                    $id,
                    serde_json::to_string(&out).unwrap_or_default(),
                ))
            }
        }
    };
}

make_tool!(IntentTool, "fw_intent", |args: Value| {
    let q = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
    let topic = if q.contains("love") {
        "love"
    } else {
        "general"
    };
    json!({
        "topics": [topic],
        "horizon": args.get("horizon").cloned().unwrap_or(json!("near")),
        "specialists": ["bazi", "ziwei"],
    })
});
make_tool!(PlanTasksTool, "fw_plan_tasks", |args: Value| {
    let topics = args.get("topics").cloned().unwrap_or(json!([]));
    json!({
        "tasks": {
            "bazi":  { "topics": topics.clone(), "kind": "bazi" },
            "ziwei": { "topics": topics,         "kind": "ziwei" },
        }
    })
});
make_tool!(BaziTaskTool, "fw_bazi_task", |args: Value| {
    let topics = args.get("topics").cloned().unwrap_or(json!([]));
    json!({
        "claims": [
            { "topic": topics.get(0).cloned().unwrap_or(json!("general")), "source": "bazi", "polarity": "favor" },
            { "topic": topics.get(0).cloned().unwrap_or(json!("general")), "source": "bazi", "polarity": "caution" }
        ],
        "notes": "bazi-ok"
    })
});
make_tool!(ZiweiTaskTool, "fw_ziwei_task", |args: Value| {
    let topics = args.get("topics").cloned().unwrap_or(json!([]));
    json!({
        "claims": [
            { "topic": topics.get(0).cloned().unwrap_or(json!("general")), "source": "ziwei", "polarity": "favor" }
        ],
        "notes": "ziwei-ok"
    })
});
make_tool!(ProtocolReviewTool, "fw_protocol_review", |args: Value| {
    let empty = args
        .as_object()
        .map(|m| {
            m.values().any(|v| {
                v.get("claims")
                    .and_then(|c| c.as_array())
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
            })
        })
        .unwrap_or(true);
    json!({ "needs_repair": empty, "coverage": if empty { "partial" } else { "complete" } })
});
make_tool!(SynthesisTool, "fw_synthesis", |args: Value| {
    let mut all: Vec<Value> = Vec::new();
    if let Some(m) = args.as_object() {
        let mut keys: Vec<&String> = m.keys().collect();
        keys.sort();
        for k in keys {
            if let Some(v) = m.get(k)
                && let Some(cs) = v.get("claims").and_then(|c| c.as_array())
            {
                all.extend(cs.iter().cloned());
            }
        }
    }
    json!({ "claims": all.clone(), "count": all.len() })
});
make_tool!(FusionTool, "fw_fusion", |args: Value| {
    let claims = args
        .get("claims")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let mut groups: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for c in &claims {
        let topic = c.get("topic").and_then(|t| t.as_str()).unwrap_or("general");
        let polarity = c
            .get("polarity")
            .and_then(|t| t.as_str())
            .unwrap_or("neutral");
        let key = format!("{}/{}", topic, polarity);
        groups.entry(key).or_default().push(c.clone());
    }
    let grouped: Vec<Value> = groups
        .into_iter()
        .map(|(k, cs)| json!({ "key": k, "claims": cs }))
        .collect();
    json!({ "groups": grouped, "total": claims.len() })
});
make_tool!(HardReviewTool, "fw_hard_review", |args: Value| {
    let total = args.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    json!({ "hard_pass": total > 0, "issues": [] })
});
make_tool!(AgenticReviewTool, "fw_agentic_review", |args: Value| {
    let hard_pass = args
        .get("hard_pass")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    json!({ "hard_pass": hard_pass, "confidence": if hard_pass { 0.8 } else { 0.4 } })
});
make_tool!(ComposeAnswerTool, "fw_compose_answer", |args: Value| {
    let fusion = args.get("fusion").cloned().unwrap_or(json!({}));
    let review = args.get("review").cloned().unwrap_or(json!({}));
    json!({
        "topics": fusion.get("groups").cloned().unwrap_or(json!([])),
        "confidence": review.get("confidence").cloned().unwrap_or(json!(0.0)),
        "claims_total": fusion.get("total").cloned().unwrap_or(json!(0)),
    })
});

struct FwShapeFactory;

impl ToolFactory for FwShapeFactory {
    fn capabilities(&self) -> Vec<String> {
        vec![
            "fw_intent".into(),
            "fw_plan_tasks".into(),
            "fw_bazi_task".into(),
            "fw_ziwei_task".into(),
            "fw_protocol_review".into(),
            "fw_synthesis".into(),
            "fw_fusion".into(),
            "fw_hard_review".into(),
            "fw_agentic_review".into(),
            "fw_compose_answer".into(),
        ]
    }
    fn create_tools(
        &self,
        capability: &str,
        _ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>, Box<dyn Error + Send + Sync>> {
        Ok(match capability {
            "fw_intent" => vec![Box::new(IntentTool)],
            "fw_plan_tasks" => vec![Box::new(PlanTasksTool)],
            "fw_bazi_task" => vec![Box::new(BaziTaskTool)],
            "fw_ziwei_task" => vec![Box::new(ZiweiTaskTool)],
            "fw_protocol_review" => vec![Box::new(ProtocolReviewTool)],
            "fw_synthesis" => vec![Box::new(SynthesisTool)],
            "fw_fusion" => vec![Box::new(FusionTool)],
            "fw_hard_review" => vec![Box::new(HardReviewTool)],
            "fw_agentic_review" => vec![Box::new(AgenticReviewTool)],
            "fw_compose_answer" => vec![Box::new(ComposeAnswerTool)],
            _ => vec![],
        })
    }
}

struct FwShapePlugin;

impl Plugin for FwShapePlugin {
    fn name(&self) -> &str {
        "fw-shape"
    }
    fn tool_factories(&self) -> Vec<Box<dyn ToolFactory>> {
        vec![Box::new(FwShapeFactory)]
    }
    fn specialist_descriptors(&self) -> Vec<SpecialistDescriptor> {
        // The four FateWeaver-equivalent specialists.
        vec![
            SpecialistDescriptor {
                id: "fateweaver_bazi".into(),
                description: "bazi specialist".into(),
                capabilities: vec!["fw_bazi_task".into()],
                skills: Vec::new(),
                system_prompt: "".into(),
            },
            SpecialistDescriptor {
                id: "fateweaver_ziwei".into(),
                description: "ziwei specialist".into(),
                capabilities: vec!["fw_ziwei_task".into()],
                skills: Vec::new(),
                system_prompt: "".into(),
            },
            SpecialistDescriptor {
                id: "fateweaver_orchestrator".into(),
                description: "orchestration coordinator".into(),
                capabilities: vec![
                    "fw_intent".into(),
                    "fw_plan_tasks".into(),
                    "fw_protocol_review".into(),
                    "fw_synthesis".into(),
                    "fw_fusion".into(),
                    "fw_hard_review".into(),
                    "fw_compose_answer".into(),
                ],
                skills: Vec::new(),
                system_prompt: "".into(),
            },
            SpecialistDescriptor {
                id: "fateweaver_reviewer".into(),
                description: "claim reviewer".into(),
                capabilities: vec!["fw_agentic_review".into()],
                skills: Vec::new(),
                system_prompt: "".into(),
            },
        ]
    }
}

async fn build_runtime_with_composer() -> Arc<GeWuRuntime> {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    gewu_storage::run_migrations(&pool).await.unwrap();
    let provider: Arc<dyn LlmProvider> = Arc::new(StubLlm);
    let rt = GeWuRuntime::builder()
        .provider(provider)
        .pool(pool)
        .register_plugin(Box::new(FwShapePlugin))
        .build()
        .unwrap();
    Arc::new(rt)
}

fn snapshot_context(rt: &GeWuRuntime, out_dir: std::path::PathBuf) -> ComposerContext {
    let specialists: Vec<SpecialistDescriptor> = rt
        .list_specialists()
        .into_iter()
        .map(|a| SpecialistDescriptor {
            id: a.name,
            description: a.description,
            capabilities: vec![],
            skills: Vec::new(),
            system_prompt: String::new(),
        })
        .collect();
    let capabilities: Vec<CapabilityDescriptor> = rt.capability_registry().list_capabilities();
    ComposerContext {
        specialists: Arc::new(specialists),
        capabilities: Arc::new(capabilities),
        workflows: Arc::new(HashMap::new()),
        output_dir: out_dir,
    }
}

async fn run_rust_reference(rt: &GeWuRuntime, input: Value) -> Blackboard {
    let mut bb = Blackboard::from_value(input);
    let intent = call(rt, "fw_intent", bb.as_value().clone()).await;
    bb.set_path("intent", intent);
    let plan = call(rt, "fw_plan_tasks", bb.get_path("intent").unwrap().clone()).await;
    bb.set_path("plan", plan);
    let tasks = bb.get_path("plan.tasks").cloned().unwrap_or(json!({}));
    let (bazi_out, ziwei_out) = tokio::join!(
        call(
            rt,
            "fw_bazi_task",
            tasks.get("bazi").cloned().unwrap_or(json!({}))
        ),
        call(
            rt,
            "fw_ziwei_task",
            tasks.get("ziwei").cloned().unwrap_or(json!({}))
        ),
    );
    bb.set_path("runs.bazi", bazi_out);
    bb.set_path("runs.ziwei", ziwei_out);
    let protocol = call(
        rt,
        "fw_protocol_review",
        bb.get_path("runs").unwrap().clone(),
    )
    .await;
    bb.set_path("protocol", protocol);
    let synth = call(rt, "fw_synthesis", bb.get_path("runs").unwrap().clone()).await;
    bb.set_path("synth", synth);
    let fusion = call(
        rt,
        "fw_fusion",
        json!({ "claims": bb.get_path("synth.claims").unwrap() }),
    )
    .await;
    bb.set_path("fusion", fusion);
    let hard = call(
        rt,
        "fw_hard_review",
        json!({ "total": bb.get_path("fusion.total").unwrap() }),
    )
    .await;
    bb.set_path("hard", hard);
    let agentic = call(
        rt,
        "fw_agentic_review",
        json!({ "hard_pass": bb.get_path("hard.hard_pass").unwrap() }),
    )
    .await;
    bb.set_path("review", agentic);
    let answer = call(
        rt, "fw_compose_answer",
        json!({ "fusion": bb.get_path("fusion").unwrap(), "review": bb.get_path("review").unwrap() })
    ).await;
    bb.set_path("final", answer);
    bb
}

async fn call(rt: &GeWuRuntime, cap: &str, args: Value) -> Value {
    let mut tools = rt.resolve_capabilities(&[cap.to_string()]).unwrap();
    let tool = tools.pop().expect("tool");
    let ctx = ToolContext::new("sess", "mid");
    let tr = tool.execute(args, &ctx).await.unwrap();
    serde_json::from_str(&tr.output).unwrap_or(Value::Null)
}

/// This is the YAML the composer agent emits when driven by a real LLM.
/// In this test we construct it programmatically to prove the tools + schema
/// suffice. The composer specialist prompt (composer_prompt.md) teaches an
/// LLM to write exactly this shape when given the same snapshot + goal.
const EXPECTED_COMPOSED_YAML: &str = r#"
id: fateweaver
description: "Auto-composed FateWeaver-shape SOP"
entry: intent
finish: [final]
nodes:
  intent:    { kind: tool, tool: fw_intent,          write: intent }
  plan:      { kind: tool, tool: fw_plan_tasks,      input: $.intent, write: plan }
  dispatch:
    kind: parallel
    branches:
      bazi:  { kind: tool, tool: fw_bazi_task,  input: $.plan.tasks.bazi,  write: runs.bazi }
      ziwei: { kind: tool, tool: fw_ziwei_task, input: $.plan.tasks.ziwei, write: runs.ziwei }
  protocol:  { kind: tool, tool: fw_protocol_review, input: $.runs,  write: protocol }
  synth:     { kind: tool, tool: fw_synthesis,       input: $.runs,  write: synth    }
  fusion:    { kind: tool, tool: fw_fusion,          input: $.synth, write: fusion   }
  hard:      { kind: tool, tool: fw_hard_review,     input: $.fusion, write: hard    }
  review:    { kind: tool, tool: fw_agentic_review,  input: $.hard,   write: review  }
  final:     { kind: tool, tool: fw_compose_answer,  write: final }
edges:
  - [intent, plan]
  - [plan, dispatch]
  - [dispatch, protocol]
  - [protocol, synth]
  - [synth, fusion]
  - [fusion, hard]
  - [hard, review]
  - [review, final]
"#;

// ---------------------------------------------------------------------------
// L3b — the critical go/no-go
// ---------------------------------------------------------------------------

#[tokio::test]
async fn self_assembly_produces_sop_matching_rust_reference() {
    let rt = build_runtime_with_composer().await;

    // Compute the Rust-imperative reference.
    let input = json!({ "question": "will love work out?", "horizon": "near" });
    let rust_bb = run_rust_reference(&rt, input.clone()).await;

    // Stage 1: composer snapshot context.
    let out_dir = tempfile::tempdir().unwrap();
    let ctx = snapshot_context(&rt, out_dir.path().to_path_buf());

    // Stage 2: the composer (here, the test harness acting as a would-be LLM)
    // calls write_workflow with the YAML that achieves the goal.
    let write_tool = WriteWorkflowTool::new(ctx.clone());
    let write_args = json!({
        "id": "fateweaver",
        "manifest_yaml": EXPECTED_COMPOSED_YAML,
    });
    let r = write_tool
        .execute(write_args, &ToolContext::new("composer", "m1"))
        .await
        .unwrap();
    let write_out: Value = serde_json::from_str(&r.output).unwrap();
    assert_eq!(
        write_out["ok"],
        json!(true),
        "write_workflow rejected composed manifest: {}",
        write_out["error"]
    );

    // Stage 3: load the persisted file from disk and invoke it.
    let mut loader = WorkflowManifestLoader::new();
    loader.add_dir(out_dir.path());
    let loaded: Vec<(_, WorkflowManifest)> = loader.load().unwrap();
    assert_eq!(loaded.len(), 1);
    let composed = loaded.into_iter().next().unwrap().1;
    assert_eq!(composed.id, "fateweaver");

    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&composed).unwrap();
    let composed_bb = graph
        .ainvoke(Blackboard::from_value(input.clone()))
        .await
        .unwrap();

    // Stage 4: the critical assertion — composer-produced workflow equals
    // Rust reference, modulo ephemeral keys.
    let opts = DiffOptions::ignoring(vec!["_errors".to_string(), "_loops".to_string()]);
    let diffs = diff_blackboards(&rust_bb, &composed_bb, &opts);
    assert!(
        diffs.is_empty(),
        "SELF-ASSEMBLY MISMATCH — composer-produced workflow diverges from Rust reference:\n{}",
        format_diff(&diffs)
    );
}

// ---------------------------------------------------------------------------
// L3c — anti-hallucination guardrail.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn composer_refuses_manifest_that_names_absent_specialist() {
    let rt = build_runtime_with_composer().await;
    let out_dir = tempfile::tempdir().unwrap();

    // Simulate a specialist being missing from the snapshot (e.g. the
    // runtime doesn't have `fateweaver_reviewer` loaded). The composer must
    // not fabricate a workflow referencing it.
    let mut ctx = snapshot_context(&rt, out_dir.path().to_path_buf());
    let filtered: Vec<SpecialistDescriptor> = ctx
        .specialists
        .iter()
        .filter(|s| s.id != "fateweaver_reviewer")
        .cloned()
        .collect();
    ctx.specialists = Arc::new(filtered);

    let yaml = r#"
id: bad
entry: a
finish: [a]
nodes:
  a: { kind: specialist, specialist: fateweaver_reviewer }
edges: []
"#;
    let tool = WriteWorkflowTool::new(ctx);
    let r = tool
        .execute(
            json!({ "id": "bad", "manifest_yaml": yaml }),
            &ToolContext::new("composer", "m"),
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
    assert!(!out_dir.path().join("bad.yaml").exists());
}

#[tokio::test]
async fn composer_context_exposes_all_specialists_and_capabilities() {
    let rt = build_runtime_with_composer().await;
    let out_dir = tempfile::tempdir().unwrap();
    let ctx = snapshot_context(&rt, out_dir.path().to_path_buf());

    let spec_ids: Vec<&str> = ctx.specialists.iter().map(|s| s.id.as_str()).collect();
    for required in &[
        "fateweaver_bazi",
        "fateweaver_ziwei",
        "fateweaver_orchestrator",
        "fateweaver_reviewer",
    ] {
        assert!(
            spec_ids.contains(required),
            "specialist {} missing from composer snapshot: {:?}",
            required,
            spec_ids
        );
    }

    let cap_names: Vec<&str> = ctx.capabilities.iter().map(|c| c.name.as_str()).collect();
    for required in &[
        "fw_intent",
        "fw_plan_tasks",
        "fw_bazi_task",
        "fw_ziwei_task",
        "fw_protocol_review",
        "fw_synthesis",
        "fw_fusion",
        "fw_hard_review",
        "fw_agentic_review",
        "fw_compose_answer",
    ] {
        assert!(
            cap_names.contains(required),
            "capability {} missing from composer snapshot: {:?}",
            required,
            cap_names
        );
    }
}

#[tokio::test]
async fn reusing_composed_workflow_via_workflow_registry_path() {
    let rt = build_runtime_with_composer().await;
    let input = json!({ "question": "love q", "horizon": "far" });

    // First run: compose + persist.
    let out_dir = tempfile::tempdir().unwrap();
    let ctx = snapshot_context(&rt, out_dir.path().to_path_buf());
    let write_tool = WriteWorkflowTool::new(ctx);
    let r = write_tool
        .execute(
            json!({ "id": "fateweaver", "manifest_yaml": EXPECTED_COMPOSED_YAML }),
            &ToolContext::new("composer", "m"),
        )
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&r.output).unwrap();
    assert_eq!(v["ok"], json!(true));

    // Second run: simulate a fresh session — load from registry by id and run.
    let mut loader = WorkflowManifestLoader::new();
    loader.add_dir(out_dir.path());
    let manifests: HashMap<String, WorkflowManifest> = loader
        .load()
        .unwrap()
        .into_iter()
        .map(|(_, m)| (m.id.clone(), m))
        .collect();
    let compiler = WorkflowCompiler::new(rt.clone()).with_workflows(manifests.clone());
    let graph = compiler
        .compile(manifests.get("fateweaver").unwrap())
        .unwrap();
    let out = graph
        .ainvoke(Blackboard::from_value(input.clone()))
        .await
        .unwrap();

    // Structural sanity: final answer exists with expected fields.
    assert_eq!(out.get_path("final.claims_total"), Some(&json!(3)));
    assert_eq!(out.get_path("protocol.needs_repair"), Some(&json!(false)));
}
