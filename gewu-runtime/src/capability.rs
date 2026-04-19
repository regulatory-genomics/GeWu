//! Capability registry and provider adapters.
//!
//! The `CapabilityRegistry` is the runtime-wide index of capability name →
//! (`ToolProvider`, `CapabilityDescriptor`). It powers manifest-driven
//! specialist assembly while remaining interoperable with the older
//! `ToolFactory`-based registration flow via `ToolFactoryProvider`.

use anyhow::{Result, anyhow};
use gewu_core::traits::{AssemblyContext, Tool, ToolFactory, ToolProvider};
use gewu_core::types::CapabilityDescriptor;
use std::collections::HashMap;
use std::sync::Arc;

/// Adapter that exposes a legacy `ToolFactory` as a `ToolProvider`.
///
/// Capability descriptors are synthesized from the factory's `capabilities()`
/// list with a caller-provided `kind` tag.
pub struct ToolFactoryProvider {
    kind: String,
    id: String,
    factory: Box<dyn ToolFactory>,
}

impl ToolFactoryProvider {
    pub fn new(
        kind: impl Into<String>,
        id: impl Into<String>,
        factory: Box<dyn ToolFactory>,
    ) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
            factory,
        }
    }

    pub fn builtin(factory: Box<dyn ToolFactory>) -> Self {
        Self::new("builtin", "builtin", factory)
    }

    pub fn plugin(name: impl Into<String>, factory: Box<dyn ToolFactory>) -> Self {
        let id = name.into();
        Self::new("plugin", id, factory)
    }
}

impl ToolProvider for ToolFactoryProvider {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.factory
            .capabilities()
            .into_iter()
            .map(|name| CapabilityDescriptor::new(name, self.kind.clone()))
            .collect()
    }

    fn build_tools(
        &self,
        capability: &str,
        ctx: &AssemblyContext,
    ) -> std::result::Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        self.factory.create_tools(capability, ctx)
    }
}

/// Runtime-wide registry of capabilities.
///
/// Populated during `GeWuRuntimeBuilder::build()` from builtin providers,
/// plugin providers, and (optionally) config-driven providers. Used by the
/// manifest loader to validate specialist capability references and by the
/// runtime to resolve tools at assembly time.
#[derive(Default)]
pub struct CapabilityRegistry {
    providers: Vec<Arc<dyn ToolProvider>>,
    /// capability name → (provider index in `providers`, descriptor)
    index: HashMap<String, (usize, CapabilityDescriptor)>,
    /// name collisions observed during registration (kept for diagnostics).
    collisions: Vec<String>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider. First registration of a capability name wins;
    /// subsequent duplicates are recorded in `collisions()` and ignored.
    pub fn register(&mut self, provider: Arc<dyn ToolProvider>) {
        let idx = self.providers.len();
        let descriptors = provider.capabilities();
        self.providers.push(provider);

        for desc in descriptors {
            if self.index.contains_key(&desc.name) {
                let prev_kind = self
                    .index
                    .get(&desc.name)
                    .map(|(i, _)| self.providers[*i].kind())
                    .unwrap_or("?");
                tracing::warn!(
                    "Capability name collision: '{}' already registered by provider kind '{}'; \
                     new provider kind '{}' ignored",
                    desc.name,
                    prev_kind,
                    self.providers[idx].kind()
                );
                self.collisions.push(desc.name);
                continue;
            }
            self.index.insert(desc.name.clone(), (idx, desc));
        }
    }

    pub fn contains(&self, capability: &str) -> bool {
        self.index.contains_key(capability)
    }

    pub fn descriptor(&self, capability: &str) -> Option<&CapabilityDescriptor> {
        self.index.get(capability).map(|(_, d)| d)
    }

    pub fn list_capabilities(&self) -> Vec<CapabilityDescriptor> {
        let mut out: Vec<CapabilityDescriptor> = self.index.values().map(|(_, d)| d.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn collisions(&self) -> &[String] {
        &self.collisions
    }

    /// Resolve a list of capabilities into concrete tool instances.
    pub fn resolve(
        &self,
        capabilities: &[String],
        ctx: &AssemblyContext,
    ) -> Result<Vec<Box<dyn Tool>>> {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        for cap in capabilities {
            let (idx, _) = self
                .index
                .get(cap)
                .ok_or_else(|| anyhow!("No provider registered for capability: {}", cap))?;
            let provider = &self.providers[*idx];
            let produced = provider
                .build_tools(cap, ctx)
                .map_err(|e| anyhow!("Provider '{}' failed for '{}': {}", provider.id(), cap, e))?;
            tools.extend(produced);
        }
        Ok(tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::traits::{AssemblyContext, Tool, ToolFactory};
    use gewu_core::types::ToolResult;

    struct DummyTool(String);

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn schema(&self) -> schemars::schema::RootSchema {
            schemars::schema::RootSchema::default()
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &gewu_core::types::ToolContext,
        ) -> std::result::Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(ToolResult::new(self.0.clone(), ""))
        }
    }

    struct DummyFactory;
    impl ToolFactory for DummyFactory {
        fn capabilities(&self) -> Vec<String> {
            vec!["alpha".into(), "beta".into()]
        }
        fn create_tools(
            &self,
            capability: &str,
            _ctx: &AssemblyContext,
        ) -> std::result::Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>>
        {
            Ok(vec![Box::new(DummyTool(capability.to_string()))])
        }
    }

    #[test]
    fn registry_lists_and_resolves_capabilities() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Arc::new(ToolFactoryProvider::new(
            "builtin",
            "dummy",
            Box::new(DummyFactory),
        )));
        assert!(reg.contains("alpha"));
        assert!(reg.contains("beta"));
        let names: Vec<String> = reg.list_capabilities().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn registry_records_collisions() {
        let mut reg = CapabilityRegistry::new();
        reg.register(Arc::new(ToolFactoryProvider::new(
            "builtin",
            "one",
            Box::new(DummyFactory),
        )));
        reg.register(Arc::new(ToolFactoryProvider::new(
            "plugin",
            "two",
            Box::new(DummyFactory),
        )));
        assert_eq!(reg.collisions().len(), 2);
    }
}
