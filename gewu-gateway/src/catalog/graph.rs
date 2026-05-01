use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogGraph {
    pub nodes: Vec<CatalogNode>,
    pub edges: Vec<CatalogEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEdge {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub edge_type: String,
}

impl CatalogGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn add_node(&mut self, node: CatalogNode) {
        if !self.nodes.iter().any(|existing| existing.id == node.id) {
            self.nodes.push(node);
        }
    }

    pub fn add_edge(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        edge_type: impl Into<String>,
    ) {
        self.edges.push(CatalogEdge {
            from: from.into(),
            to: to.into(),
            edge_type: edge_type.into(),
        });
    }
}

impl Default for CatalogGraph {
    fn default() -> Self {
        Self::new()
    }
}
