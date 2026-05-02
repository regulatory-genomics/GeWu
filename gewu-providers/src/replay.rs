//! Test-only LLM providers for deterministic workflow validation.
//!
//! Two flavors:
//!
//! - [`ScriptedLlmProvider`] — returns events from a FIFO queue. Each call
//!   to `stream_response` pops one batch. Simple, ignores inputs. Use for
//!   small hand-written scenarios.
//!
//! - [`ReplayLlmProvider`] — content-addressed. Hashes `(messages, tool names)`
//!   and returns the pre-recorded events for that key. Fails on cache miss
//!   so that a test cannot silently hit a live model. Recordings live in a
//!   JSONL file: one object per line with `{ "key": "<hex>", "events": [...] }`.
//!   Future: a `RecordingLlmProvider` that wraps a real provider and writes
//!   out such files from live runs.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use gewu_core::traits::{LlmProvider, Tool};
use gewu_core::types::{Message, ProviderEvent};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// ScriptedLlmProvider
// ---------------------------------------------------------------------------

pub struct ScriptedLlmProvider {
    queue: Mutex<VecDeque<Vec<ProviderEvent>>>,
    strict: bool,
}

impl ScriptedLlmProvider {
    /// Create a provider that serves `batches` in FIFO order. When `strict`,
    /// running out of batches yields an `Error` event; otherwise it yields
    /// an empty batch (which the agent treats as an immediate finish).
    pub fn new(batches: Vec<Vec<ProviderEvent>>, strict: bool) -> Self {
        Self {
            queue: Mutex::new(batches.into()),
            strict,
        }
    }

    pub fn remaining(&self) -> usize {
        self.queue.lock().len()
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlmProvider {
    async fn stream_response<'a>(
        &'a self,
        _messages: &[Message],
        _tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let mut q = self.queue.lock();
        let batch = q.pop_front();
        drop(q);
        let events = match batch {
            Some(b) => b,
            None if self.strict => {
                return Err("ScriptedLlmProvider: queue exhausted (strict mode)".into());
            }
            None => vec![ProviderEvent::Finish { reason: None }],
        };
        let s = stream::iter(events.into_iter().map(Ok));
        Ok(s.boxed())
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["scripted".into()])
    }
}

// ---------------------------------------------------------------------------
// ReplayLlmProvider
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub key: String,
    pub events: Vec<ProviderEvent>,
    /// Human-readable excerpt of the request that produced this key (for
    /// debugging cache misses). Not used in lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

pub struct ReplayLlmProvider {
    by_key: HashMap<String, Vec<ProviderEvent>>,
}

impl ReplayLlmProvider {
    pub fn from_entries(entries: Vec<ReplayEntry>) -> Self {
        let mut by_key = HashMap::new();
        for e in entries {
            by_key.insert(e.key, e.events);
        }
        Self { by_key }
    }

    /// Load entries from a JSONL file (one `ReplayEntry` per line).
    pub fn load_jsonl(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut entries = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let e: ReplayEntry = serde_json::from_str(line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            entries.push(e);
        }
        Ok(Self::from_entries(entries))
    }

    /// Compute the content-addressed key for a `(messages, tool_names)` pair.
    /// Canonical form: `json!({ "messages": <messages>, "tools": <names> })`
    /// serialized with sorted-key objects, SHA-256 hex digest.
    pub fn key(messages: &[Message], tools: &[Arc<dyn Tool>]) -> String {
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let canonical = serde_json::json!({
            "messages": messages,
            "tools": tool_names,
        });
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hex::encode(hasher.finalize())
    }
}

#[async_trait]
impl LlmProvider for ReplayLlmProvider {
    async fn stream_response<'a>(
        &'a self,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
    ) -> Result<
        BoxStream<'a, Result<ProviderEvent, Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let key = Self::key(messages, tools);
        let events = self
            .by_key
            .get(&key)
            .ok_or_else(|| {
                format!(
                    "ReplayLlmProvider: cache miss for key {} ({} tools, {} messages)",
                    &key,
                    tools.len(),
                    messages.len()
                )
            })?
            .clone();
        let s = stream::iter(events.into_iter().map(Ok));
        Ok(s.boxed())
    }

    async fn list_models(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        Ok(vec!["replay".into()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn scripted_returns_batches_in_order() {
        let p = ScriptedLlmProvider::new(
            vec![
                vec![ProviderEvent::ContentDelta { delta: "hi".into() }],
                vec![ProviderEvent::ContentDelta {
                    delta: "bye".into(),
                }],
            ],
            true,
        );
        let stream = p.stream_response(&[], &[]).await.unwrap();
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            ProviderEvent::ContentDelta { delta } => assert_eq!(delta, "hi"),
            _ => panic!(),
        }
        let stream2 = p.stream_response(&[], &[]).await.unwrap();
        let events2: Vec<_> = stream2.collect().await;
        match events2[0].as_ref().unwrap() {
            ProviderEvent::ContentDelta { delta } => assert_eq!(delta, "bye"),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn scripted_strict_errors_when_exhausted() {
        let p = ScriptedLlmProvider::new(vec![], true);
        let r = p.stream_response(&[], &[]).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn replay_roundtrip_via_jsonl() {
        let msgs = vec![Message::User {
            content: vec![gewu_core::types::ContentPart::Text {
                text: "hello".into(),
            }],
        }];
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let key = ReplayLlmProvider::key(&msgs, &tools);

        let entry = ReplayEntry {
            key: key.clone(),
            events: vec![ProviderEvent::ContentDelta {
                delta: "recorded".into(),
            }],
            excerpt: Some("hello".into()),
        };
        let line = serde_json::to_string(&entry).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.jsonl");
        std::fs::write(&path, format!("{}\n", line)).unwrap();

        let p = ReplayLlmProvider::load_jsonl(&path).unwrap();
        let stream = p.stream_response(&msgs, &tools).await.unwrap();
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            ProviderEvent::ContentDelta { delta } => assert_eq!(delta, "recorded"),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn replay_cache_miss_errors() {
        let p = ReplayLlmProvider::from_entries(vec![]);
        match p.stream_response(&[], &[]).await {
            Ok(_) => panic!("expected cache miss error"),
            Err(e) => assert!(e.to_string().contains("cache miss")),
        }
    }

    #[test]
    fn replay_key_is_stable_for_same_input() {
        let msgs = vec![Message::User {
            content: vec![gewu_core::types::ContentPart::Text { text: "x".into() }],
        }];
        let k1 = ReplayLlmProvider::key(&msgs, &[]);
        let k2 = ReplayLlmProvider::key(&msgs, &[]);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64); // SHA-256 hex length
    }
}
