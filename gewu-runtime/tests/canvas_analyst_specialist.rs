use gewu_runtime::registry::specialist_descriptors;

#[test]
fn canvas_analyst_specialist_exists() {
    let descriptors = specialist_descriptors();
    let analyst = descriptors.iter().find(|d| d.id == "canvas-analyst");
    assert!(analyst.is_some(), "canvas-analyst specialist not found");

    let analyst = analyst.unwrap();
    assert!(analyst.capabilities.contains(&"workflow_emit".to_string()));
    assert!(analyst.capabilities.contains(&"canvas_list".to_string()));
    assert!(analyst.capabilities.contains(&"bash".to_string()));
    assert!(analyst.system_prompt.contains("bioinformatics"));
    assert!(analyst.system_prompt.contains("workflow.emit"));
}
