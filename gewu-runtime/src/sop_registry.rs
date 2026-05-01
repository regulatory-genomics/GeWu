//! Lightweight SOP registry extracted from workflow manifest metadata.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::manifest::workflow::WorkflowManifest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SopMetadata {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub visible_to: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SopDescriptor {
    pub workflow_id: String,
    pub name: String,
    pub description: String,
    pub visible_to: Vec<String>,
    pub triggers: Vec<String>,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default)]
pub struct SopRegistry {
    sops: HashMap<String, SopDescriptor>,
}

impl SopRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_workflows(workflows: &HashMap<String, WorkflowManifest>) -> Self {
        let mut registry = Self::new();
        for workflow in workflows.values() {
            if let Some(descriptor) = SopDescriptor::from_workflow(workflow) {
                registry.insert(descriptor);
            }
        }
        registry
    }

    pub fn insert(&mut self, descriptor: SopDescriptor) {
        self.sops.insert(descriptor.workflow_id.clone(), descriptor);
    }

    pub fn list(&self) -> Vec<&SopDescriptor> {
        let mut sops = self.sops.values().collect::<Vec<_>>();
        sops.sort_by(|left, right| left.workflow_id.cmp(&right.workflow_id));
        sops
    }

    pub fn visible_to(&self, specialist_id: &str) -> Vec<&SopDescriptor> {
        let mut sops = self
            .sops
            .values()
            .filter(|sop| sop.is_visible_to(specialist_id))
            .collect::<Vec<_>>();
        sops.sort_by(|left, right| left.workflow_id.cmp(&right.workflow_id));
        sops
    }

    pub fn is_empty(&self) -> bool {
        self.sops.is_empty()
    }
}

impl SopDescriptor {
    pub fn from_workflow(workflow: &WorkflowManifest) -> Option<Self> {
        let sop = workflow.metadata.get("sop")?;
        let metadata: SopMetadata = serde_json::from_value(sop.clone()).ok()?;
        Some(Self {
            workflow_id: workflow.id.clone(),
            name: if metadata.name.trim().is_empty() {
                workflow.id.clone()
            } else {
                metadata.name
            },
            description: if metadata.description.trim().is_empty() {
                workflow.description.clone()
            } else {
                metadata.description
            },
            visible_to: metadata.visible_to,
            triggers: metadata.triggers,
            input_schema: metadata.input_schema,
        })
    }

    pub fn is_visible_to(&self, specialist_id: &str) -> bool {
        self.visible_to
            .iter()
            .any(|id| id == "*" || id == specialist_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::workflow::WorkflowManifest;
    use std::collections::BTreeMap;

    #[test]
    fn extracts_sop_metadata_from_workflow() {
        let workflow = WorkflowManifest {
            id: "daily_opencode_rollup".into(),
            description: "Fallback description".into(),
            entry: "start".into(),
            finish: vec!["done".into()],
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            metadata: serde_json::json!({
                "sop": {
                    "name": "Daily OpenCode Rollup",
                    "description": "Summarize a day.",
                    "visible_to": ["general", "build"],
                    "triggers": ["daily rollup"],
                    "input_schema": {"type": "object"}
                }
            }),
        };
        let descriptor = SopDescriptor::from_workflow(&workflow).unwrap();
        assert_eq!(descriptor.workflow_id, "daily_opencode_rollup");
        assert_eq!(descriptor.triggers, vec!["daily rollup"]);
        assert!(descriptor.is_visible_to("general"));
        assert!(!descriptor.is_visible_to("daily-memory-rollup-agent"));
    }

    #[test]
    fn wildcard_visible_to_matches_all_specialists() {
        let descriptor = SopDescriptor {
            workflow_id: "global".into(),
            name: "Global".into(),
            description: String::new(),
            visible_to: vec!["*".into()],
            triggers: Vec::new(),
            input_schema: Value::Null,
        };
        assert!(descriptor.is_visible_to("any-specialist"));
    }
}
