use gewu_canvas::invocation_poller::PendingInvocation;
use gewu_runtime::invocation_loop::InvocationExecutor;

#[test]
fn executor_converts_invocation_to_agent_prompt() {
    let invocation = PendingInvocation {
        invocation_id: "inv_001".to_string(),
        agent_id: "canvas-analyst".to_string(),
        briefing: "## Trigger\nUser: run QC on SM30\n## Selection\n- ucf_SM30_1".to_string(),
        selected_nodes: vec!["ucf_SM30_1".to_string()],
    };

    let executor = InvocationExecutor::new("http://localhost:3210", 100);
    let prompt = executor.build_prompt(&invocation);

    assert!(prompt.contains("run QC on SM30"));
    assert!(prompt.contains("ucf_SM30_1"));
    assert!(prompt.contains("## Trigger"));
    assert!(prompt.contains("workflow.emit"));
}
