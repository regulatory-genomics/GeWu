//! Configuration for the Experience store.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub qdrant_url: String,
    pub qdrant_api_key: Option<String>,
    pub collection_name: String,
    pub db_path: PathBuf,
    pub embedding_model: String,
    pub default_score_threshold: f32,
    pub embedding_dimension: usize,
}

impl Config {
    pub fn from_env() -> Self {
        let qdrant_url = std::env::var("QDRANT_URL")
            .unwrap_or_else(|_| "http://localhost:6334".to_string());

        let qdrant_api_key = std::env::var("QDRANT_API_KEY").ok();

        let collection_name = std::env::var("EXPERIENCE_COLLECTION")
            .unwrap_or_else(|_| "gewu_experiences".to_string());

        let db_path = std::env::var("EXPERIENCE_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                if let Some(proj_dirs) = ProjectDirs::from("com", "gewu", "GeWu") {
                    proj_dirs.data_dir().join("experiences.db")
                } else {
                    PathBuf::from("~/.gewu/experiences.db")
                }
            });

        let embedding_model = std::env::var("EXPERIENCE_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "BAAI/bge-base-en-v1.5".to_string());

        let default_score_threshold = std::env::var("EXPERIENCE_SCORE_THRESHOLD")
            .unwrap_or_else(|_| "0.7".to_string())
            .parse::<f32>()
            .unwrap_or(0.7);

        Self {
            qdrant_url,
            qdrant_api_key,
            collection_name,
            db_path,
            embedding_model,
            default_score_threshold,
            embedding_dimension: 768,
        }
    }

    pub fn with_qdrant(url: impl Into<String>, api_key: Option<String>) -> Self {
        let db_path = if let Some(proj_dirs) = ProjectDirs::from("com", "gewu", "GeWu") {
            proj_dirs.data_dir().join("experiences.db")
        } else if let Some(home) = dirs::home_dir() {
            home.join(".gewu").join("experiences.db")
        } else {
            PathBuf::from("/tmp/.gewu_experiences.db")
        };

        Self {
            qdrant_url: url.into(),
            qdrant_api_key: api_key,
            collection_name: "gewu_experiences".to_string(),
            db_path,
            embedding_model: "BAAI/bge-base-en-v1.5".to_string(),
            default_score_threshold: 0.7,
            embedding_dimension: 768,
        }
    }

    pub fn qdrant_url(&self) -> &str {
        &self.qdrant_url
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    pub fn from_file(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut stripped = content.clone();
        json_strip_comments::strip(&mut stripped)?;

        let json: serde_json::Value = serde_json::from_str(&stripped)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        let qdrant_url = json["qdrant_url"]
            .as_str()
            .unwrap_or("http://localhost:6334")
            .to_string();

        let qdrant_api_key = json["qdrant_api_key"].as_str().map(|s| s.to_string());

        let db_path = if let Some(proj_dirs) = ProjectDirs::from("com", "gewu", "GeWu") {
            proj_dirs.data_dir().join("experiences.db")
        } else if let Some(home) = dirs::home_dir() {
            home.join(".gewu").join("experiences.db")
        } else {
            PathBuf::from("/tmp/.gewu_experiences.db")
        };

        Ok(Config {
            qdrant_url,
            qdrant_api_key,
            collection_name: "test_experiences".to_string(),
            db_path,
            embedding_model: "BAAI/bge-base-en-v1.5".to_string(),
            default_score_threshold: 0.7,
            embedding_dimension: 768,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env()
    }
}
