# GeWu RAG

A Rust implementation of Agentic RAG (Retrieval Augmented Generation) system.

## Features

- **Hierarchical Indexing**: Parent-child chunking strategy for improved retrieval
- **Hybrid Search**: Dense embeddings (Qwen3) + Sparse (BM25) with RRF fusion via Qdrant
- **Multi-Agent Graph**: Uses `gewu-graph` for LangGraph-style workflow orchestration
- **Query Pipeline**:
  - Conversation summarization
  - Query rewriting/clarification
  - Parallel agent subgraphs (Map-Reduce pattern)
  - Context compression
  - Self-correction
- **Tool-based Agent**: Uses `gewu-core` Tool trait for retrieval operations
- **Configurable**: CLI args, environment variables, or JSONC config file

## Architecture

```
User Query
    ↓
summarize_history → rewrite_query → [parallel agents] → aggregate_answers
                                     ↓
                           orchestrator → tools → compress_context
                                     ↓
                               fallback_response → collect_answer
```

### Agent Subgraph
```
orchestrator (decides: tools or collect_answer)
    ↓
tools (executes search_child_chunks, retrieve_parent_chunks, retrieve_many_parent_chunks)
    ↓
should_compress (checks token budget)
    ↓
compress_context (reduces context if needed)
    ↓
orchestrator (loops back)
```

## Installation

```bash
# Add to your Cargo.toml
[dependencies]
gewu-rag = { path = "../gewu-rag" }
```

## Configuration

GeWu RAG supports configuration via **CLI arguments**, **environment variables**, or a **config file**.

### Priority Order

CLI arguments override config file values, which override environment variables.

### Config File (Recommended)

Create a `config.jsonc` file:

```jsonc
{
  // API Configuration
  "api_key": "sk-your-key-here",
  "base_url": "https://api.openai.com/v1",
  "model": "gpt-4.1",

  // Qdrant Configuration
  "qdrant_url": "http://localhost:6334",
  "qdrant_api_key": "your-qdrant-api-key",
  "parent_store_path": "./parent_store",
  "collection": "document_child_chunks",

  // Indexing Configuration
  "chunk_size": 500,
  "chunk_overlap": 100,
  "min_parent_size": 2000,
  "max_parent_size": 4000,
  "docs_dir": "./markdown_docs",

  // RAG Configuration
  "max_iterations": 10,
  "max_tool_calls": 8,
  "similarity_threshold": 0.7,
  "enable_sparse": false
}
```

Run with config file:
```bash
cargo run -p gewu-rag --bin gewu-rag --config config.jsonc --query "Your question"
```

## CLI Usage

### Index Documents

```bash
cargo run -p gewu-rag --bin gewu-rag --index ./markdown_docs \
  --config config.jsonc
```

### Query

```bash
cargo run -p gewu-rag --bin gewu-rag --query "What is RAG?" \
  --config config.jsonc
```

### Interactive Mode

```bash
cargo run -p gewu-rag --bin gewu-rag --interactive \
  --config config.jsonc
```

### Recreate Index

```bash
cargo run -p gewu-rag --bin gewu-rag --index ./markdown_docs --recreate-index \
  --config config.jsonc
```

## CLI Options

| Flag | Description | Default |
|------|-------------|---------|
| `--config` | Config file (JSONC) | - |
| `--query` | Query to search | - |
| `--index` | Directory to index | - |
| `--interactive` | Interactive query mode | false |
| `--recreate-index` | Clear and rebuild index | false |
| `--qdrant-url` | Qdrant server URL | http://localhost:6334 |
| `--qdrant-api-key` | Qdrant API key | - |
| `--parent-store-path` | Parent chunk storage | ./parent_store |
| `--collection` | Collection name | document_child_chunks |
| `--chunk-size` | Child chunk size | 500 |
| `--chunk-overlap` | Chunk overlap | 100 |
| `--min-parent-size` | Min parent chunk size | 2000 |
| `--max-parent-size` | Max parent chunk size | 4000 |
| `--docs-dir` | Documents directory | markdown_docs |
| `--output-dir` | Output directory | parent_store |
| `--max-iterations` | Max agent iterations | 10 |
| `--max-tool-calls` | Max tool calls per iteration | 8 |
| `--similarity-threshold` | Retrieval threshold (0.0-1.0) | 0.7 |
| `--base-url` | LLM endpoint | https://api.openai.com/v1 |
| `--api-key` | LLM API key | - |
| `--model` | LLM model | gpt-4.1 |
| `--enable-sparse` | Enable BM25 hybrid search | false |

### Environment Variables

- `OPENAI_API_KEY` - API key for LLM
- `OPENAI_BASE_URL` - LLM endpoint
- `QDRANT_URL` - Qdrant server URL
- `QDRANT_API_KEY` - Qdrant API key
- `PARENT_STORE_PATH` - Parent chunk storage path

## Library Usage

```rust
use gewu_rag::{Config, RagSystem};
use gewu_providers::OpenAIProvider;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = Config::new();
    let llm = Arc::new(OpenAIProvider::new_with_endpoint(&api_key, &endpoint, &model));
    let rag_system = RagSystem::new(config, llm).await?;
    let answer = rag_system.query("What is RAG?").await?;
    println!("{}", answer);

    Ok(())
}
```

## Components

### Document Processing
- `DocumentChunker`: Splits documents into parent-child chunks using regex header splitting
- `DocumentIndexer`: Indexes markdown files into Qdrant with BM25 sparse vectors

### Retrieval
- `QdrantStore`: Vector storage with Qwen3 embeddings (1024 dimensions) via Candle
- `ParentStore`: JSON file storage for full document chunks
- `SearchChildChunksTool`: Search vector DB for relevant chunks
- `RetrieveParentChunksTool`: Retrieve single parent document
- `RetrieveManyParentChunksTool`: Retrieve multiple parent documents in batch

### Agent
- `RagSystem`: Main system orchestrating the RAG pipeline
- Graph nodes: `orchestrator`, `tools`, `compress_context`, `fallback_response`, `collect_answer`, `aggregate_answers`

## Known Limitations

| Feature | Status | Notes |
|---------|--------|-------|
| Hybrid Search (Dense + Sparse) | ✅ Implemented | BM25 + RRF fusion |
| True Streaming UI | TODO | CLI does not yet support streaming |
| Conversation Checkpointing | TODO | No persistent thread/history |
| PDF Processing | TODO | `pdf_to_markdown` is stub |
| Gradio UI | TODO | CLI only for now |
| Langfuse Observability | TODO | No callback integration |

## Embedding Model

Uses **Potion (Model2Vec)** via `embed_anything` with Candle backend:

- **Model**: `minishlab/potion-base-8M`
- **Size**: 8M parameters (lightweight, fast CPU inference)
- **Dimensions**: 256
- **Backend**: Candle (Rust-native, no ONNX Runtime)

This replaces the previous `fastembed` dependency which had ONNX Runtime linking issues on some systems.

For production with better quality embeddings, consider using `Qwen/Qwen3-Embedding-0.6B` (509M params, multilingual, 1024 dimensions).

## Dependencies

- `gewu-core`: Core traits and types
- `gewu-providers`: LLM provider integration
- `gewu-graph`: Graph orchestration (LangGraph-style)
- `qdrant-client`: Vector database client
- `embed_anything`: Embedding generation (Candle backend)

## License

Same as GeWu workspace.
