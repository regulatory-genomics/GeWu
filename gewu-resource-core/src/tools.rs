use std::error::Error;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolExecutionPolicy, ToolResult};
use schemars::{schema::RootSchema, schema_for};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::catalog::{ResourceCatalog, ResourceCatalogEntry};
use crate::provider::MutableResourceProvider;
use crate::query::{
    DescribeRequest, RegisterMode, RegisterRequest, ResolveRequest, SearchRequest, ValidateRequest,
};
use crate::registry::ResourceRegistry;
use crate::uri::ResourceUri;

fn success_result(
    tool_name: &str,
    data: impl Serialize,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    Ok(ToolResult::new(
        tool_name,
        serde_json::to_string(&json!({ "ok": true, "data": data }))?,
    ))
}

fn recoverable_error_result(
    tool_name: &str,
    code: &str,
    message: impl Into<String>,
    suggested_fix: &str,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    Ok(ToolResult::new(
        tool_name,
        serde_json::to_string(&json!({
            "ok": false,
            "error": {
                "code": code,
                "retryable": false,
                "message": message.into(),
                "suggested_fix": suggested_fix,
            }
        }))?,
    ))
}

macro_rules! resource_tool {
    ($name:ident, $request:ty, $tool_name:literal, $description:literal, $method:ident) => {
        pub struct $name {
            registry: Arc<ResourceRegistry>,
        }

        impl $name {
            pub fn new(registry: Arc<ResourceRegistry>) -> Self {
                Self { registry }
            }
        }

        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }

            fn description(&self) -> &str {
                $description
            }

            fn schema(&self) -> RootSchema {
                schema_for!($request)
            }

            fn execution_policy(&self) -> ToolExecutionPolicy {
                ToolExecutionPolicy::ParallelSafe
            }

            async fn execute(
                &self,
                args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
                let request: $request = match serde_json::from_value(args) {
                    Ok(request) => request,
                    Err(error) => {
                        return recoverable_error_result(
                            $tool_name,
                            "invalid_args",
                            error.to_string(),
                            "Call this tool with arguments matching its JSON schema.",
                        );
                    }
                };
                let response = match self.registry.$method(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        return recoverable_error_result(
                            $tool_name,
                            "resource_error",
                            error.to_string(),
                            "Check the resource registry configuration and retry with a valid resource query.",
                        );
                    }
                };
                success_result($tool_name, response)
            }
        }
    };
}

resource_tool!(
    ResourceSearchTool,
    SearchRequest,
    "resource.search",
    "Search registered resources",
    search
);
resource_tool!(
    ResourceResolveTool,
    ResolveRequest,
    "resource.resolve",
    "Resolve a resource query to one resource or candidates",
    resolve
);
resource_tool!(
    ResourceDescribeTool,
    DescribeRequest,
    "resource.describe",
    "Describe a resource by URI",
    describe
);

pub struct ResourceValidateTool {
    registry: Arc<ResourceRegistry>,
}

impl ResourceValidateTool {
    pub fn new(registry: Arc<ResourceRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ResourceValidateTool {
    fn name(&self) -> &str {
        "resource.validate"
    }

    fn description(&self) -> &str {
        "Validate that a resource value is usable"
    }

    fn schema(&self) -> RootSchema {
        schema_for!(ValidateRequest)
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::ParallelSafe
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let request: ValidateRequest = match serde_json::from_value(args) {
            Ok(request) => request,
            Err(error) => {
                return recoverable_error_result(
                    "resource.validate",
                    "invalid_args",
                    error.to_string(),
                    "Call this tool with arguments matching its JSON schema.",
                );
            }
        };
        let response = match self.registry.validate(request).await {
            Ok(response) => response,
            Err(error) => {
                return recoverable_error_result(
                    "resource.validate",
                    "resource_error",
                    error.to_string(),
                    "Check the resource registry configuration and retry with a valid resource query.",
                );
            }
        };

        if response.ok {
            success_result("resource.validate", response)
        } else {
            let tool_result = ToolResult::new(
                "resource.validate",
                serde_json::to_string(&json!({
                    "ok": false,
                    "error": {
                        "code": response.code.unwrap_or_else(|| "resource_validation_failed".into()),
                        "retryable": response.retryable.unwrap_or(false),
                        "message": response.message,
                        "field": response.field,
                        "suggested_fix": response.suggested_fix,
                        "details": response.details,
                        "warnings": response.warnings
                    }
                }))?,
            );
            Ok(tool_result)
        }
    }
}

pub struct ResourceRegisterTool {
    registry: Arc<ResourceRegistry>,
    mutable_provider: Arc<MutableResourceProvider>,
    working_directory: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

struct CatalogLock {
    path: PathBuf,
}

impl CatalogLock {
    fn acquire(path: PathBuf) -> std::io::Result<Self> {
        for attempt in 0..50 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    file.sync_all()?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 49 {
                        return Err(error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "resource catalog lock is busy",
        ))
    }
}

impl Drop for CatalogLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ResourceRegisterTool {
    pub fn new(
        registry: Arc<ResourceRegistry>,
        mutable_provider: Arc<MutableResourceProvider>,
        working_directory: PathBuf,
    ) -> Self {
        Self::with_lock(
            registry,
            mutable_provider,
            working_directory,
            Arc::new(Mutex::new(())),
        )
    }

    pub fn with_lock(
        registry: Arc<ResourceRegistry>,
        mutable_provider: Arc<MutableResourceProvider>,
        working_directory: PathBuf,
        write_lock: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            registry,
            mutable_provider,
            working_directory,
            write_lock,
        }
    }

    fn catalog_path(&self, catalog: &str) -> Result<PathBuf, &'static str> {
        let path = Path::new(catalog);
        if path.is_absolute() {
            return Err("Catalog must be project-local under .gewu/resources.");
        }
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err("Catalog path cannot contain '..'.");
        }

        let file_name = if path.components().count() == 1 {
            path.file_name()
        } else {
            let mut components = path.components();
            match (
                components.next(),
                components.next(),
                components.next(),
                components.next(),
            ) {
                (
                    Some(Component::Normal(first)),
                    Some(Component::Normal(second)),
                    Some(Component::Normal(file)),
                    None,
                ) if first == ".gewu" && second == "resources" => Some(Path::new(file).as_os_str()),
                _ => None,
            }
        }
        .ok_or("Catalog must be a YAML file directly inside .gewu/resources.")?;

        let file_path = Path::new(file_name);
        let is_yaml = matches!(
            file_path.extension().and_then(|ext| ext.to_str()),
            Some("yaml" | "yml")
        );
        if !is_yaml {
            return Err("Catalog extension must be .yaml or .yml.");
        }

        Ok(self
            .working_directory
            .join(".gewu")
            .join("resources")
            .join(file_path))
    }

    fn ensure_safe_catalog_path(&self, path: &Path) -> Result<(), String> {
        self.ensure_catalog_path(path, true)
    }

    fn validate_catalog_path_without_writes(&self, path: &Path) -> Result<(), String> {
        self.ensure_catalog_path(path, false)
    }

    fn ensure_catalog_path(&self, path: &Path, create_dir: bool) -> Result<(), String> {
        let resources_dir = self.working_directory.join(".gewu").join("resources");
        if create_dir {
            std::fs::create_dir_all(&resources_dir)
                .map_err(|error| format!("failed to create resource catalog directory: {error}"))?;
        } else if !resources_dir.exists() {
            return Ok(());
        }
        let resources_metadata = std::fs::symlink_metadata(&resources_dir)
            .map_err(|error| format!("failed to inspect resource catalog directory: {error}"))?;
        if resources_metadata.file_type().is_symlink() {
            return Err("resource catalog directory cannot be a symlink".into());
        }
        if !resources_metadata.is_dir() {
            return Err("resource catalog path is not a directory".into());
        }

        let canonical_workdir = self.working_directory.canonicalize().map_err(|error| {
            format!("failed to canonicalize project working directory: {error}")
        })?;
        let canonical_resources = resources_dir.canonicalize().map_err(|error| {
            format!("failed to canonicalize resource catalog directory: {error}")
        })?;
        if !canonical_resources.starts_with(&canonical_workdir) {
            return Err("resource catalog directory must stay inside the project workdir".into());
        }

        if path.exists() {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("failed to inspect catalog file: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("resource catalog file cannot be a symlink".into());
            }
            if !metadata.is_file() {
                return Err("resource catalog path must be a regular file".into());
            }
        }
        Ok(())
    }

    fn acquire_catalog_lock(&self, catalog_path: &Path) -> std::io::Result<CatalogLock> {
        let parent = catalog_path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "catalog path must have parent directory",
            )
        })?;
        CatalogLock::acquire(parent.join(".agent-registered.lock"))
    }

    fn read_catalog(&self, path: &Path) -> Result<ResourceCatalog, Box<dyn Error + Send + Sync>> {
        if !path.exists() {
            return Ok(ResourceCatalog::default());
        }
        let content = std::fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }

    fn write_catalog(
        &self,
        path: &Path,
        catalog: &ResourceCatalog,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_yaml::to_string(catalog)?;
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "catalog path must have parent directory",
            )
        })?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "catalog path must have UTF-8 file name",
                )
            })?;
        let process_id = std::process::id();
        for attempt in 0..16 {
            let temp_path = parent.join(format!(".{file_name}.{process_id}.{attempt}.tmp"));
            let mut temp_file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Box::new(error)),
            };
            if let Err(error) = temp_file.write_all(content.as_bytes()) {
                let _ = std::fs::remove_file(&temp_path);
                return Err(Box::new(error));
            }
            if let Err(error) = temp_file.sync_all() {
                let _ = std::fs::remove_file(&temp_path);
                return Err(Box::new(error));
            }
            drop(temp_file);
            if let Err(error) = std::fs::rename(&temp_path, path) {
                let _ = std::fs::remove_file(&temp_path);
                return Err(Box::new(error));
            }
            if let Ok(parent_dir) = std::fs::File::open(parent) {
                parent_dir.sync_all()?;
            }
            return Ok(());
        }
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not create a unique temporary catalog file",
        )))
    }

    async fn registry_contains(
        &self,
        uri: &ResourceUri,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(self
            .registry
            .describe(DescribeRequest { uri: uri.clone() })
            .await?
            .is_some())
    }
}

#[async_trait]
impl Tool for ResourceRegisterTool {
    fn name(&self) -> &str {
        "resource.register"
    }

    fn description(&self) -> &str {
        "Safely register a project-local resource catalog entry"
    }

    fn schema(&self) -> RootSchema {
        schema_for!(RegisterRequest)
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        // Check for object URI before generic parsing
        if let Some(uri_value) = args.pointer("/resource/uri") {
            if uri_value.is_object() {
                return recoverable_error_result(
                    self.name(),
                    "invalid_args",
                    "resource.uri must be a canonical URI string, not an object",
                    "Use a string like \"uri\": \"gewu://sample/ucf_atac_hg38\". Resource kind and id are encoded in the URI.",
                );
            }
        }
        if let Some(uri) = args
            .get("resource")
            .and_then(|resource| resource.get("uri"))
            .and_then(Value::as_str)
            && uri.parse::<ResourceUri>().is_err()
        {
            return recoverable_error_result(
                self.name(),
                "invalid_uri",
                format!("resource.uri is not a valid resource URI: {uri}"),
                "Use a URI like gewu://<kind>/<id> with non-empty kind and id.",
            );
        }
        let request: RegisterRequest = match serde_json::from_value(args) {
            Ok(request) => request,
            Err(error) => {
                return recoverable_error_result(
                    self.name(),
                    "invalid_args",
                    error.to_string(),
                    "Call this tool with a resource object and optional mode, catalog, dry_run, and reason fields.",
                );
            }
        };

        let resource = request.resource;
        if resource.kind != resource.uri.kind() {
            return recoverable_error_result(
                self.name(),
                "kind_mismatch",
                format!(
                    "resource.kind '{}' does not match URI kind '{}'",
                    resource.kind,
                    resource.uri.kind()
                ),
                "Set resource.kind to the kind embedded in resource.uri.",
            );
        }

        let catalog_path = match self.catalog_path(&request.catalog) {
            Ok(path) => path,
            Err(message) => {
                return recoverable_error_result(
                    self.name(),
                    "unsafe_catalog_path",
                    message,
                    "Use a file name such as agent-registered.yaml or .gewu/resources/agent-registered.yaml.",
                );
            }
        };

        if request.dry_run {
            if let Err(message) = self.validate_catalog_path_without_writes(&catalog_path) {
                return recoverable_error_result(
                    self.name(),
                    "unsafe_catalog_path",
                    message,
                    "Use a regular YAML file directly under the project .gewu/resources directory.",
                );
            }
        } else if let Err(message) = self.ensure_safe_catalog_path(&catalog_path) {
            return recoverable_error_result(
                self.name(),
                "unsafe_catalog_path",
                message,
                "Use a regular YAML file directly under the project .gewu/resources directory.",
            );
        }

        let registry_exists = match self.registry_contains(&resource.uri).await {
            Ok(exists) => exists,
            Err(error) => {
                return recoverable_error_result(
                    self.name(),
                    "registry_update_failed",
                    error.to_string(),
                    "Retry after ensuring the shared resource registry is healthy.",
                );
            }
        };

        let mut catalog = if request.dry_run {
            if catalog_path.exists() {
                match self.read_catalog(&catalog_path) {
                    Ok(catalog) => catalog,
                    Err(error) => {
                        return recoverable_error_result(
                            self.name(),
                            "catalog_parse_failed",
                            error.to_string(),
                            "Fix the YAML catalog syntax or choose a different project-local catalog file.",
                        );
                    }
                }
            } else {
                ResourceCatalog::default()
            }
        } else {
            let _write_guard = self.write_lock.lock().await;
            let _catalog_lock = match self.acquire_catalog_lock(&catalog_path) {
                Ok(lock) => lock,
                Err(error) => {
                    return recoverable_error_result(
                        self.name(),
                        "catalog_write_failed",
                        error.to_string(),
                        "Retry after the current resource catalog write finishes.",
                    );
                }
            };
            if let Err(message) = self.ensure_safe_catalog_path(&catalog_path) {
                return recoverable_error_result(
                    self.name(),
                    "unsafe_catalog_path",
                    message,
                    "Use a regular YAML file directly under the project .gewu/resources directory.",
                );
            }
            match self.read_catalog(&catalog_path) {
                Ok(catalog) => catalog,
                Err(error) => {
                    return recoverable_error_result(
                        self.name(),
                        "catalog_parse_failed",
                        error.to_string(),
                        "Fix the YAML catalog syntax or choose a different project-local catalog file.",
                    );
                }
            }
        };

        let existing_index = catalog
            .resources
            .iter()
            .position(|entry| entry.uri == resource.uri);
        let exists = registry_exists
            || existing_index.is_some()
            || match self.mutable_provider.contains_uri(&resource.uri) {
                Ok(exists) => exists,
                Err(error) => {
                    return recoverable_error_result(
                        self.name(),
                        "registry_update_failed",
                        error.to_string(),
                        "Retry after ensuring the shared registration provider is healthy.",
                    );
                }
            };

        match request.mode {
            RegisterMode::CreateOnly if exists => {
                return recoverable_error_result(
                    self.name(),
                    "resource_exists",
                    format!("resource already exists: {}", resource.uri),
                    "Use mode:'update_only' or mode:'upsert' to replace the existing resource.",
                );
            }
            RegisterMode::UpdateOnly if !exists => {
                return recoverable_error_result(
                    self.name(),
                    "resource_not_found",
                    format!("resource does not exist: {}", resource.uri),
                    "Use mode:'create_only' or mode:'upsert' to create the resource.",
                );
            }
            _ => {}
        }

        let operation = if exists { "update" } else { "create" };
        let uri = resource.uri.to_string();
        let uri_for_next_actions = uri.clone();
        if request.dry_run {
            return success_result(
                self.name(),
                json!({
                    "dry_run": true,
                    "operation": operation,
                    "uri": uri,
                    "catalog_path": catalog_path.display().to_string(),
                    "registry_updated": false,
                    "available_now": false,
                    "will_update_registry": true,
                    "verify": {"tool":"resource.resolve","args":{"query": uri}},
                    "next_actions": [
                        {"tool":"resource.resolve","args":{"query": uri_for_next_actions},"purpose":"Verify the newly registered resource is available in the current registry."},
                        {"tool":"resource.describe","args":{"uri": uri_for_next_actions},"purpose":"Inspect the authoritative registered resource metadata."}
                    ]
                }),
            );
        }

        let entry = ResourceCatalogEntry::from_resource(resource.clone());
        if let Some(index) = existing_index {
            catalog.resources[index] = entry;
        } else {
            catalog.resources.push(entry);
        }
        catalog
            .resources
            .sort_by(|left, right| left.uri.cmp(&right.uri));

        if let Err(error) = self.write_catalog(&catalog_path, &catalog) {
            return recoverable_error_result(
                self.name(),
                "catalog_write_failed",
                error.to_string(),
                "Check that the project .gewu/resources directory is writable and retry.",
            );
        }

        let catalog_resources = match catalog.clone().into_resources() {
            Ok(resources) => resources,
            Err(error) => {
                return recoverable_error_result(
                    self.name(),
                    "registry_update_failed",
                    error.to_string(),
                    "The catalog was written but could not be converted into registry resources; reload the runtime after fixing the catalog.",
                );
            }
        };

        if let Err(error) = self.mutable_provider.upsert_many(catalog_resources) {
            return recoverable_error_result(
                self.name(),
                "registry_update_failed",
                error.to_string(),
                "The catalog was written but the in-memory registry update failed; reload the runtime or retry.",
            );
        }
        let available_now = self
            .registry
            .resolve(crate::query::ResolveRequest::new(uri.clone()))
            .await
            .map(|response| response.status == crate::query::ResolveStatus::Ok)
            .unwrap_or(false);

        success_result(
            self.name(),
            json!({
                "dry_run": false,
                "operation": operation,
                "uri": uri,
                "catalog_path": catalog_path.display().to_string(),
                "registry_updated": true,
                "available_now": available_now,
                "will_update_registry": true,
                "verify": {"tool":"resource.resolve","args":{"query": uri}},
                "next_actions": [
                    {"tool":"resource.resolve","args":{"query": uri.clone()},"purpose":"Verify the newly registered resource is available in the current registry."},
                    {"tool":"resource.describe","args":{"uri": uri.clone()},"purpose":"Inspect the authoritative registered resource metadata."}
                ]
            }),
        )
    }
}
