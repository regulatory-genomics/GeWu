use crate::config::Config;
use crate::retrieval::qdrant_store::{QdrantStore, SearchResult};
use crate::retrieval::parent_store::ParentStore;
use async_trait::async_trait;
use gewu_core::types::{ToolContext, ToolResult};
use gewu_core::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum RetrievalToolError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("No relevant chunks found")]
    #[allow(dead_code)]
    NoResults,
    #[error("Parent not found: {0}")]
    ParentNotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchChildChunksInput {
    pub query: String,
    pub limit: Option<usize>,
}

impl Default for SearchChildChunksInput {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: Some(5),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RetrieveParentChunksInput {
    pub parent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RetrieveManyParentChunksInput {
    pub parent_ids: Vec<String>,
}

pub struct SearchChildChunksTool {
    store: Arc<QdrantStore>,
    parent_store: Arc<ParentStore>,
    config: Config,
}

impl SearchChildChunksTool {
    pub fn new(store: Arc<QdrantStore>, parent_store: Arc<ParentStore>, config: Config) -> Self {
        Self {
            store,
            parent_store,
            config,
        }
    }
}

#[async_trait]
impl Tool for SearchChildChunksTool {
    fn name(&self) -> &str {
        "search_child_chunks"
    }

    fn description(&self) -> &str {
        "Search for the top K most relevant child chunks from the document knowledge base.
        
        Args:
            query: Search query string
            limit: Maximum number of results to return (default: 5)
            
        Returns:
            A formatted string containing the search results with parent IDs, file names, and content."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(SearchChildChunksInput)
    }

    async fn init(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: SearchChildChunksInput = serde_json::from_value(args)?;
        let limit = input.limit.unwrap_or(self.config.retrieval_limit);
        let query = input.query.trim();

        if query.is_empty() {
            return Ok(ToolResult::new("Search Results", "NO_RELEVANT_CHUNKS"));
        }

        tracing::debug!("search_child_chunks executing: query='{}', limit={}, threshold={}",
            query, limit, self.config.similarity_threshold as f32);

        let search_result = if self.config.enable_sparse {
            let sparse_vec = self.store.compute_sparse_from_text(query).await?;
            self.store
                .hybrid_search(query, limit, self.config.similarity_threshold as f32, &sparse_vec)
                .await
        } else {
            self.store
                .search(query, limit, self.config.similarity_threshold as f32)
                .await
        };

        let results = search_result.map_err(|e| {
            let err_str = e.to_string();
            Box::new(RetrievalToolError::Store(err_str)) as Box<dyn std::error::Error + Send + Sync>
        })?;

        if results.is_empty() {
            tracing::debug!("search returned no results for query: {}", query);
            return Ok(ToolResult::new("Search Results", "NO_RELEVANT_CHUNKS"));
        }

        tracing::debug!("search returned {} results for query: {}", results.len(), query);
        let formatted = format_search_results_with_parent(&self.parent_store, &results);

        Ok(ToolResult::new("Search Results", formatted))
    }
}

pub struct RetrieveParentChunksTool {
    store: Arc<ParentStore>,
}

impl RetrieveParentChunksTool {
    pub fn new(store: Arc<ParentStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RetrieveParentChunksTool {
    fn name(&self) -> &str {
        "retrieve_parent_chunks"
    }

    fn description(&self) -> &str {
        "Retrieve full parent chunks by their IDs.
        
        Args:
            parent_id: Parent chunk ID to retrieve
            
        Returns:
            The complete content of the parent chunk including source file information."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(RetrieveParentChunksInput)
    }

    async fn init(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: RetrieveParentChunksInput = serde_json::from_value(args)?;
        let parent_id = input.parent_id.trim();

        if parent_id.is_empty() {
            return Err(Box::new(RetrievalToolError::ParentNotFound("Empty parent_id".to_string())));
        }

        let load_result = self.store.load(parent_id);
        
        let parent = load_result.map_err(|e| {
            let err_str = e.to_string();
            Box::new(RetrievalToolError::Store(err_str)) as Box<dyn std::error::Error + Send + Sync>
        })?;

        let formatted = format_parent_single(&parent.id, &parent.metadata, &parent.content);

        Ok(ToolResult::new("Parent Chunk Retrieved", formatted))
    }
}

pub struct RetrieveManyParentChunksTool {
    store: Arc<ParentStore>,
}

impl RetrieveManyParentChunksTool {
    pub fn new(store: Arc<ParentStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RetrieveManyParentChunksTool {
    fn name(&self) -> &str {
        "retrieve_many_parent_chunks"
    }

    fn description(&self) -> &str {
        "Retrieve multiple full parent chunks by their IDs in batch.
        
        Args:
            parent_ids: List of parent chunk IDs to retrieve
            
        Returns:
            The complete content of all parent chunks including source file information."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(RetrieveManyParentChunksInput)
    }

    async fn init(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: RetrieveManyParentChunksInput = serde_json::from_value(args)?;
        
        if input.parent_ids.is_empty() {
            return Err(Box::new(RetrievalToolError::ParentNotFound("Empty parent_ids list".to_string())));
        }

        let mut results = Vec::new();
        let mut errors = Vec::new();

        for parent_id in &input.parent_ids {
            match self.store.load(parent_id) {
                Ok(parent) => {
                    results.push(format_parent_single(&parent.id, &parent.metadata, &parent.content));
                }
                Err(e) => {
                    errors.push(format!("Failed to load {}: {}", parent_id, e));
                }
            }
        }

        let output = if results.is_empty() {
            format!("All parent retrievals failed:\n{}", errors.join("\n"))
        } else {
            let mut output = results.join("\n\n");
            if !errors.is_empty() {
                output.push_str("\n\nErrors:\n");
                output.push_str(&errors.join("\n"));
            }
            output
        };

        Ok(ToolResult::new("Parent Chunks Retrieved", output))
    }
}

fn format_parent_single(id: &str, metadata: &std::collections::HashMap<String, String>, content: &str) -> String {
    format!(
        "Parent ID: {}\nFile Name: {}\nContent: {}",
        id,
        metadata.get("source").cloned().unwrap_or_else(|| "unknown".to_string()),
        content.trim()
    )
}

fn format_search_results_with_parent(parent_store: &ParentStore, results: &[SearchResult]) -> String {
    let mut formatted_parts = Vec::new();

    for r in results {
        let parent_id = r.metadata.get("parent_id").cloned().unwrap_or_default();
        if parent_id.is_empty() {
            tracing::warn!("Search result missing parent_id: id={}, score={}", r.id, r.score);
            continue;
        }
        match parent_store.load(&parent_id) {
            Ok(parent) => {
                formatted_parts.push(format!(
                    "Parent ID: {}\nFile Name: {}\nContent: {}",
                    parent.id,
                    parent.metadata.get("source").cloned().unwrap_or_else(|| "unknown".to_string()),
                    parent.content.trim()
                ));
            }
            Err(e) => {
                tracing::warn!("Failed to load parent {}: {}", parent_id, e);
            }
        }
    }

    formatted_parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_child_chunks_input_default() {
        let input = SearchChildChunksInput::default();
        assert_eq!(input.limit, Some(5));
        assert!(input.query.is_empty());
    }

    #[test]
    fn test_search_child_chunks_input_custom() {
        let input = SearchChildChunksInput {
            query: "test query".to_string(),
            limit: Some(10),
        };
        assert_eq!(input.query, "test query");
        assert_eq!(input.limit, Some(10));
    }

    #[test]
    fn test_retrieve_parent_chunks_input() {
        let input = RetrieveParentChunksInput {
            parent_id: "parent_123".to_string(),
        };
        assert_eq!(input.parent_id, "parent_123");
    }

    #[test]
    fn test_retrieve_many_parent_chunks_input() {
        let input = RetrieveManyParentChunksInput {
            parent_ids: vec!["parent_1".to_string(), "parent_2".to_string()],
        };
        assert_eq!(input.parent_ids.len(), 2);
    }
}
