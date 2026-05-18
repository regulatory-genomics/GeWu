use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent, ToolContext};
use gewu_runtime::{GeWuRuntime, PluginHost, RuntimeFactory};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use std::error::Error;
use std::sync::Arc;

struct FinishLlm;

#[async_trait]
impl LlmProvider for FinishLlm {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        Ok(Box::pin(stream::iter(vec![Ok(ProviderEvent::Finish {
            reason: Some("resource wiring checked".to_string()),
        })])))
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec![])
    }
}

async fn build_runtime(project_root: &std::path::Path) -> GeWuRuntime {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    gewu_storage::run_migrations(&pool).await.unwrap();

    GeWuRuntime::builder()
        .provider(Arc::new(FinishLlm))
        .pool(pool)
        .register_builtin_capabilities()
        .with_resource_dir(project_root.join(".gewu/resources"))
        .build()
        .unwrap()
}

async fn build_bare_runtime() -> GeWuRuntime {
    GeWuRuntime::builder()
        .provider(Arc::new(FinishLlm))
        .pool(pool().await)
        .register_builtin_capabilities()
        .build()
        .unwrap()
}

async fn pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    gewu_storage::run_migrations(&pool).await.unwrap();
    pool
}

fn write_catalog(project_root: &std::path::Path) {
    let resources_dir = project_root.join(".gewu/resources");
    std::fs::create_dir_all(&resources_dir).unwrap();
    std::fs::write(
        resources_dir.join("catalog.yaml"),
        r#"
resources:
  - uri: gewu://document/readme
    name: Project Readme
    aliases: [docs]
    labels: {type: documentation}
    status: available
    spec: {path: README.md}
"#,
    )
    .unwrap();
}

fn write_gene_resolver_tool(project_root: &std::path::Path) {
    let tools_dir = project_root.join(".gewu/tools");
    std::fs::create_dir_all(&tools_dir).unwrap();
    std::fs::write(
        tools_dir.join("gene.resolve_symbol.yaml"),
        r#"
name: gene.resolve_symbol
description: Resolve a gene symbol to a locus.
command: /bin/echo
input_schema: { type: object }
"#,
    )
    .unwrap();
}

#[tokio::test]
async fn runtime_registers_resource_tools_when_catalog_exists() {
    let dir = tempfile::tempdir().unwrap();
    write_catalog(dir.path());

    let runtime = build_runtime(dir.path()).await;

    assert!(runtime.capability_registry().contains("resource.search"));
    assert!(runtime.capability_registry().contains("resource.resolve"));

    let tools = runtime
        .resolve_capabilities_in_directory(
            &[
                "resource.search".to_string(),
                "resource.resolve".to_string(),
            ],
            dir.path(),
        )
        .unwrap();
    let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(names, vec!["resource.search", "resource.resolve"]);

    let search = tools
        .iter()
        .find(|tool| tool.name() == "resource.search")
        .unwrap();
    let result = search
        .execute(
            json!({"query":"docs"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], true);
    assert_eq!(
        output["data"]["candidates"][0]["uri"],
        "gewu://document/readme"
    );
}

#[tokio::test]
async fn runtime_does_not_register_resource_tools_without_catalogs() {
    let dir = tempfile::tempdir().unwrap();
    let original_cwd = std::env::current_dir().unwrap();

    struct RestoreCwd(std::path::PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    let _restore = RestoreCwd(original_cwd);
    std::env::set_current_dir(dir.path()).unwrap();

    let runtime = build_bare_runtime().await;

    assert!(!runtime.capability_registry().contains("resource.search"));
    assert!(!runtime.capability_registry().contains("resource.resolve"));
    assert!(!runtime.capability_registry().contains("resource.register"));
}

#[tokio::test]
async fn runtime_resource_register_available_for_explicit_empty_resource_dir() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = build_runtime(dir.path()).await;

    assert!(runtime.capability_registry().contains("resource.register"));
    assert!(runtime.capability_registry().contains("resource.resolve"));

    let tools = runtime
        .resolve_capabilities_in_directory(
            &[
                "resource.register".to_string(),
                "resource.resolve".to_string(),
            ],
            dir.path(),
        )
        .unwrap();
    let register = tools
        .iter()
        .find(|tool| tool.name() == "resource.register")
        .unwrap();

    let result = register
        .execute(
            json!({
                "resource": {
                    "uri": "gewu://document/registered-runtime",
                    "kind": "document",
                    "name": "Registered Runtime Doc",
                    "aliases": ["runtime-doc"],
                    "labels": {"project":"runtime"},
                    "spec": {"path":"docs/runtime.md"},
                    "status": "available"
                },
                "reason": "runtime wiring test"
            }),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], true);
    assert_eq!(output["data"]["available_now"], true);

    let resolve = tools
        .iter()
        .find(|tool| tool.name() == "resource.resolve")
        .unwrap();
    let result = resolve
        .execute(
            json!({"query":"runtime-doc"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], true);
    assert_eq!(output["data"]["status"], "ok");
    assert_eq!(
        output["data"]["selected"]["uri"],
        "gewu://document/registered-runtime"
    );
    assert!(
        dir.path()
            .join(".gewu/resources/agent-registered.yaml")
            .exists()
    );
}

#[tokio::test]
async fn build_specialist_can_use_resources_and_gene_resolver_in_project_workdir() {
    let dir = tempfile::tempdir().unwrap();
    write_gene_resolver_tool(dir.path());
    let pool = pool().await;
    let provider: Arc<dyn LlmProvider> = Arc::new(FinishLlm);
    let plugin_host = PluginHost::empty();
    let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs_in_directory(
        provider,
        pool,
        &plugin_host,
        &[],
        &[],
        &[],
        &[],
        dir.path(),
    )
    .unwrap();

    let build = runtime.find_specialist("build").unwrap();
    assert!(
        build
            .capabilities
            .contains(&"resource.register".to_string())
    );
    assert!(build.capabilities.contains(&"resource.resolve".to_string()));
    assert!(build.capabilities.contains(&"resource.search".to_string()));
    assert!(
        build
            .capabilities
            .contains(&"gene.resolve_symbol".to_string())
    );

    let tools = runtime
        .resolve_capabilities_in_directory(&build.capabilities, dir.path())
        .unwrap();
    let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
    assert!(names.contains(&"resource.register"));
    assert!(names.contains(&"resource.resolve"));
    assert!(names.contains(&"resource.search"));
    assert!(names.contains(&"gene.resolve_symbol"));
}

#[tokio::test]
async fn runtime_factory_registers_resource_tools_from_non_cwd_workdir() {
    let original_cwd = std::env::current_dir().unwrap();
    let cwd_dir = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    write_catalog(project_dir.path());

    struct RestoreCwd(std::path::PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    let _restore = RestoreCwd(original_cwd);
    std::env::set_current_dir(cwd_dir.path()).unwrap();

    let pool = pool().await;
    let provider: Arc<dyn LlmProvider> = Arc::new(FinishLlm);
    let plugin_host = PluginHost::empty();
    let runtime = RuntimeFactory::build_with_manifest_and_runtime_plugin_dirs_in_directory(
        provider,
        pool,
        &plugin_host,
        &[],
        &[],
        &[],
        &[],
        project_dir.path(),
    )
    .unwrap();

    assert!(runtime.capability_registry().contains("resource.search"));
    assert!(runtime.capability_registry().contains("resource.resolve"));
}
