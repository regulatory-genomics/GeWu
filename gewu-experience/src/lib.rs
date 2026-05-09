//! GeWu Experience - Experience memory with semantic recall for AI agents.
//!
//! This crate provides the Experience system for storing and retrieving
//! problem-solving experiences. When an agent successfully solves a task,
//! the experience is stored with embeddings for semantic search.
//!
//! # Architecture
//!
//! - [`ExperienceStore`] - Main store combining SQLite and Qdrant
//! - [`Experience`] - A stored experience record
//! - [`ExperienceQuery`] - Query parameters for recall
//! - [`RecalledExperience`] - A recalled experience with similarity score
//!
//! # Usage
//!
//! ```ignore
//! use gewu_experience::{ExperienceStore, Config};
//!
//! let config = Config::from_env();
//! let store = ExperienceStore::new(&config).await?;
//!
//! // Store a successful experience
//! let experience = Experience::new(...);
//! store.store(&experience).await?;
//!
//! // Recall similar experiences
//! let query = ExperienceQuery {
//!     query_text: "fixing memory leak".to_string(),
//!     ..Default::default()
//! };
//! let results = store.recall(&query).await?;
//! ```

pub mod config;
pub mod tools;
pub mod types;

mod store;

pub use config::Config;
pub use store::ExperienceStore;
pub use tools::{ExperienceRecallTool, ExperienceStoreTool};
pub use types::{Experience, ExperienceQuery, RecalledExperience, StoreResult};
