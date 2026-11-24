//! Bounded access to a completed plugin result.
//!
//! A gateway result can be larger than the model context. The compact response
//! carries a result id; this module reads that stored value by JSON pointer or
//! materializes the selected slice as a workspace file. It never invents a
//! second result store and never accepts an arbitrary filesystem path.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use gewu_core::types::SessionState;

const MAX_INLINE_BYTES: usize = 12 * 1024;

pub async fn read(
    state: &SessionState,
    session: &str,
    result_id: &str,
    pointer: &str,
) -> Result<Value> {
    let value = load(state, session, result_id, pointer).await?;
    let bytes = serde_json::to_vec(&value)?.len();
    if bytes > MAX_INLINE_BYTES {
        return Ok(json!({
            "ok": true,
            "complete": false,
            "resultId": result_id,
            "pointer": pointer,
            "bytes": bytes,
            "limit": MAX_INLINE_BYTES,
            "next_action": {
                "tool": "plugin_result",
                "args": {"action": "export", "resultId": result_id, "pointer": pointer}
            }
        }));
    }
    Ok(json!({
        "ok": true,
        "complete": true,
        "resultId": result_id,
        "pointer": pointer,
        "bytes": bytes,
        "value": value,
    }))
}

pub async fn export(
    state: &SessionState,
    session: &str,
    root: &Path,
    result_id: &str,
    pointer: &str,
    destination: &str,
) -> Result<Value> {
    let value = load(state, session, result_id, pointer).await?;
    let bytes = serde_json::to_vec_pretty(&value)?;
    let relative = workspace_relative(destination)?;
    let root = root.canonicalize().context("workspace root is unavailable")?;
    let target = root.join(&relative);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        ensure!(
            parent.starts_with(&root),
            "destination escapes the workspace before writing"
        );
    }
    if target
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        anyhow::bail!("destination must be a regular file");
    }
    if let Ok(existing) = std::fs::read(&target) {
        if existing == bytes {
            return Ok(receipt(result_id, pointer, destination, &bytes, true));
        }
        anyhow::bail!("OUTPUT_EXISTS: destination already contains different content");
    }
    let temporary = target.with_file_name(format!(
        ".{}.tmp-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("result"),
        uuid::Uuid::new_v4()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = std::fs::hard_link(&temporary, &target) {
        let _ = std::fs::remove_file(&temporary);
        if target.exists() {
            anyhow::bail!("OUTPUT_EXISTS: destination was created concurrently");
        }
        return Err(error.into());
    }
    std::fs::remove_file(&temporary)?;
    Ok(receipt(result_id, pointer, destination, &bytes, false))
}

fn receipt(result_id: &str, pointer: &str, destination: &str, bytes: &[u8], reused: bool) -> Value {
    let mut digest = Sha256::new();
    digest.update(bytes);
    json!({
        "ok": true,
        "complete": true,
        "resultId": result_id,
        "pointer": pointer,
        "destination": destination,
        "bytes": bytes.len(),
        "sha256": format!("sha256:{:x}", digest.finalize()),
        "reused": reused,
    })
}

async fn load(
    state: &SessionState,
    session: &str,
    result_id: &str,
    pointer: &str,
) -> Result<Value> {
    ensure!(
        !result_id.is_empty()
            && result_id.len() <= 128
            && result_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'),
        "INVALID_RESULT_ID"
    );
    let stored: Option<Value> = state
        .read(session, &format!("gewu.plugin.result.{result_id}"))
        .await?;
    let stored = stored.context("UNKNOWN_RESULT: no stored result for this session")?;
    let data = stored
        .get("result")
        .and_then(|result| result.get("data"))
        .cloned()
        .unwrap_or(stored);
    if pointer.is_empty() || pointer == "/" {
        return Ok(data);
    }
    data.pointer(pointer)
        .cloned()
        .with_context(|| format!("POINTER_NOT_FOUND: {pointer}"))
}

fn workspace_relative(destination: &str) -> Result<PathBuf> {
    let rest = destination
        .strip_prefix("workspace:///")
        .context("destination must use workspace:///")?;
    ensure!(!rest.is_empty(), "destination is empty");
    let path = Path::new(rest);
    ensure!(
        path.components().all(|component| matches!(
            component,
            Component::Normal(_)
        )),
        "destination must be a contained relative path"
    );
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::types::SessionState;
    use serde_json::json;

    async fn state_with(value: Value) -> SessionState {
        let state = SessionState::in_memory();
        state
            .write(
                "session-a",
                "gewu.plugin.result.result-123",
                &json!({"result": {"data": value}}),
            )
            .await
            .unwrap();
        state
    }

    #[tokio::test]
    async fn reads_a_pointer_and_exports_once() {
        let state = state_with(json!({"next_action": {"request_template": {"input": {"path": ""}}}}))
            .await;
        let read = read(&state, "session-a", "result-123", "/next_action/request_template")
            .await
            .unwrap();
        assert_eq!(read["value"]["input"]["path"], "");
        let dir = tempfile::tempdir().unwrap();
        let receipt = export(
            &state,
            "session-a",
            dir.path(),
            "result-123",
            "/next_action/request_template",
            "workspace:///run/request.json",
        )
        .await
        .unwrap();
        assert_eq!(receipt["reused"], false);
        let bytes = std::fs::read(dir.path().join("run/request.json")).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["input"]["path"], "");
        let again = export(
            &state,
            "session-a",
            dir.path(),
            "result-123",
            "/next_action/request_template",
            "workspace:///run/request.json",
        )
        .await
        .unwrap();
        assert_eq!(again["reused"], true);
    }

    #[tokio::test]
    async fn rejects_other_sessions_and_path_escape() {
        let state = state_with(json!({"value": 1})).await;
        assert!(read(&state, "session-b", "result-123", "").await.is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(export(&state, "session-a", dir.path(), "result-123", "", "workspace:///../escape.json")
            .await
            .is_err());
    }
}
