use gewu_cli::{find_descriptor, list_available_agents, specialist_descriptors};
use std::collections::HashSet;

#[test]
fn list_available_agents_contains_builtins() {
    let agents = list_available_agents();
    let names: HashSet<_> = agents.iter().map(|a| a.name.as_str()).collect();
    assert!(names.contains("build"), "missing 'build': {names:?}");
    assert!(names.contains("explore"), "missing 'explore': {names:?}");
    assert!(names.contains("general"), "missing 'general': {names:?}");
}

#[test]
fn specialist_descriptors_have_capabilities_and_prompts() {
    let descriptors = specialist_descriptors();
    assert!(!descriptors.is_empty());
    for d in &descriptors {
        assert!(!d.id.is_empty(), "empty id");
        assert!(!d.description.is_empty(), "empty description for {}", d.id);
        assert!(!d.system_prompt.is_empty(), "empty prompt for {}", d.id);
        assert!(!d.capabilities.is_empty(), "no capabilities for {}", d.id);
    }
}

#[test]
fn find_descriptor_resolves_known_and_returns_none_for_unknown() {
    assert!(find_descriptor("build").is_some());
    assert!(find_descriptor("explore").is_some());
    assert!(find_descriptor("general").is_some());
    assert!(find_descriptor("does_not_exist_zz").is_none());
}
