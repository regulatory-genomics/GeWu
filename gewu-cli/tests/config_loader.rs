use gewu_cli::config::LoadedConfig;
use std::fs;
use tempfile::TempDir;

#[test]
fn loads_explicit_path_jsonc_with_comments() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.jsonc");
    fs::write(
        &path,
        r#"{
            // commented out
            "model": "test-model",
            "provider": "openai"
        }"#,
    )
    .unwrap();

    let loaded = LoadedConfig::load(Some(path.to_str().unwrap())).expect("load ok");
    assert_eq!(loaded.config.model.as_deref(), Some("test-model"));
    assert_eq!(loaded.config.provider.as_deref(), Some("openai"));
    assert_eq!(loaded.path.as_deref(), Some(path.as_path()));
}

#[test]
fn missing_path_returns_default_config() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("does_not_exist.json");
    let loaded = LoadedConfig::load(Some(path.to_str().unwrap())).expect("load ok");
    assert!(loaded.path.is_none());
    assert!(loaded.config.model.is_none());
    assert!(loaded.config.api_key.is_none());
}

#[test]
fn malformed_json_returns_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bad.json");
    fs::write(&path, "{ this is not json }").unwrap();
    let result = LoadedConfig::load(Some(path.to_str().unwrap()));
    assert!(
        result.is_err(),
        "expected parse error, got {:?}",
        result.ok()
    );
}

#[test]
fn max_output_tokens_aliases_accepted() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    fs::write(&path, r#"{ "maxTokens": 4096 }"#).unwrap();
    let loaded = LoadedConfig::load(Some(path.to_str().unwrap())).expect("load ok");
    assert_eq!(loaded.config.max_output_tokens, Some(4096));
}
