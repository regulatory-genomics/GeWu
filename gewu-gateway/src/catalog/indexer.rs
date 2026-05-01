use std::path::PathBuf;
use std::sync::Arc;

use gewu_runtime::manifest::runtime_plugin_loader::LoadedRuntimePluginManifest;
use gewu_runtime::{NodeSpec, RuntimePluginManifestLoader, SkillManifestLoader};

use crate::catalog::graph::{CatalogGraph, CatalogNode};
use crate::routes::catalog::SkillSummary;
use crate::state::AppState;

pub struct CatalogIndexer {
    state: Arc<AppState>,
}

impl CatalogIndexer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn build(&self) -> CatalogGraph {
        let mut graph = CatalogGraph::new();
        self.add_specialists(&mut graph);
        self.add_tools(&mut graph);
        self.add_skills(&mut graph);
        self.add_workflows(&mut graph);
        self.add_runtime_plugins(&mut graph);
        self.add_jobs(&mut graph).await;
        graph.nodes.sort_by(|left, right| left.id.cmp(&right.id));
        graph.edges.sort_by(|left, right| {
            (
                left.from.as_str(),
                left.to.as_str(),
                left.edge_type.as_str(),
            )
                .cmp(&(
                    right.from.as_str(),
                    right.to.as_str(),
                    right.edge_type.as_str(),
                ))
        });
        graph
    }

    pub fn load_skills(&self) -> Vec<SkillSummary> {
        let mut loader = SkillManifestLoader::with_default_dirs();
        for dir in &self.state.config.skill_dirs {
            loader.add_dir(dir);
        }
        loader
            .load()
            .unwrap_or_default()
            .into_iter()
            .map(|skill| SkillSummary {
                name: skill.name,
                description: skill.description,
                capabilities: skill.capabilities,
                specialists: skill.specialists,
                location: skill.location.display().to_string(),
            })
            .collect()
    }

    pub fn load_runtime_plugins(&self) -> Vec<serde_json::Value> {
        self.loaded_runtime_plugins()
            .into_iter()
            .map(|loaded| {
                serde_json::json!({
                    "id": loaded.manifest.id,
                    "kind": loaded.manifest.kind,
                    "enabled": loaded.manifest.enabled,
                    "baseUrl": loaded.manifest.base_url,
                    "timeoutMs": loaded.manifest.timeout_ms,
                    "sourcePath": loaded.source_path.display().to_string(),
                    "tools": loaded.manifest.tools,
                    "hooks": loaded.manifest.hooks,
                })
            })
            .collect()
    }

    fn add_specialists(&self, graph: &mut CatalogGraph) {
        for specialist in self.state.runtime.list_specialist_descriptors() {
            let specialist_id = format!("specialist:{}", specialist.id);
            graph.add_node(CatalogNode {
                id: specialist_id.clone(),
                node_type: "specialist".to_string(),
                name: specialist.id,
                description: Some(specialist.description),
                source_path: None,
                metadata: serde_json::json!({
                    "capabilities": specialist.capabilities,
                    "skills": specialist.skills,
                }),
            });

            if let Some(capabilities) = graph
                .nodes
                .iter()
                .find(|node| node.id == specialist_id)
                .and_then(|node| node.metadata.get("capabilities"))
                .and_then(|value| value.as_array())
                .cloned()
            {
                for capability in capabilities.iter().filter_map(|value| value.as_str()) {
                    graph.add_edge(
                        specialist_id.clone(),
                        format!("tool:{capability}"),
                        "can_use",
                    );
                }
            }
        }
    }

    fn add_tools(&self, graph: &mut CatalogGraph) {
        for tool in self.state.runtime.capability_registry().list_capabilities() {
            graph.add_node(CatalogNode {
                id: format!("tool:{}", tool.name),
                node_type: "tool".to_string(),
                name: tool.name,
                description: Some(tool.description),
                source_path: None,
                metadata: serde_json::json!({
                    "provider_kind": tool.provider_kind,
                    "input_schema": tool.input_schema,
                    "metadata": tool.metadata,
                }),
            });
        }
    }

    fn add_skills(&self, graph: &mut CatalogGraph) {
        for skill in self.load_skills() {
            let skill_id = format!("skill:{}", skill.name);
            graph.add_node(CatalogNode {
                id: skill_id.clone(),
                node_type: "skill".to_string(),
                name: skill.name,
                description: Some(skill.description),
                source_path: Some(skill.location),
                metadata: serde_json::json!({
                    "capabilities": skill.capabilities,
                    "specialists": skill.specialists,
                }),
            });
            if let Some(specialists) = graph
                .nodes
                .iter()
                .find(|node| node.id == skill_id)
                .and_then(|node| node.metadata.get("specialists"))
                .and_then(|value| value.as_array())
                .cloned()
            {
                for specialist in specialists.iter().filter_map(|value| value.as_str()) {
                    graph.add_edge(
                        format!("specialist:{specialist}"),
                        skill_id.clone(),
                        "attached_skill",
                    );
                }
            }
        }
    }

    fn add_workflows(&self, graph: &mut CatalogGraph) {
        let Ok(registry) = self.state.workflow_registry() else {
            return;
        };
        for workflow in registry.as_map().values() {
            let workflow_id = format!("workflow:{}", workflow.id);
            graph.add_node(CatalogNode {
                id: workflow_id.clone(),
                node_type: "workflow".to_string(),
                name: workflow.id.clone(),
                description: Some(workflow.description.clone()),
                source_path: None,
                metadata: serde_json::json!({
                    "entry": workflow.entry,
                    "finish": workflow.finish,
                    "metadata": workflow.metadata,
                }),
            });
            if workflow.metadata.get("sop").is_some() {
                let sop_id = format!("sop:{}", workflow.id);
                graph.add_node(CatalogNode {
                    id: sop_id.clone(),
                    node_type: "sop".to_string(),
                    name: workflow.id.clone(),
                    description: Some(workflow.description.clone()),
                    source_path: None,
                    metadata: workflow.metadata.get("sop").cloned().unwrap_or_default(),
                });
                graph.add_edge(sop_id, workflow_id.clone(), "describes");
            }
            for node in workflow.nodes.values() {
                match node {
                    NodeSpec::Specialist {
                        specialist, tool, ..
                    } => {
                        graph.add_edge(
                            workflow_id.clone(),
                            format!("specialist:{}", specialist.id()),
                            "calls",
                        );
                        if let Some(tool) = tool {
                            graph.add_edge(workflow_id.clone(), format!("tool:{tool}"), "calls");
                        }
                    }
                    NodeSpec::Tool { tool, .. } => {
                        graph.add_edge(workflow_id.clone(), format!("tool:{tool}"), "calls");
                    }
                    NodeSpec::Workflow { workflow, .. } => {
                        graph.add_edge(
                            workflow_id.clone(),
                            format!("workflow:{workflow}"),
                            "subworkflow",
                        );
                    }
                    NodeSpec::Parallel { branches } => {
                        for branch in branches.values() {
                            if let NodeSpec::Tool { tool, .. } = branch {
                                graph.add_edge(
                                    workflow_id.clone(),
                                    format!("tool:{tool}"),
                                    "calls",
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn add_runtime_plugins(&self, graph: &mut CatalogGraph) {
        for loaded in self.loaded_runtime_plugins() {
            let plugin_id = format!("runtime_plugin:{}", loaded.manifest.id);
            graph.add_node(CatalogNode {
                id: plugin_id.clone(),
                node_type: "runtime_plugin".to_string(),
                name: loaded.manifest.id.clone(),
                description: None,
                source_path: Some(loaded.source_path.display().to_string()),
                metadata: serde_json::json!({ "base_url": loaded.manifest.base_url }),
            });
            for tool in loaded.manifest.tools {
                graph.add_edge(plugin_id.clone(), format!("tool:{}", tool.name), "provides");
            }
        }
    }

    fn loaded_runtime_plugins(&self) -> Vec<LoadedRuntimePluginManifest> {
        let mut loader = RuntimePluginManifestLoader::with_default_dirs();
        for dir in &self.state.config.runtime_plugin_dirs {
            loader.add_dir(dir);
        }
        loader.load().unwrap_or_default()
    }

    async fn add_jobs(&self, graph: &mut CatalogGraph) {
        let jobs = self.state.jobs.list_jobs().await.unwrap_or_default();
        for job in jobs {
            let job_id = format!("job:{}", job.id);
            graph.add_node(CatalogNode {
                id: job_id.clone(),
                node_type: "job".to_string(),
                name: job.name,
                description: Some(job.schedule),
                source_path: None,
                metadata: serde_json::json!({
                    "enabled": job.enabled,
                    "timezone": job.timezone,
                    "next_run_at": job.next_run_at,
                }),
            });
            graph.add_edge(
                job_id,
                format!("specialist:{}", job.specialist_id),
                "scheduled_for",
            );
        }
    }
}

#[allow(dead_code)]
fn home_gewu_path(child: &str) -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".gewu").join(child))
        .unwrap_or_else(|| PathBuf::from(".gewu").join(child))
}
