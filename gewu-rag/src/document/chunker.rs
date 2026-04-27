use crate::config::Config;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ParentChunk {
    pub id: String,
    pub content: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ChildChunk {
    #[allow(dead_code)]
    pub id: String,
    pub content: String,
    pub metadata: HashMap<String, String>,
    pub parent_id: String,
}

pub struct DocumentChunker {
    config: Config,
    bm25: BM25,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bm25SparseVector {
    pub indices: Vec<u64>,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct BM25 {
    #[allow(dead_code)]
    k1: f32,
    #[allow(dead_code)]
    b: f32,
    idf: HashMap<String, f32>,
    avg_doc_length: f32,
    vocab: HashMap<String, u64>,
}

impl BM25 {
    pub fn new(k1: f32, b: f32, avg_doc_length: f32) -> Self {
        Self {
            k1,
            b,
            idf: HashMap::new(),
            avg_doc_length,
            vocab: HashMap::new(),
        }
    }

    pub fn compute_idf(&mut self, documents: &[Vec<String>]) {
        let num_docs = documents.len() as f32;
        let mut doc_freq: HashMap<String, f32> = HashMap::new();

        for doc in documents {
            let mut seen = std::collections::HashSet::new();
            for term in doc {
                if seen.insert(term.clone()) {
                    *doc_freq.entry(term.clone()).or_insert(0.0) += 1.0;
                }
            }
        }

        for (term, df) in doc_freq {
            let idf = (num_docs - df + 0.5) / (df + 0.5);
            let idf = idf.max(0.0).ln() + 1.0;
            self.idf.insert(term, idf);
        }
    }
}

impl DocumentChunker {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            bm25: BM25::new(1.5, 0.75, 100.0),
        }
    }

    pub fn tokenize(&self, text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    pub fn set_avg_doc_length(&mut self, avg_len: f32) {
        self.bm25.avg_doc_length = avg_len;
    }

    pub fn build_idf(&mut self, documents: &[String]) {
        let tokenized: Vec<Vec<String>> = documents.iter().map(|d| self.tokenize(d)).collect();
        self.bm25.compute_idf(&tokenized);

        let mut vocab: HashMap<String, u64> = HashMap::new();
        for doc in &tokenized {
            for term in doc {
                if !vocab.contains_key(term) {
                    let idx = vocab.len() as u64;
                    vocab.insert(term.clone(), idx);
                }
            }
        }
        self.bm25.vocab = vocab;
    }

    pub fn split_by_headers(&self, text: &str) -> Vec<String> {
        let header_pattern = Regex::new(r"^(#{1,3})\s+(.+)$").unwrap();
        let mut sections = Vec::new();
        let mut current_section = String::new();

        for line in text.lines() {
            if let Some(captures) = header_pattern.captures(line) {
                let level = captures.get(1).map(|m| m.as_str().len()).unwrap_or(0);

                if level > 0 && !current_section.is_empty() {
                    sections.push(current_section.trim().to_string());
                    current_section = String::new();
                }
                current_section.push_str(line);
                current_section.push('\n');
            } else {
                current_section.push_str(line);
                current_section.push('\n');
            }
        }

        if !current_section.trim().is_empty() {
            sections.push(current_section.trim().to_string());
        }

        sections
    }

    pub fn split_by_chars(&self, text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
        let mut chunks = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let total_len = chars.len();

        if total_len == 0 {
            return chunks;
        }

        let mut start = 0;
        while start < total_len {
            let end = (start + chunk_size).min(total_len);
            let chunk: String = chars[start..end].iter().collect();

            if !chunk.trim().is_empty() {
                chunks.push(chunk);
            }

            if end >= total_len {
                break;
            }
            start = end - overlap.min(chunk_size);
        }

        chunks
    }

    pub fn merge_small_parents(&self, chunks: &[String], min_size: usize) -> Vec<String> {
        if chunks.is_empty() {
            return Vec::new();
        }

        let mut merged = Vec::new();
        let mut current = chunks[0].clone();

        for chunk in chunks.iter().skip(1) {
            if current.len() < min_size {
                current.push_str("\n\n");
                current.push_str(chunk);
            } else {
                merged.push(current);
                current = chunk.clone();
            }
        }

        if !current.is_empty() {
            if let Some(last) = merged.last_mut() {
                last.push_str("\n\n");
                last.push_str(&current);
            } else {
                merged.push(current);
            }
        }

        merged
    }

    pub fn split_large_parents(&self, chunks: &[String], max_size: usize) -> Vec<String> {
        let mut result = Vec::new();

        for chunk in chunks {
            if chunk.len() <= max_size {
                result.push(chunk.clone());
            } else {
                let sub_chunks = self.split_by_chars(chunk, max_size, self.config.chunk_overlap);
                result.extend(sub_chunks);
            }
        }

        result
    }

    pub fn clean_small_chunks(&self, chunks: &[String], min_size: usize) -> Vec<String> {
        if chunks.is_empty() {
            return Vec::new();
        }

        let mut cleaned: Vec<String> = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            if chunk.len() < min_size {
                if let Some(last) = cleaned.last_mut() {
                    last.push_str("\n\n");
                    last.push_str(chunk);
                } else if i < chunks.len() - 1 {
                    // Skip merging with next for now to avoid borrowing issues
                    cleaned.push(chunk.clone());
                } else {
                    cleaned.push(chunk.clone());
                }
            } else {
                cleaned.push(chunk.clone());
            }
        }

        cleaned
    }

    pub fn create_parent_chunks(&self, text: &str, source_name: &str) -> Vec<ParentChunk> {
        let header_sections = self.split_by_headers(text);
        let merged = self.merge_small_parents(&header_sections, self.config.min_parent_size);
        let split = self.split_large_parents(&merged, self.config.max_parent_size);
        let cleaned = self.clean_small_chunks(&split, self.config.min_parent_size);

        cleaned
            .into_iter()
            .enumerate()
            .map(|(i, content)| {
                let mut metadata = HashMap::new();
                metadata.insert("source".to_string(), source_name.to_string());
                let parent_id = format!("{}_parent_{}", source_name, i);
                metadata.insert("parent_id".to_string(), parent_id.clone());

                ParentChunk {
                    id: parent_id,
                    content,
                    metadata,
                }
            })
            .collect()
    }

    pub fn create_child_chunks(&self, parent: &ParentChunk, doc_stem: &str) -> Vec<ChildChunk> {
        let chunks = self.split_by_chars(
            &parent.content,
            self.config.chunk_size,
            self.config.chunk_overlap,
        );

        chunks
            .into_iter()
            .enumerate()
            .map(|(i, content)| {
                let mut metadata = HashMap::new();
                metadata.insert(
                    "source".to_string(),
                    parent.metadata.get("source").cloned().unwrap_or_default(),
                );
                metadata.insert("parent_id".to_string(), parent.id.clone());

                ChildChunk {
                    id: format!("{}_child_{}", doc_stem, i),
                    content,
                    metadata,
                    parent_id: parent.id.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_by_headers() {
        let chunker = DocumentChunker::new(Config::default());
        let text = "# Header 1\nSome content\n## Header 2\nMore content\n### Header 3\nEven more";

        let sections = chunker.split_by_headers(text);
        assert!(sections.len() >= 3);
    }

    #[test]
    fn test_split_by_chars() {
        let chunker = DocumentChunker::new(Config::default());
        let text = "Hello world this is a test string for chunking";

        let chunks = chunker.split_by_chars(text, 10, 3);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_merge_small_parents() {
        let chunker = DocumentChunker::new(Config::default());
        let chunks = vec![
            "Small".to_string(),
            "Another small".to_string(),
            "This is a larger chunk that should not be merged".to_string(),
        ];

        let merged = chunker.merge_small_parents(&chunks, 20);
        assert!(merged.len() <= chunks.len());
    }
}
