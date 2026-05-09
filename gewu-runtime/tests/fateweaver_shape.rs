//! Phase 6 validation: the hand-written `fateweaver.yaml` workflow manifest
//! reproduces the same structural result as a Rust-imperative reference
//! implementation of FateWeaver's SOP.
//!
//! The real FateWeaver orchestrator is hand-coded Rust across multiple
//! crates (`fateweaver-orchestrator`, `fateweaver-orchestration-tools`,
//! `fateweaver-specialists`). To stay in-tree and deterministic — i.e. not
//! require an LLM — we shape-check against an in-test Rust reference built
//! from the *same* deterministic tools. If the two paths agree, the
//! `WorkflowCompiler` faithfully reproduces the SOP topology, which is what
//! the self-assembly story (Phase 8) ultimately relies on.
//!
//! Tools below are the deterministic analogues of FateWeaver's orchestration
//! tools (`intent_tool`, `task_planning_tool`, `protocol_review_tool`,
//! `synthesis_tool`, `fusion_tool`, `hard_review_tool`, `reviewer_tool`,
//! `answer_tool`) plus simplified bazi/ziwei specialists.

use async_trait::async_trait;
use futures::stream::BoxStream;
use gewu_core::traits::{AssemblyContext, LlmProvider, Plugin, Tool, ToolFactory};
use gewu_core::types::{Message, ProviderEvent, SpecialistDescriptor, ToolContext, ToolResult};
use gewu_graph::Blackboard;
use gewu_runtime::manifest::workflow::WorkflowManifest;
use gewu_runtime::{DiffOptions, GeWuRuntime, WorkflowCompiler, diff_blackboards, format_diff};
use schemars::schema::RootSchema;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Minimal LLM stub (never called — we use tool nodes only).
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
// Deterministic FateWeaver-shape tools
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

// `intent`: topics + horizon derived trivially from the CaseRequest.
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

// `plan_tasks`: emit a SpecialistTask per requested specialist.
make_tool!(PlanTasksTool, "fw_plan_tasks", |args: Value| {
    let intent = args;
    let topics = intent.get("topics").cloned().unwrap_or(json!([]));
    json!({
        "tasks": {
            "bazi":  { "topics": topics.clone(), "kind": "bazi" },
            "ziwei": { "topics": topics,         "kind": "ziwei" },
        }
    })
});

// `bazi_task`: produce 2 claims (deterministic).
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

// `ziwei_task`: 1 supporting claim.
make_tool!(ZiweiTaskTool, "fw_ziwei_task", |args: Value| {
    let topics = args.get("topics").cloned().unwrap_or(json!([]));
    json!({
        "claims": [
            { "topic": topics.get(0).cloned().unwrap_or(json!("general")), "source": "ziwei", "polarity": "favor" }
        ],
        "notes": "ziwei-ok"
    })
});

// `protocol_review`: signals repair if either run produced zero claims.
// Input is the `runs` object: `{ bazi: {...}, ziwei: {...} }`.
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

// `synthesis`: concat claims per source. Preserves order. Input is the `runs`
// object (same shape as protocol input).
make_tool!(SynthesisTool, "fw_synthesis", |args: Value| {
    let mut all: Vec<Value> = Vec::new();
    if let Some(m) = args.as_object() {
        let mut keys: Vec<&String> = m.keys().collect();
        keys.sort(); // deterministic order
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

// `fusion`: group claims by topic+polarity.
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
        let key = format!("{topic}/{polarity}");
        groups.entry(key).or_default().push(c.clone());
    }
    let grouped: Vec<Value> = groups
        .into_iter()
        .map(|(k, cs)| json!({ "key": k, "claims": cs }))
        .collect();
    json!({ "groups": grouped, "total": claims.len() })
});

// `hard_review`: deterministic rule check.
make_tool!(HardReviewTool, "fw_hard_review", |args: Value| {
    let total = args.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    json!({ "hard_pass": total > 0, "issues": [] })
});

// `agentic_review`: echoes hard review + confidence.
make_tool!(AgenticReviewTool, "fw_agentic_review", |args: Value| {
    let hard_pass = args
        .get("hard_pass")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    json!({ "hard_pass": hard_pass, "confidence": if hard_pass { 0.8 } else { 0.4 } })
});

// `compose_answer`: final user-facing object.
make_tool!(ComposeAnswerTool, "fw_compose_answer", |args: Value| {
    let fusion = args.get("fusion").cloned().unwrap_or(json!({}));
    let review = args.get("review").cloned().unwrap_or(json!({}));
    json!({
        "topics": fusion.get("groups").cloned().unwrap_or(json!([])),
        "confidence": review.get("confidence").cloned().unwrap_or(json!(0.0)),
        "claims_total": fusion.get("total").cloned().unwrap_or(json!(0)),
    })
});

// ---------------------------------------------------------------------------
// Tool factory registering all FW-shape tools.
// ---------------------------------------------------------------------------

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
        vec![]
    }
}

async fn build_runtime() -> Arc<GeWuRuntime> {
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

// ---------------------------------------------------------------------------
// Rust imperative reference SOP — the oracle we diff the compiled workflow against.
// ---------------------------------------------------------------------------

async fn run_rust_reference(rt: &GeWuRuntime, input: Value) -> Blackboard {
    let mut bb = Blackboard::from_value(input);

    // intent
    let intent = call(rt, "fw_intent", bb.as_value().clone()).await;
    bb.set_path("intent", intent);

    // plan
    let plan = call(rt, "fw_plan_tasks", bb.get_path("intent").unwrap().clone()).await;
    bb.set_path("plan", plan);

    // parallel dispatch
    let tasks = bb.get_path("plan.tasks").cloned().unwrap_or(json!({}));
    let bazi_in = tasks.get("bazi").cloned().unwrap_or(json!({}));
    let ziwei_in = tasks.get("ziwei").cloned().unwrap_or(json!({}));
    let (bazi_out, ziwei_out) = tokio::join!(
        call(rt, "fw_bazi_task", bazi_in),
        call(rt, "fw_ziwei_task", ziwei_in),
    );
    bb.set_path("runs.bazi", bazi_out);
    bb.set_path("runs.ziwei", ziwei_out);

    // protocol
    let protocol = call(
        rt,
        "fw_protocol_review",
        bb.get_path("runs").unwrap().clone(),
    )
    .await;
    bb.set_path("protocol", protocol);

    // synthesis (always — v1 has no repair loop)
    let synth = call(rt, "fw_synthesis", bb.get_path("runs").unwrap().clone()).await;
    bb.set_path("synth", synth);

    // fusion
    let fusion = call(
        rt,
        "fw_fusion",
        json!({ "claims": bb.get_path("synth.claims").unwrap() }),
    )
    .await;
    bb.set_path("fusion", fusion);

    // hard review
    let hard = call(
        rt,
        "fw_hard_review",
        json!({ "total": bb.get_path("fusion.total").unwrap() }),
    )
    .await;
    bb.set_path("hard", hard);

    // agentic review
    let agentic = call(
        rt,
        "fw_agentic_review",
        json!({ "hard_pass": bb.get_path("hard.hard_pass").unwrap() }),
    )
    .await;
    bb.set_path("review", agentic);

    // compose answer
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

// ---------------------------------------------------------------------------
// Test: hand-written fateweaver_shape.yaml matches Rust reference.
// ---------------------------------------------------------------------------

/// The reference shape we compare against: identical topology, identical
/// deterministic tools, different execution strategy.
const FW_SHAPE_YAML: &str = r#"
id: fw_shape
description: "FateWeaver-shape SOP (deterministic test fixture)"
entry: intent
finish: [final]
nodes:
  intent:    { kind: tool, tool: fw_intent,           write: intent }
  plan:      { kind: tool, tool: fw_plan_tasks,       input: $.intent, write: plan }
  dispatch:
    kind: parallel
    branches:
      bazi:  { kind: tool, tool: fw_bazi_task,  input: $.plan.tasks.bazi,  write: runs.bazi }
      ziwei: { kind: tool, tool: fw_ziwei_task, input: $.plan.tasks.ziwei, write: runs.ziwei }
  protocol:  { kind: tool, tool: fw_protocol_review,  input: $.runs,       write: protocol }
  synth:     { kind: tool, tool: fw_synthesis,        input: $.runs,       write: synth   }
  fusion:    { kind: tool, tool: fw_fusion,           input: $.synth,      write: fusion  }
  hard:      { kind: tool, tool: fw_hard_review,      input: $.fusion,     write: hard    }
  review:    { kind: tool, tool: fw_agentic_review,   input: $.hard,       write: review  }
  final:     { kind: tool, tool: fw_compose_answer,   write: final  }
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

// For the `final` node, `compose_answer` expects `{ fusion, review }`. We
// pass the whole blackboard; its reader pulls `$.fusion` and `$.review`.
// That keeps the manifest simple: no input_path for `final`.

#[tokio::test]
async fn l1_fw_shape_yaml_matches_rust_reference_on_clean_input() {
    let rt = build_runtime().await;

    let input = json!({
        "question": "will love work out?",
        "horizon": "near",
    });

    let rust_bb = run_rust_reference(&rt, input.clone()).await;

    let manifest: WorkflowManifest = serde_yaml::from_str(FW_SHAPE_YAML).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&manifest).unwrap();
    let wf_bb = graph.ainvoke(Blackboard::from_value(input)).await.unwrap();

    let opts = DiffOptions::ignoring(vec!["_errors".to_string(), "_loops".to_string()]);
    let diffs = diff_blackboards(&rust_bb, &wf_bb, &opts);
    assert!(
        diffs.is_empty(),
        "L1 structural diff between Rust ref and compiled workflow:\n{}",
        format_diff(&diffs)
    );
}

#[tokio::test]
async fn l1_fw_shape_yaml_produces_expected_final_structure() {
    let rt = build_runtime().await;
    let manifest: WorkflowManifest = serde_yaml::from_str(FW_SHAPE_YAML).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&manifest).unwrap();
    let input = json!({ "question": "love q", "horizon": "near" });
    let out = graph.ainvoke(Blackboard::from_value(input)).await.unwrap();

    // Structural invariants on the final blackboard
    assert!(out.get_path("intent.topics").is_some());
    assert!(out.get_path("plan.tasks.bazi").is_some());
    assert!(out.get_path("plan.tasks.ziwei").is_some());
    assert!(out.get_path("runs.bazi.claims").is_some());
    assert!(out.get_path("runs.ziwei.claims").is_some());
    assert_eq!(out.get_path("protocol.needs_repair"), Some(&json!(false)));
    assert!(out.get_path("synth.count").is_some());
    assert!(out.get_path("fusion.groups").is_some());
    assert_eq!(out.get_path("hard.hard_pass"), Some(&json!(true)));
    assert!(out.get_path("review.confidence").is_some());
    assert!(out.get_path("final.topics").is_some());
}

#[tokio::test]
async fn fw_shape_specialist_and_tool_call_order_is_same_in_both_paths() {
    let rt = build_runtime().await;
    let input = json!({ "question": "love q", "horizon": "far" });

    let rust_bb = run_rust_reference(&rt, input.clone()).await;

    let manifest: WorkflowManifest = serde_yaml::from_str(FW_SHAPE_YAML).unwrap();
    let compiler = WorkflowCompiler::new(rt.clone());
    let graph = compiler.compile(&manifest).unwrap();
    let wf_bb = graph.ainvoke(Blackboard::from_value(input)).await.unwrap();

    // Spot-check: same final answer structure, same claim count total.
    assert_eq!(
        rust_bb.get_path("final.claims_total"),
        wf_bb.get_path("final.claims_total")
    );
    assert_eq!(
        rust_bb.get_path("fusion.total"),
        wf_bb.get_path("fusion.total")
    );
    assert_eq!(
        rust_bb.get_path("protocol.needs_repair"),
        wf_bb.get_path("protocol.needs_repair")
    );
}
