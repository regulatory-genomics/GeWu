use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub docs_dir: PathBuf,
    pub parent_store_path: PathBuf,
    pub qdrant_url: String,
    pub qdrant_api_key: Option<String>,
    pub child_collection: String,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub min_parent_size: usize,
    pub max_parent_size: usize,
    pub max_iterations: usize,
    pub max_tool_calls: usize,
    pub base_token_threshold: usize,
    pub token_growth_factor: f64,
    pub retrieval_limit: usize,
    pub similarity_threshold: f64,
    pub enable_sparse: bool,
    pub sparse_model: String,
    pub sparse_vector_name: String,
    pub dense_vector_name: String,
    pub avg_doc_length: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            docs_dir: PathBuf::from("docs"),
            parent_store_path: PathBuf::from("parent_store"),
            qdrant_url: "http://localhost:6334".to_string(),
            qdrant_api_key: None,
            child_collection: "document_child_chunks".to_string(),
            chunk_size: 500,
            chunk_overlap: 100,
            min_parent_size: 2000,
            max_parent_size: 4000,
            max_iterations: 10,
            max_tool_calls: 8,
            base_token_threshold: 40000,
            token_growth_factor: 0.9,
            retrieval_limit: 5,
            similarity_threshold: 0.7,
            enable_sparse: false,
            sparse_model: "prithivida/Splade_PP_en_v1".to_string(),
            sparse_vector_name: "sparse".to_string(),
            dense_vector_name: "dense".to_string(),
            avg_doc_length: 100,
        }
    }
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }
}
