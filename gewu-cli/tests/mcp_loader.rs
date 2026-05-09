use gewu_cli::config::LoadedConfig;
use gewu_tools::mcp::types::McpServerConfig;
use std::fs;
use tempfile::TempDir;

#[test]
fn parses_stdio_mcp_server() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "mcpServers": {
                "fs": { "command": "uvx", "args": ["mcp-server-fs"], "env": { "FOO": "bar" } }
            }
        }"#,
    )
    .unwrap();

    let loaded = LoadedConfig::load(Some(path.to_str().unwrap())).expect("load ok");
    let server = loaded.config.mcp_servers.get("fs").expect("server present");
    match server {
        McpServerConfig::Stdio { command, args, env } => {
            assert_eq!(command, "uvx");
            assert_eq!(args, &vec!["mcp-server-fs".to_string()]);
            assert_eq!(env.get("FOO").map(|s| s.as_str()), Some("bar"));
        }
        other => panic!("expected Stdio, got {other:?}"),
    }
}

#[test]
fn parses_sse_mcp_server() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "mcpServers": {
                "remote": { "url": "http://localhost:8080/sse" }
            }
        }"#,
    )
    .unwrap();

    let loaded = LoadedConfig::load(Some(path.to_str().unwrap())).expect("load ok");
    let server = loaded
        .config
        .mcp_servers
        .get("remote")
        .expect("server present");
    match server {
        McpServerConfig::Sse { url } => {
            assert_eq!(url, "http://localhost:8080/sse");
        }
        other => panic!("expected Sse, got {other:?}"),
    }
}

#[test]
fn rejects_invalid_mcp_server_shape() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    fs::write(
        &path,
        r#"{
            "mcpServers": {
                "broken": { "garbage": true }
            }
        }"#,
    )
    .unwrap();

    let result = LoadedConfig::load(Some(path.to_str().unwrap()));
    assert!(
        result.is_err(),
        "expected parse error for invalid mcp server"
    );
}
