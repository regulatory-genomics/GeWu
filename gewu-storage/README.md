# gewu-storage

`gewu-storage` provides a persistent `Memory` implementation for `gewu-core` using SQLite.

## Features

- **SQLite Backend**: Utilizes `sqlx` for asynchronous database interactions.
- **Session Management**: Automatically creates and manages tables for storing conversation history.
- **Context Preservation**: Supports storing, retrieving, and updating conversation history.

## Usage

### SqliteMemory

To use persistent memory, create a `SqliteMemory` instance with a `SqlitePool` and a unique session ID.

```rust
use gewu_storage::SqliteMemory;
use gewu_core::Memory;
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::Arc;

let pool = SqlitePoolOptions::new()
    .connect("sqlite://path/to/db.sqlite")
    .await?;

let session_id = "unique-session-id";
let memory = Box::new(SqliteMemory::new(pool, session_id).await?);

// Pass to agent builder
let agent = Agent::builder()
    .memory(memory)
    // ...
    .build()?;
```

### Session history queries

Standalone helpers work directly on the pool: `list_sessions` (newest first),
`load_session` (one session's messages), and `delete_session` (remove a
session's stored messages, returning the number of rows removed).
