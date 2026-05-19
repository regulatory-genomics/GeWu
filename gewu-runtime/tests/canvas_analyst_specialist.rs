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

#[test]
fn canvas_analyst_prompt_prefers_code_execute_when_requested() {
    let descriptors = specialist_descriptors();
    let analyst = descriptors
        .iter()
        .find(|d| d.id == "canvas-analyst")
        .expect("canvas-analyst specialist exists");

    assert!(analyst.system_prompt.contains("code_execute"));
    assert!(analyst
        .system_prompt
        .contains("When the user explicitly asks to use code_execute"));
}

#[test]
fn canvas_analyst_prompt_treats_code_execute_as_workflow_recording() {
    let descriptors = specialist_descriptors();
    let analyst = descriptors
        .iter()
        .find(|d| d.id == "canvas-analyst")
        .expect("canvas-analyst specialist exists");

    assert!(analyst.system_prompt.contains("code_execute counts as workflow recording"));
    assert!(
        !analyst.system_prompt.contains("Never skip workflow.emit"),
        "prompt must not contradict explicit code_execute requests"
    );
}
