use crate::config::Config;
use crate::document::chunker::{ChildChunk, Bm25SparseVector};
use embed_anything::embeddings::embed::{Embedder, EmbeddingResult};
use qdrant_client::qdrant::{
    Datatype, Distance, Fusion, Modifier, NamedVectors, PointStruct, Query, QueryPointsBuilder,
    SparseIndexConfigBuilder, SparseVectorParamsBuilder, SparseVectorBuilder, Value, Vector,
    VectorInput, VectorParamsBuilder,
};
use qdrant_client::Qdrant;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tokio::sync::RwLock;

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum RetrievalError {
    #[error("Qdrant error: {0}")]
    Qdrant(String),
    #[error("EmbedAnything error: {0}")]
    EmbedAnything(String),
    #[error("Collection not found: {0}")]
    CollectionNotFound(String),
    #[error("No results found")]
    NoResults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub content: String,
    pub score: f32,
    pub metadata: HashMap<String, String>,
}

pub struct QdrantStore {
    config: Config,
    client: Qdrant,
    embedding_model: RwLock<Embedder>,
    embedding_dimension: usize,
    sparse_embedder: Option<RwLock<Embedder>>,
}

impl QdrantStore {
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = Qdrant::from_url(&config.qdrant_url);
        if let Some(ref api_key) = config.qdrant_api_key {
            builder = builder.api_key(api_key.as_str());
        }
        let client = builder.build()?;

        let embedding_model = Embedder::from_pretrained_hf(
            "BAAI/bge-base-en-v1.5",
            None,
            None,
            None,
        )?;

        let embedding_dimension = 768;

        let sparse_embedder = if config.enable_sparse {
            Some(RwLock::new(Embedder::from_pretrained_hf(
                &config.sparse_model,
                None,
                None,
                None,
            )?))
        } else {
            None
        };

        Ok(Self {
            config,
            client,
            embedding_model: tokio::sync::RwLock::new(embedding_model),
            embedding_dimension,
            sparse_embedder,
        })
    }

    pub async fn ensure_collection(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use qdrant_client::qdrant::{VectorParams, VectorParamsMap, VectorsConfig};

        let collection_name = &self.config.child_collection;

        let exists = self.client.collection_exists(collection_name).await?;

        if !exists {
            let mut vectors_map: HashMap<String, VectorParams> = HashMap::new();
            vectors_map.insert(
                self.config.dense_vector_name.clone(),
                VectorParamsBuilder::new(self.embedding_dimension as u64, Distance::Cosine).build(),
            );
            let vectors_config = VectorsConfig {
                config: Some(
                    qdrant_client::qdrant::vectors_config::Config::from(VectorParamsMap { map: vectors_map })
                ),
            };

            let mut sparse_map: HashMap<String, qdrant_client::qdrant::SparseVectorParams> = HashMap::new();
            sparse_map.insert(
                self.config.sparse_vector_name.clone(),
                SparseVectorParamsBuilder::default()
                    .modifier(Modifier::Idf)
                    .index(SparseIndexConfigBuilder::default().datatype(Datatype::Float32))
                    .build(),
            );

            let create = qdrant_client::qdrant::CreateCollectionBuilder::new(collection_name)
                .vectors_config(vectors_config)
                .sparse_vectors_config(sparse_map);

            self.client.create_collection(create).await?;
        }

        Ok(())
    }

    pub async fn delete_collection(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let collection_name = &self.config.child_collection;
        let exists = self.client.collection_exists(collection_name).await?;
        if exists {
            self.client.delete_collection(collection_name).await?;
        }
        Ok(())
    }

    fn convert_dense_to_sparse(dense: &[f32], threshold: f32) -> Bm25SparseVector {
        let mut indices: Vec<u64> = Vec::new();
        let mut values: Vec<f32> = Vec::new();

        for (i, &val) in dense.iter().enumerate() {
            if val > threshold {
                indices.push(i as u64);
                values.push(val);
            }
        }

        Bm25SparseVector { indices, values }
    }

    pub async fn compute_sparse_from_text(&self, text: &str) -> Result<Bm25SparseVector, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(ref sparse_embedder) = self.sparse_embedder {
            let raw_results = sparse_embedder.read().await
                .embed(&[text], None, None).await?;
            if let Some(EmbeddingResult::DenseVector(v)) = raw_results.first() {
                let sparse = Self::convert_dense_to_sparse(v, 0.0);
                tracing::debug!("SPLADE sparse: {} non-zero values out of {}", sparse.values.len(), v.len());
                return Ok(sparse);
            }
        }
        Ok(Bm25SparseVector { indices: vec![], values: vec![] })
    }

    pub async fn add_chunks(
        &self,
        chunks: &[ChildChunk],
        sparse_vectors: Option<&[Bm25SparseVector]>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if chunks.is_empty() {
            return Ok(());
        }

        self.ensure_collection().await?;

        let collection_name = &self.config.child_collection;

        let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
        let raw_results = self.embedding_model.read().await
            .embed(&texts, None, None).await?;
        let vectors: Vec<Vec<f32>> = raw_results
            .into_iter()
            .map(|r| match r {
                EmbeddingResult::DenseVector(v) => v,
                EmbeddingResult::MultiVector(vecs) => vecs.into_iter().flatten().collect(),
            })
            .collect();

        let mut points: Vec<PointStruct> = Vec::new();

        for (i, c) in chunks.iter().enumerate() {
            let mut named_vectors = NamedVectors::default()
                .add_vector(&self.config.dense_vector_name, vectors[i].clone());

            if let Some(sparse_vecs) = sparse_vectors {
                if i < sparse_vecs.len() {
                    let sparse = &sparse_vecs[i];
                    if !sparse.indices.is_empty() {
                        let indices_u32: Vec<u32> = sparse.indices.iter().map(|&x| x as u32).collect();
                        let sparse_vector = SparseVectorBuilder::new(indices_u32, sparse.values.clone()).build();
                        named_vectors = named_vectors.add_vector(
                            &self.config.sparse_vector_name,
                            Vector::from(sparse_vector),
                        );
                    }
                }
            }

            let mut payload: HashMap<String, Value> = HashMap::new();
            for (k, v) in &c.metadata {
                payload.insert(k.clone(), v.clone().into());
            }
            payload.insert("parent_id".to_string(), c.parent_id.clone().into());

            points.push(PointStruct::new(i as u64, named_vectors, payload));
        }

        self.client
            .upsert_points(
                qdrant_client::qdrant::UpsertPointsBuilder::new(collection_name, points).wait(true),
            )
            .await?;

        Ok(())
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        score_threshold: f32,
    ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let collection_name = &self.config.child_collection;

        let query_text = "query: ".to_string() + query;
        let raw_results = self.embedding_model.read().await
            .embed(&[query_text.as_str()], None, None).await?;
        let query_vector: Vec<f32> = match &raw_results[0] {
            EmbeddingResult::DenseVector(v) => v.clone(),
            EmbeddingResult::MultiVector(vecs) => vecs.iter().flatten().cloned().collect(),
        };

        let request = QueryPointsBuilder::new(collection_name)
            .limit(limit as u64)
            .query(Query::new_nearest(query_vector.clone()))
            .using(&self.config.dense_vector_name)
            .score_threshold(score_threshold)
            .with_payload(true);

        let search_result = self.client.query(request).await?;

        tracing::debug!("Qdrant search for query '{}' returned {} raw points (threshold: {})",
            query, search_result.result.len(), score_threshold);
        for point in &search_result.result {
            tracing::debug!("  point score: {}", point.score);
        }

        let results: Vec<SearchResult> = search_result
            .result
            .into_iter()
            .filter(|point| point.score >= score_threshold)
            .map(|point| {
                let mut metadata = HashMap::new();
                for (k, v) in &point.payload {
                    if let Some(s) = v.as_str() {
                        metadata.insert(k.clone(), s.to_string());
                    }
                }

                SearchResult {
                    id: format!("{:?}", point.id),
                    content: String::new(),
                    score: point.score,
                    metadata,
                }
            })
            .collect();

        Ok(results)
    }

    pub async fn hybrid_search(
        &self,
        query: &str,
        limit: usize,
        score_threshold: f32,
        sparse_vector: &Bm25SparseVector,
    ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let collection_name = &self.config.child_collection;

        let query_text = "query: ".to_string() + query;
        let raw_results = self.embedding_model.read().await
            .embed(&[query_text.as_str()], None, None).await?;
        let dense_query: Vec<f32> = match &raw_results[0] {
            EmbeddingResult::DenseVector(v) => v.clone(),
            EmbeddingResult::MultiVector(vecs) => vecs.iter().flatten().cloned().collect(),
        };

        let indices_u32: Vec<u32> = sparse_vector.indices.iter().map(|&x| x as u32).collect();
        let sparse_input = VectorInput::new_sparse(indices_u32, sparse_vector.values.clone());

        let request = QueryPointsBuilder::new(collection_name)
            .limit(limit as u64)
            .query(Query::new_fusion(Fusion::Rrf))
            .add_prefetch(
                qdrant_client::qdrant::PrefetchQueryBuilder::default()
                    .using(&self.config.dense_vector_name)
                    .query(Query::new_nearest(dense_query)),
            )
            .add_prefetch(
                qdrant_client::qdrant::PrefetchQueryBuilder::default()
                    .using(&self.config.sparse_vector_name)
                    .query(Query::new_nearest(sparse_input)),
            )
            .score_threshold(score_threshold)
            .with_payload(true);

        let search_result = self.client.query(request).await?;

        let results: Vec<SearchResult> = search_result
            .result
            .into_iter()
            .filter(|point| point.score >= score_threshold)
            .map(|point| {
                let mut metadata = HashMap::new();
                for (k, v) in &point.payload {
                    if let Some(s) = v.as_str() {
                        metadata.insert(k.clone(), s.to_string());
                    }
                }

                SearchResult {
                    id: format!("{:?}", point.id),
                    content: String::new(),
                    score: point.score,
                    metadata,
                }
            })
            .collect();

        Ok(results)
    }
}