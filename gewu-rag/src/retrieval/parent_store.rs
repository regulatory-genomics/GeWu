use crate::document::chunker::ParentChunk;
use std::collections::HashMap;
use std::fs;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ParentStoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Parent not found: {0}")]
    NotFound(String),
}

pub struct ParentStore {
    base_path: std::path::PathBuf,
}

impl ParentStore {
    pub fn new(base_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    pub fn init(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.base_path.exists() {
            fs::create_dir_all(&self.base_path)?;
        }
        Ok(())
    }

    pub fn save(
        &self,
        parent: &ParentChunk,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.init()?;

        let file_name = format!("{}.json", parent.id);
        let file_path = self.base_path.join(&file_name);

        let doc_dict = serde_json::json!({
            "page_content": parent.content,
            "metadata": parent.metadata
        });

        let json_string = serde_json::to_string_pretty(&doc_dict)?;
        fs::write(&file_path, json_string)?;

        Ok(())
    }

    pub fn load(&self, parent_id: &str) -> Result<ParentChunk, ParentStoreError> {
        let file_name = if parent_id.to_lowercase().ends_with(".json") {
            parent_id.to_string()
        } else {
            format!("{}.json", parent_id)
        };

        let file_path = self.base_path.join(&file_name);

        if !file_path.exists() {
            return Err(ParentStoreError::NotFound(parent_id.to_string()));
        }

        let content = fs::read_to_string(&file_path)?;
        let data: serde_json::Value = serde_json::from_str(&content)?;

        let page_content = data["page_content"]
            .as_str()
            .ok_or_else(|| ParentStoreError::NotFound(parent_id.to_string()))?
            .to_string();

        let metadata: HashMap<String, String> = data["metadata"]
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        Ok(ParentChunk {
            id: parent_id.to_string(),
            content: page_content,
            metadata,
        })
    }

    #[allow(dead_code)]
    pub fn delete(&self, parent_id: &str) -> Result<(), ParentStoreError> {
        let file_name = if parent_id.to_lowercase().ends_with(".json") {
            parent_id.to_string()
        } else {
            format!("{}.json", parent_id)
        };

        let file_path = self.base_path.join(&file_name);

        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn list_all(&self) -> Result<Vec<String>, ParentStoreError> {
        if !self.base_path.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.base_path)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "json")
                    .unwrap_or(false)
            })
            .map(|e| e.path().file_stem().unwrap().to_string_lossy().to_string())
            .collect();

        Ok(entries)
    }

    #[allow(dead_code)]
    pub fn clear(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.base_path.exists() {
            for entry in fs::read_dir(&self.base_path)? {
                let entry = entry?;
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "json")
                    .unwrap_or(false)
                {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parent_store_save_load() {
        let temp_dir = std::env::temp_dir();
        let store = ParentStore::new(temp_dir.join("test_parent_store"));

        let parent = ParentChunk {
            id: "test_parent_0".to_string(),
            content: "Test content".to_string(),
            metadata: HashMap::from([
                ("source".to_string(), "test.pdf".to_string()),
                ("parent_id".to_string(), "test_parent_0".to_string()),
            ]),
        };

        store.save(&parent).unwrap();
        let loaded = store.load("test_parent_0").unwrap();

        assert_eq!(loaded.content, parent.content);
        assert_eq!(loaded.metadata.get("source"), Some(&"test.pdf".to_string()));

        store.delete("test_parent_0").unwrap();
    }
}
