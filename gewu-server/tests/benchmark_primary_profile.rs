use gewu_server::config::Config;

#[test]
fn custom_primary_profile_requires_explicit_opt_in() {
    let opted_in: Result<Config, _> = serde_json::from_str(r#"{"agents":{"benchmark":{"base":"general","description":"Benchmark agent","primary":true,"instructions":"Research carefully","tools":["read_file"]}}}"#);
    assert!(opted_in.is_ok(), "explicit primary profile must be configurable: {opted_in:?}");
    let config = opted_in.unwrap();
    let names = gewu_server::registry::primary_agent_names(&config.agents);
    assert!(names.iter().any(|s|s=="benchmark"));
    assert!(names.iter().any(|s|s=="build"));
    let default: Config = serde_json::from_str(r#"{"agents":{"worker":{"base":"general","description":"Worker"}}}"#).unwrap();
    assert!(!gewu_server::registry::primary_agent_names(&default.agents).iter().any(|s|s=="worker"));
}

#[test]
fn opted_in_primary_profile_can_disable_all_tools_for_bounded_decisions(){
    let primary:Config=serde_json::from_str(r#"{"agents":{"decision":{"base":"general","description":"No-tool decision model","primary":true,"instructions":"Return JSON only","tools":[]}}}"#).unwrap();
    assert!(primary.validate().is_ok(),"primary decision profiles must permit an explicit empty tool list");
    let subagent:Config=serde_json::from_str(r#"{"agents":{"worker":{"base":"general","description":"Worker","tools":[]}}}"#).unwrap();
    assert!(subagent.validate().is_err(),"preserve existing subagent validation");
}
