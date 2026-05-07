use gewu_core::traits::{Tool, ToolFactory};
use gewu_core::types::{ToolContext, ToolExecutionPolicy};
use gewu_resource_core::{
    CatalogLoader, InMemoryResourceProvider, ResolveRequest, ResolveStatus, Resource,
    ResourceCatalog, ResourceCatalogEntry, ResourceRegistry, ResourceStatus, ResourceToolFactory,
    ResourceUri, SearchRequest,
};
use serde_json::json;

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
