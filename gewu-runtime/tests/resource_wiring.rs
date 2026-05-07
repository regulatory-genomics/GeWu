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
    let runtime = build_runtime(dir.path()).await;

    assert!(!runtime.capability_registry().contains("resource.search"));
    assert!(!runtime.capability_registry().contains("resource.resolve"));
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
