use gewu_core::traits::{Tool, ToolFactory};
use gewu_core::types::{ToolContext, ToolExecutionPolicy};
use gewu_resource_core::{
    CatalogLoader, DescribeRequest, InMemoryResourceProvider, MutableResourceProvider,
    ResolveRequest, ResolveStatus, Resource, ResourceCatalog, ResourceCatalogEntry,
    ResourceRegistry, ResourceStatus, ResourceToolFactory, ResourceUri, SearchRequest,
};
use serde_json::json;
use std::sync::Arc;

#[test]
fn resource_uri_parses_formats_and_rejects_invalid_parts() {
    let uri: ResourceUri = "gewu://dataset/sample-1".parse().unwrap();
    assert_eq!(uri.kind(), "dataset");
    assert_eq!(uri.id(), "sample-1");
    assert_eq!(uri.to_string(), "gewu://dataset/sample-1");

    for invalid in [
        "dataset/sample-1",
        "gewu:///sample-1",
        "gewu://dataset/",
        "gewu://data/set/sample-1",
        "gewu://data:set/sample-1",
        "gewu://dataset/sample:1",
    ] {
        assert!(invalid.parse::<ResourceUri>().is_err(), "{invalid}");
    }
}

#[test]
fn resource_model_builds_search_text_and_matches_labels() {
    let resource = Resource::new(
        ResourceUri::new("dataset", "sample-1").unwrap(),
        "Example Sample",
    )
    .with_aliases(["alpha", "beta"])
    .with_label("project", "atlas")
    .with_annotation("note", "curated")
    .with_spec(json!({"path":"/tmp/sample.tsv"}))
    .with_status(ResourceStatus::Available);

    assert!(resource.searchable_text().contains("Example Sample"));
    assert!(resource.searchable_text().contains("alpha"));
    assert!(resource.searchable_text().contains("curated"));
    assert!(resource.label_matches(&[("project".into(), "atlas".into())]));
    assert!(!resource.label_matches(&[("project".into(), "other".into())]));
    assert_eq!(
        Resource::new(
            ResourceUri::new("dataset", "unchecked").unwrap(),
            "Unchecked"
        )
        .status,
        ResourceStatus::Unchecked
    );
}

fn registry_with(resources: Vec<Resource>) -> ResourceRegistry {
    ResourceRegistry::new().with_provider(InMemoryResourceProvider::new("memory", resources))
}

#[test]
fn registry_builder_registers_providers_and_counts_them() {
    let provider = InMemoryResourceProvider::new(
        "memory",
        vec![Resource::new(
            ResourceUri::new("dataset", "one").unwrap(),
            "One",
        )],
    );
    let mut registry = ResourceRegistry::new();
    assert_eq!(registry.provider_count(), 0);
    registry.register(provider);
    assert_eq!(registry.provider_count(), 1);

    let registry =
        ResourceRegistry::new().with_provider(InMemoryResourceProvider::new("empty", vec![]));
    assert_eq!(registry.provider_count(), 1);
}

#[tokio::test]
async fn registry_resolves_unique_and_ambiguous_resources() {
    let first =
        Resource::new(ResourceUri::new("dataset", "one").unwrap(), "Shared").with_aliases(["same"]);
    let second =
        Resource::new(ResourceUri::new("dataset", "two").unwrap(), "Other").with_aliases(["same"]);
    let registry = registry_with(vec![first.clone(), second]);

    let unique = registry
        .resolve(ResolveRequest::new("gewu://dataset/one"))
        .await
        .unwrap();
    assert_eq!(unique.status, ResolveStatus::Ok);
    assert_eq!(unique.selected.unwrap().uri, first.uri.to_string());

    let ambiguous = registry.resolve(ResolveRequest::new("same")).await.unwrap();
    assert_eq!(ambiguous.status, ResolveStatus::Ambiguous);
    assert_eq!(ambiguous.candidates.len(), 2);
}

#[tokio::test]
async fn registry_search_filters_by_labels_and_limit() {
    let keep = Resource::new(ResourceUri::new("dataset", "keep").unwrap(), "Keep")
        .with_label("project", "atlas");
    let skip = Resource::new(ResourceUri::new("dataset", "skip").unwrap(), "Skip")
        .with_label("project", "other");
    let registry = registry_with(vec![keep.clone(), skip]);

    let response = registry
        .search(
            SearchRequest::new("dataset")
                .with_label("project", "atlas")
                .with_limit(1),
        )
        .await
        .unwrap();

    assert_eq!(response.candidates.len(), 1);
    assert_eq!(response.candidates[0].uri, keep.uri.to_string());
}

#[test]
fn catalog_loader_reads_file_and_directory_in_sorted_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("b.yml"),
        r#"
resources:
  - uri: gewu://dataset/b
    name: B Resource
    aliases: [bravo]
    labels: {project: atlas}
    status: missing
    spec: {rank: 2}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("a.yaml"),
        r#"
resources:
  - uri: gewu://dataset/a
    name: A Resource
    aliases: [alpha]
    labels: {project: atlas}
    status: available
    spec: {rank: 1}
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "ignored").unwrap();

    let file_loaded = CatalogLoader::load_file(dir.path().join("a.yaml")).unwrap();
    assert_eq!(file_loaded.len(), 1);
    assert_eq!(file_loaded[0].uri.to_string(), "gewu://dataset/a");

    let loaded = CatalogLoader::load_dir(dir.path()).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].uri.to_string(), "gewu://dataset/a");
    assert_eq!(loaded[0].aliases, vec!["alpha"]);
    assert_eq!(loaded[0].status, ResourceStatus::Available);
    assert_eq!(loaded[0].spec["rank"], 1);
    assert_eq!(loaded[1].uri.to_string(), "gewu://dataset/b");
    assert_eq!(loaded[1].status, ResourceStatus::Missing);

    let entry = ResourceCatalogEntry {
        uri: ResourceUri::new("dataset", "inline").unwrap(),
        name: "Inline".into(),
        aliases: vec![],
        labels: Default::default(),
        annotations: Default::default(),
        spec: json!({}),
        status: ResourceStatus::Available,
        provenance: None,
    };
    let catalog = ResourceCatalog {
        resources: vec![entry],
    };
    assert_eq!(catalog.into_resources().unwrap().len(), 1);
}

#[tokio::test]
async fn resource_tools_return_json_and_factory_exposes_exact_capabilities() {
    let resource = Resource::new(ResourceUri::new("dataset", "sample").unwrap(), "Sample")
        .with_aliases(["sample-alias"])
        .with_label("project", "atlas");
    let registry = std::sync::Arc::new(registry_with(vec![resource]));
    let tool = gewu_resource_core::tools::ResourceResolveTool::new(registry.clone());

    assert_eq!(tool.name(), "resource.resolve");
    assert_eq!(tool.execution_policy(), ToolExecutionPolicy::ParallelSafe);
    assert!(tool.schema().schema.metadata().title.is_some());

    let result = tool
        .execute(
            json!({"query":"sample-alias"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], true);
    assert_eq!(output["data"]["status"], "ok");
    assert_eq!(output["data"]["selected"]["uri"], "gewu://dataset/sample");

    let invalid = tool
        .execute(json!({}), &ToolContext::new("session", "message"))
        .await
        .unwrap();
    let invalid_output: serde_json::Value = serde_json::from_str(&invalid.output).unwrap();
    assert_eq!(invalid_output["ok"], false);
    assert_eq!(invalid_output["error"]["code"], "invalid_args");
    assert_eq!(invalid_output["error"]["retryable"], false);
    assert!(
        invalid_output["error"]["message"]
            .as_str()
            .unwrap()
            .contains("query")
    );
    assert!(
        !invalid_output["error"]["suggested_fix"]
            .as_str()
            .unwrap()
            .is_empty()
    );

    let factory = ResourceToolFactory::new(registry);
    let mut capabilities = factory.capabilities();
    capabilities.sort();
    assert_eq!(
        capabilities,
        vec![
            "resource.describe",
            "resource.resolve",
            "resource.search",
            "resource.validate"
        ]
    );
}

fn resource_register_subject(
    project_root: &std::path::Path,
) -> (
    Arc<ResourceRegistry>,
    Arc<MutableResourceProvider>,
    gewu_resource_core::ResourceRegisterTool,
) {
    let mutable_provider = Arc::new(MutableResourceProvider::new("agent-registered"));
    let registry = Arc::new(ResourceRegistry::new().with_provider(mutable_provider.clone()));
    let tool = gewu_resource_core::ResourceRegisterTool::new(
        registry.clone(),
        mutable_provider.clone(),
        project_root.to_path_buf(),
    );
    (registry, mutable_provider, tool)
}

fn register_resource() -> Resource {
    Resource::new(
        ResourceUri::new("document", "registered").unwrap(),
        "Registered Doc",
    )
    .with_aliases(["reg-doc"])
    .with_label("project", "atlas")
    .with_spec(json!({"path":"docs/registered.md"}))
    .with_status(ResourceStatus::Available)
}

#[tokio::test]
async fn resource_register_dry_run_does_not_write_or_mutate_registry() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, _provider, tool) = resource_register_subject(dir.path());
    let catalog_path = dir.path().join(".gewu/resources/agent-registered.yaml");

    let result = tool
        .execute(
            json!({"resource": register_resource(), "dry_run": true, "reason":"preview"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], true);
    assert_eq!(output["data"]["dry_run"], true);
    assert_eq!(output["data"]["operation"], "create");
    assert_eq!(output["data"]["will_update_registry"], true);
    assert_eq!(output["data"]["registry_updated"], false);
    assert_eq!(output["data"]["available_now"], false);
    assert!(!catalog_path.exists());
    assert!(!dir.path().join(".gewu/resources").exists());

    let resolved = registry
        .resolve(ResolveRequest::new("reg-doc"))
        .await
        .unwrap();
    assert_eq!(resolved.status, ResolveStatus::NotFound);
}

#[tokio::test]
async fn resource_register_writes_yaml_and_updates_same_registry_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, _provider, tool) = resource_register_subject(dir.path());
    let catalog_path = dir.path().join(".gewu/resources/agent-registered.yaml");

    let result = tool
        .execute(
            json!({"resource": register_resource()}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], true);
    assert_eq!(output["data"]["dry_run"], false);
    assert_eq!(output["data"]["operation"], "create");
    assert_eq!(output["data"]["uri"], "gewu://document/registered");
    assert_eq!(output["data"]["registry_updated"], true);
    assert_eq!(output["data"]["available_now"], true);
    assert_eq!(
        output["data"]["verify"],
        json!({"tool":"resource.resolve","args":{"query":"gewu://document/registered"}})
    );
    assert!(catalog_path.exists());

    let loaded = CatalogLoader::load_file(&catalog_path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].uri.to_string(), "gewu://document/registered");

    let resolved = registry
        .resolve(ResolveRequest::new("reg-doc"))
        .await
        .unwrap();
    assert_eq!(resolved.status, ResolveStatus::Ok);
    assert_eq!(resolved.selected.unwrap().uri, "gewu://document/registered");
    let described = registry
        .describe(DescribeRequest {
            uri: "gewu://document/registered".parse().unwrap(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(described.name, "Registered Doc");
    let search = registry.search(SearchRequest::new("atlas")).await.unwrap();
    assert_eq!(search.candidates[0].uri, "gewu://document/registered");
}

#[tokio::test]
async fn resource_register_modes_consider_static_catalog_resources() {
    let dir = tempfile::tempdir().unwrap();
    let mutable_provider = Arc::new(MutableResourceProvider::new("agent-registered"));
    let static_resource = Resource::new(
        ResourceUri::new("document", "registered").unwrap(),
        "Original Static Doc",
    )
    .with_aliases(["reg-doc"])
    .with_status(ResourceStatus::Available);
    let mut registry = ResourceRegistry::new();
    registry.register(mutable_provider.clone());
    registry.register(InMemoryResourceProvider::new(
        "static",
        vec![static_resource],
    ));
    let registry = Arc::new(registry);
    let tool = gewu_resource_core::ResourceRegisterTool::new(
        registry.clone(),
        mutable_provider,
        dir.path().to_path_buf(),
    );

    let create_only = tool
        .execute(
            json!({"resource": register_resource(), "mode": "create_only"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let create_only_output: serde_json::Value = serde_json::from_str(&create_only.output).unwrap();
    assert_eq!(create_only_output["ok"], false);
    assert_eq!(create_only_output["error"]["code"], "resource_exists");

    let update_only = tool
        .execute(
            json!({"resource": register_resource(), "mode": "update_only"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let update_only_output: serde_json::Value = serde_json::from_str(&update_only.output).unwrap();
    assert_eq!(update_only_output["ok"], true);
    assert_eq!(update_only_output["data"]["operation"], "update");

    let described = registry
        .describe(DescribeRequest {
            uri: "gewu://document/registered".parse().unwrap(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(described.name, "Registered Doc");
    assert_eq!(described.spec["path"], "docs/registered.md");
}

#[cfg(unix)]
#[tokio::test]
async fn resource_register_rejects_symlinked_resource_directory() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".gewu")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".gewu/resources")).unwrap();
    let (_registry, _provider, tool) = resource_register_subject(dir.path());

    let result = tool
        .execute(
            json!({"resource": register_resource()}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], false);
    assert_eq!(output["error"]["code"], "unsafe_catalog_path");
    assert!(!outside.path().join("agent-registered.yaml").exists());
}

#[tokio::test]
async fn resource_register_rejects_unsafe_catalog_paths_and_kind_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _provider, tool) = resource_register_subject(dir.path());

    for unsafe_catalog in [
        "/tmp/agent-registered.yaml",
        "../agent-registered.yaml",
        ".gewu/resources/nested/catalog.yaml",
        "agent-registered.json",
    ] {
        let result = tool
            .execute(
                json!({"resource": register_resource(), "catalog": unsafe_catalog}),
                &ToolContext::new("session", "message"),
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["ok"], false, "{unsafe_catalog}");
        assert_eq!(output["error"]["code"], "unsafe_catalog_path");
    }

    let mut mismatched = register_resource();
    mismatched.kind = "artifact".into();
    let result = tool
        .execute(
            json!({"resource": mismatched}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], false);
    assert_eq!(output["error"]["code"], "kind_mismatch");
}

#[test]
fn resource_register_capability_only_appears_with_registration_factory() {
    let mutable_provider = Arc::new(MutableResourceProvider::new("agent-registered"));
    let registry = Arc::new(ResourceRegistry::new().with_provider(mutable_provider.clone()));

    let read_only = ResourceToolFactory::new(registry.clone());
    assert!(
        !read_only
            .capabilities()
            .contains(&"resource.register".into())
    );

    let registration = ResourceToolFactory::with_registration(registry, mutable_provider);
    assert!(
        registration
            .capabilities()
            .contains(&"resource.register".into())
    );
}

#[tokio::test]
async fn resource_validate_invalid_value_returns_top_level_failure_with_guidance() {
    let registry = Arc::new(registry_with(vec![]));
    let tool = gewu_resource_core::tools::ResourceValidateTool::new(registry);

    let result = tool
        .execute(
            json!({"value":{"path":"/data2/example.ucf","assay":"ATAC"},"kind":"sample"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], false, "expected top-level ok=false");
    let error = &output["error"];
    assert_eq!(
        error["code"].as_str().unwrap(),
        "invalid_resource_uri",
        "expected error.code=invalid_resource_uri"
    );
    assert_eq!(
        error["field"].as_str().unwrap(),
        "value",
        "expected error.field=value"
    );
    assert!(
        error["suggested_fix"]
            .as_str()
            .unwrap()
            .contains("gewu://sample/example"),
        "expected suggested_fix to contain gewu://sample/example"
    );
}

#[tokio::test]
async fn resource_validate_ucf_object_suggests_canonical_sample_uri() {
    let registry = Arc::new(registry_with(vec![]));
    let tool = gewu_resource_core::tools::ResourceValidateTool::new(registry);

    let result = tool
        .execute(
            json!({
                "value": {
                    "path": "/data2/litian/macrophage_bk/data/ATAC/ucf/E3-1_ATAC12.filtered.ucf",
                    "assay": "ATAC-seq"
                },
                "kind": "sample"
            }),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], false);
    assert_eq!(output["error"]["code"], "invalid_resource_uri");
    assert!(
        output["error"]["suggested_fix"]
            .as_str()
            .unwrap()
            .contains("gewu://sample/E3-1_ATAC12")
    );
}

#[tokio::test]
async fn resource_validate_suggestion_prefers_sample_then_sample_id_then_path_basename() {
    let registry = Arc::new(registry_with(vec![]));
    let tool = gewu_resource_core::tools::ResourceValidateTool::new(registry);

    for (value, expected_uri) in [
        (
            json!({
                "sample": "preferred_sample",
                "sample_id": "secondary_sample",
                "path": "/data2/example.ucf"
            }),
            "gewu://sample/preferred_sample",
        ),
        (
            json!({
                "sample_id": "secondary_sample",
                "path": "/data2/example.ucf"
            }),
            "gewu://sample/secondary_sample",
        ),
        (
            json!({"path": "/data2/example.filtered.ucf"}),
            "gewu://sample/example",
        ),
    ] {
        let result = tool
            .execute(
                json!({"value": value, "kind": "sample"}),
                &ToolContext::new("session", "message"),
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            output["error"]["suggested_fix"]
                .as_str()
                .unwrap()
                .contains(expected_uri),
            "expected suggested_fix to contain {expected_uri}"
        );
    }
}

#[tokio::test]
async fn resource_register_object_uri_error_shows_canonical_uri_example() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _provider, tool) = resource_register_subject(dir.path());

    let result = tool
        .execute(
            json!({"resource": {"uri": {"kind":"sample","id":"ucf_atac_hg38"}, "name": "UCF Sample"}}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], false, "expected top-level ok=false");
    let error = &output["error"];
    assert_eq!(
        error["code"].as_str().unwrap(),
        "invalid_args",
        "expected error.code=invalid_args"
    );
    assert!(
        error["suggested_fix"]
            .as_str()
            .unwrap()
            .contains("\"uri\": \"gewu://sample/ucf_atac_hg38\""),
        "expected suggested_fix to contain the canonical URI example"
    );
}

#[tokio::test]
async fn resource_register_success_returns_next_actions() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _provider, tool) = resource_register_subject(dir.path());

    let result = tool
        .execute(
            json!({"resource": register_resource()}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], true, "expected top-level ok=true");
    let data = &output["data"];
    assert!(
        data["next_actions"].is_array(),
        "expected data.next_actions to be an array"
    );
    assert_eq!(
        data["next_actions"][0]["tool"].as_str().unwrap(),
        "resource.resolve",
        "expected next_actions[0].tool == resource.resolve"
    );
    assert_eq!(
        data["next_actions"][0]["args"]["query"].as_str().unwrap(),
        "gewu://document/registered",
        "expected next_actions[0].args.query == gewu://document/registered"
    );
}

#[tokio::test]
async fn resource_validate_missing_resource_returns_top_level_failure() {
    let registry = Arc::new(registry_with(vec![]));
    let tool = gewu_resource_core::tools::ResourceValidateTool::new(registry);

    let result = tool
        .execute(
            json!({"value":"gewu://sample/missing","kind":"sample"}),
            &ToolContext::new("session", "message"),
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();

    assert_eq!(output["ok"], false, "expected top-level ok=false");
    let error = &output["error"];
    assert_eq!(
        error["code"].as_str().unwrap(),
        "resource_not_found",
        "expected error.code=resource_not_found"
    );
    assert_eq!(
        error["retryable"].as_bool().unwrap(),
        false,
        "expected error.retryable=false"
    );
}
