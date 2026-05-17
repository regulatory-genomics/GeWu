#[test]
fn canvas_analyst_prompt_contains_audited_sm30_pu1_protocol() {
    let descriptors = gewu_runtime::registry::specialist_descriptors();
    let analyst = descriptors
        .iter()
        .find(|descriptor| descriptor.id == "canvas-analyst")
        .expect("canvas-analyst specialist exists");

    assert!(analyst.system_prompt.contains("exactly 19 workflow.emit"));
    assert!(analyst.system_prompt.contains("帮我对 SM30 组的 PU.1 样本做 QC"));
    assert!(analyst.system_prompt.contains("qc_table_SM30"));
    assert!(analyst.system_prompt.contains("repro_SM30"));
    assert!(analyst.system_prompt.contains("Spearman r=0.576"));
}
