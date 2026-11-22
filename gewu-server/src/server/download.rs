//! HTTP file downloads.
//!
//! Files are streamed as attachments. Folders are packaged into a ZIP that
//! mirrors the file tree (hidden entries are skipped, ignore rules such as
//! `.gitignore` are not applied, symlinks are not followed) and streamed from
//! a temporary file, so memory use stays bounded regardless of folder size.

use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use tracing::warn;

use super::ServerState;

/// Largest number of ZIP entries a folder download may contain.
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
/// Largest total uncompressed size a folder download may contain.
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Characters safe to leave unescaped in the RFC 5987 filename.
const DISPOSITION_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

#[derive(Deserialize)]
struct DownloadQuery {
    session_id: String,
    path: String,
}

/// HTTP routes for file downloads.
pub(super) fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/download", get(download).head(download_head))
        .with_state(state)
}

async fn download(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, DownloadError> {
    prepare(state, headers, query, false).await
}

async fn download_head(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, DownloadError> {
    prepare(state, headers, query, true).await
}

async fn prepare(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: DownloadQuery,
    head_only: bool,
) -> Result<Response, DownloadError> {
    authorize(&state, &headers)?;
    let root = session_root(&state, &query.session_id).await?;
    let path = super::resolve_workspace_path(&root, &query.path, state.bootstrap.enforcement)
        .map_err(|error| DownloadError::BadRequest(error.to_string()))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| DownloadError::NotFound(format!("cannot read path: {error}")))?;
    if metadata.is_dir() {
        archive_response(&path, head_only).await
    } else if metadata.is_file() {
        file_response(&path, &metadata, head_only).await
    } else {
        Err(DownloadError::BadRequest(
            "path is not a file or directory".into(),
        ))
    }
}

fn authorize(state: &ServerState, headers: &HeaderMap) -> Result<(), DownloadError> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(());
    };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied == Some(expected) {
        Ok(())
    } else {
        Err(DownloadError::Unauthorized)
    }
}

/// Resolve the workspace root for a session, falling back to the stored
/// session workspace and finally `cwd`, mirroring session creation.
async fn session_root(state: &ServerState, session_id: &str) -> Result<String, DownloadError> {
    {
        let sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            return Ok(session.workspace.path.clone());
        }
    }
    let stored = gewu_storage::session_workspace(&state.bootstrap.pool, session_id)
        .await
        .map_err(DownloadError::internal)?;
    let workspace_id = stored.unwrap_or_else(|| "cwd".to_string());
    let workspace = state
        .restored_workspace(session_id, &workspace_id)
        .await
        .map_err(DownloadError::internal)?;
    Ok(workspace.path)
}

async fn file_response(
    path: &Path,
    metadata: &std::fs::Metadata,
    head_only: bool,
) -> Result<Response, DownloadError> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, metadata.len().to_string())
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Ok(value) = HeaderValue::from_str(&attachment(&name)) {
        builder = builder.header(header::CONTENT_DISPOSITION, value);
    }
    let body = if head_only {
        Body::empty()
    } else {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(DownloadError::internal)?;
        Body::from_stream(ReaderStream::new(file))
    };
    builder
        .body(body)
        .map_err(|error| DownloadError::Internal(error.to_string()))
}

async fn archive_response(path: &Path, head_only: bool) -> Result<Response, DownloadError> {
    let folder = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let name = format!("{folder}.zip");
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Ok(value) = HeaderValue::from_str(&attachment(&name)) {
        builder = builder.header(header::CONTENT_DISPOSITION, value);
    }

    let body = if head_only {
        Body::empty()
    } else {
        let root = path.to_path_buf();
        let folder_name = folder.clone();
        let (file, size) = tokio::task::spawn_blocking(move || build_zip(&root, &folder_name))
            .await
            .map_err(DownloadError::internal)??;
        builder = builder.header(header::CONTENT_LENGTH, size.to_string());
        Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)))
    };
    builder
        .body(body)
        .map_err(|error| DownloadError::Internal(error.to_string()))
}

/// Build the folder archive into an auto-deleted temporary file.
fn build_zip(root: &Path, folder_name: &str) -> Result<(std::fs::File, u64), DownloadError> {
    let file = tempfile::tempfile().map_err(DownloadError::internal)?;
    let mut zip = zip::ZipWriter::new(file);
    let file_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let dir_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    zip.add_directory(format!("{folder_name}/"), dir_options)
        .map_err(DownloadError::internal)?;

    let mut entries = 1usize;
    let mut total = 0u64;
    // Hidden entries stay out, but ignore rules must not filter the archive:
    // a directory the file tree shows must download completely.
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .build()
        .skip(1)
    {
        let entry = entry.map_err(DownloadError::internal)?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        // Symlinks can escape the root or form cycles; archive regular paths.
        if file_type.is_symlink() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(DownloadError::internal)?;
        let name = format!(
            "{folder_name}/{}",
            relative.to_string_lossy().replace('\\', "/")
        );
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err(DownloadError::TooLarge(format!(
                "folder has more than {MAX_ARCHIVE_ENTRIES} entries"
            )));
        }
        if file_type.is_dir() {
            zip.add_directory(format!("{name}/"), dir_options)
                .map_err(DownloadError::internal)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(DownloadError::internal)?;
        total = total.saturating_add(metadata.len());
        if total > MAX_ARCHIVE_BYTES {
            return Err(DownloadError::TooLarge(format!(
                "folder exceeds the {} GiB download limit",
                MAX_ARCHIVE_BYTES / (1024 * 1024 * 1024)
            )));
        }
        zip.start_file(name, file_options)
            .map_err(DownloadError::internal)?;
        let mut source = std::fs::File::open(entry.path()).map_err(DownloadError::internal)?;
        std::io::copy(&mut source, &mut zip).map_err(DownloadError::internal)?;
    }

    let mut file = zip.finish().map_err(DownloadError::internal)?;
    let size = file
        .seek(SeekFrom::End(0))
        .map_err(DownloadError::internal)?;
    file.seek(SeekFrom::Start(0))
        .map_err(DownloadError::internal)?;
    Ok((file, size))
}

/// `attachment` disposition with an ASCII fallback plus RFC 5987 UTF-8 name.
fn attachment(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let fallback = fallback.trim().trim_matches('.');
    let fallback = if fallback.is_empty() {
        "download"
    } else {
        fallback
    };
    let encoded = utf8_percent_encode(name, DISPOSITION_ENCODE_SET);
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[derive(Debug)]
enum DownloadError {
    Unauthorized,
    BadRequest(String),
    NotFound(String),
    TooLarge(String),
    Internal(String),
}

impl DownloadError {
    fn internal(error: impl std::fmt::Display) -> Self {
        warn!("download failed: {error}");
        Self::Internal(error.to_string())
    }
}

impl IntoResponse for DownloadError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
            Self::TooLarge(message) => (StatusCode::PAYLOAD_TOO_LARGE, message),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        (status, message).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn attachment_has_ascii_fallback_and_utf8_name() {
        let value = attachment("résumé 1.txt");
        assert!(
            value.starts_with("attachment; filename=\"r_sum_ 1.txt\"; filename*=UTF-8''"),
            "{value}"
        );
        assert!(value.ends_with("r%C3%A9sum%C3%A9%201.txt"), "{value}");
    }

    #[test]
    fn archive_skips_hidden_entries_but_keeps_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/nested/deep.txt"), "deep\n").unwrap();
        std::fs::write(root.join(".hidden"), "no\n").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("ignored.log"), "no\n").unwrap();
        std::fs::write(root.join(".ignore"), "*.tmp\n").unwrap();
        std::fs::write(root.join("scratch.tmp"), "no\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("README.md"), root.join("link.md")).unwrap();

        let (file, size) = build_zip(&root, "project").unwrap();
        assert!(size > 0);
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entries = std::collections::BTreeMap::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_string();
            let mut content = String::new();
            if !entry.is_dir() {
                entry.read_to_string(&mut content).unwrap();
            }
            entries.insert(name, content);
        }

        assert_eq!(entries.remove("project/").as_deref(), Some(""));
        assert_eq!(entries.remove("project/README.md").as_deref(), Some("hi\n"));
        assert_eq!(
            entries.remove("project/src/main.rs").as_deref(),
            Some("fn main() {}\n")
        );
        assert_eq!(
            entries.remove("project/src/nested/deep.txt").as_deref(),
            Some("deep\n")
        );
        // Ignore rules must not filter the archive; only dotfiles and
        // symlinks stay out.
        assert_eq!(
            entries.remove("project/ignored.log").as_deref(),
            Some("no\n")
        );
        assert_eq!(
            entries.remove("project/scratch.tmp").as_deref(),
            Some("no\n")
        );
        assert!(entries.contains_key("project/src/"), "{entries:?}");
        assert!(entries.contains_key("project/src/nested/"), "{entries:?}");
        assert!(
            !entries.keys().any(|name| name.starts_with("project/.hid")
                || name.starts_with("project/.git")
                || name.starts_with("project/.ign")
                || name.contains("link.md")),
            "{entries:?}"
        );
    }
}
