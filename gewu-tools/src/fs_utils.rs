//! Filesystem Utilities
//!
//! This module provides high-level async wrappers around `tokio::fs` operations,
//! simplifying common tasks like reading/writing text, JSON handling, and
//! recursive file searching.

#![allow(dead_code)]
#![allow(clippy::collapsible_if)]
use anyhow::{Context, Result};
use mime_guess;
use serde::{Serialize, de::DeserializeOwned};
use std::path::{Component, Path, PathBuf};
use tokio::fs;

pub fn into_tool_error(e: impl Into<anyhow::Error>) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::other(e.into()))
}

pub fn parse_empty_args(tool_name: &str, required_params: &[(&str, &str)]) -> String {
    let params: Vec<String> = required_params
        .iter()
        .map(|(name, type_)| format!("  - {}: {}", name, type_))
        .collect();
    format!(
        "Arguments are required for {}.\n\nRequired parameters:\n{}",
        tool_name,
        params.join("\n")
    )
}

/// Normalizes a path by resolving `.` and `..` components lexically.
/// This does not require the path to exist.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    ret
}

/// Resolves a user-provided path for write access.
/// Allows writing to the project root OR ~/.gewu/ (GeWu's config directory).
pub fn resolve_write_path(root: impl AsRef<Path>, user_path: impl AsRef<Path>) -> Result<PathBuf> {
    let user_path = user_path.as_ref();

    if user_path.is_absolute() {
        let normalized = normalize_path(user_path);
        let gewu_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".gewu");
        if normalized.starts_with(root.as_ref()) || normalized.starts_with(&gewu_dir) {
            Ok(normalized)
        } else {
            Err(anyhow::anyhow!(
                "Path traversal detected: {:?} is outside of root {:?}",
                normalized,
                root.as_ref()
            ))
        }
    } else {
        resolve_safe_path(root, user_path)
    }
}

/// Resolves a user-provided path for read-only access.
/// Allows reading from anywhere under the user's home directory or the project root.
/// This is less restrictive than resolve_safe_path since reads are non-destructive.
pub fn resolve_read_path(root: impl AsRef<Path>, user_path: impl AsRef<Path>) -> Result<PathBuf> {
    let user_path = user_path.as_ref();

    if user_path.is_absolute() {
        let normalized = normalize_path(user_path);
        // Allow any absolute path under home dir or project root
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        if normalized.starts_with(&home) || normalized.starts_with(root.as_ref()) {
            Ok(normalized)
        } else {
            Err(anyhow::anyhow!(
                "Read access denied: {:?} is outside of home directory",
                normalized
            ))
        }
    } else {
        // Relative paths resolve against root (same as before)
        resolve_safe_path(root, user_path)
    }
}

/// Resolves a user-provided path against a root directory, ensuring it stays within the root.
/// Returns the absolute path if safe, or an error if it attempts to traverse outside.
pub fn resolve_safe_path(root: impl AsRef<Path>, user_path: impl AsRef<Path>) -> Result<PathBuf> {
    let root = root.as_ref();
    let user_path = user_path.as_ref();

    // If user_path is absolute, it must start with root.
    // If it is relative, we join it to root.
    let joined = if user_path.is_absolute() {
        let user_path_ref: &Path = user_path;
        if user_path_ref == Path::new("/") {
            return Ok(root.join("."));
        }
        user_path.to_path_buf()
    } else {
        root.join(user_path)
    };

    let normalized = normalize_path(&joined);

    // Check if the normalized path starts with the root
    if normalized.starts_with(root) {
        Ok(normalized)
    } else {
        Err(anyhow::anyhow!(
            "Path traversal detected: {:?} is outside of root {:?}",
            normalized,
            root
        ))
    }
}

/// Checks if a path exists.
pub async fn exists(path: impl AsRef<Path>) -> bool {
    fs::metadata(path).await.is_ok()
}

/// Checks if a path exists and is a directory.
pub async fn is_dir(path: impl AsRef<Path>) -> bool {
    fs::metadata(path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// Reads a file into a String.
pub async fn read_text(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read text file: {:?}", path))
}

/// Reads a file into a byte vector.
pub async fn read_bytes(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    fs::read(path)
        .await
        .with_context(|| format!("Failed to read file: {:?}", path))
}

/// Reads a JSON file and deserializes it into type T.
pub async fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let content = read_text(path).await?;
    serde_json::from_str(&content).with_context(|| format!("Failed to parse JSON from: {:?}", path))
}

/// Writes byte content to a file, creating parent directories if needed.
pub async fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !exists(parent).await {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create directory: {:?}", parent))?;
        }
    }
    fs::write(path, content)
        .await
        .with_context(|| format!("Failed to write file: {:?}", path))?;
    Ok(())
}

/// Serializes data to JSON and writes it to a file.
pub async fn write_json<T: Serialize>(path: impl AsRef<Path>, data: &T) -> Result<()> {
    let content = serde_json::to_string_pretty(data)?;
    write(path, content.as_bytes()).await
}

/// Guesses the MIME type of a file based on its extension.
pub fn mime_type(path: impl AsRef<Path>) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

/// Returns true if the file appears to be binary based on extension and content sampling.
/// Extension check first, then sample content if needed.
pub async fn is_binary_file(path: impl AsRef<Path>, file_size: u64) -> Result<bool> {
    let path = path.as_ref();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    // Binary extensions list (same as legacy)
    if let Some(
        ".zip" | ".tar" | ".gz" | ".exe" | ".dll" | ".so" | ".class" | ".jar" | ".war" | ".7z"
        | ".doc" | ".docx" | ".xls" | ".xlsx" | ".ppt" | ".pptx" | ".odt" | ".ods" | ".odp"
        | ".bin" | ".dat" | ".obj" | ".o" | ".a" | ".lib" | ".wasm" | ".pyc" | ".pyo",
    ) = ext.as_deref()
    {
        return Ok(true);
    }

    if file_size == 0 {
        return Ok(false);
    }

    let fh = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(fh);
    let sample_size = std::cmp::min(4096, file_size as usize);
    let mut bytes = vec![0u8; sample_size];
    let result = tokio::io::AsyncReadExt::read(&mut reader, &mut bytes).await?;
    if result == 0 {
        return Ok(false);
    }

    let mut non_printable_count = 0;
    for i in 0..result {
        if bytes[i] == 0 {
            return Ok(true);
        }
        if bytes[i] < 9 || (bytes[i] > 13 && bytes[i] < 32) {
            non_printable_count += 1;
        }
    }

    // If >30% non-printable characters, consider it binary
    Ok(non_printable_count as f64 / result as f64 > 0.3)
}

/// Searches for a file or directory by walking up from `start_dir`.
pub async fn find_up(name: impl AsRef<Path>, start_dir: impl AsRef<Path>) -> Option<PathBuf> {
    let mut current = start_dir.as_ref().to_path_buf();
    loop {
        let target = current.join(&name);
        if exists(&target).await {
            return Some(target);
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// Finds files matching a glob pattern.
pub fn glob(pattern: &str) -> Result<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = glob::glob(pattern)?
        .filter_map(|entry| entry.ok())
        .collect();
    Ok(paths)
}

/// Finds files matching a glob pattern, respecting .gitignore files.
pub fn glob_with_gitignore(root: impl AsRef<Path>, pattern: &str) -> Result<Vec<PathBuf>> {
    let root = root.as_ref();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false) // Allow hidden files unless in .gitignore
        .git_ignore(true)
        .build();

    let glob_pattern = glob::Pattern::new(pattern)
        .with_context(|| format!("Invalid glob pattern: {}", pattern))?;

    let mut matches = Vec::new();

    for result in walker {
        match result {
            Ok(entry) => {
                let path = entry.path();
                if path.is_file() {
                    // Compute path relative to root to match against the pattern
                    if let Ok(relative_path) = path.strip_prefix(root) {
                        // glob::Pattern matches against the string representation
                        //We need to handle the case where the pattern assumes the root is the current directory
                        if glob_pattern.matches_path(relative_path) {
                            matches.push(path.to_path_buf());
                        }
                    }
                }
            }
            Err(_err) => {
                // We might want to log this but for now just ignore/continue
                // eprintln!("Error walking directory: {}", err);
            }
        }
    }

    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_read_write_json() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("test.json");

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct TestData {
            name: String,
            value: i32,
        }

        let data = TestData {
            name: "test".to_string(),
            value: 42,
        };

        write_json(&file_path, &data).await?;
        let read_data: TestData = read_json(&file_path).await?;

        assert_eq!(data, read_data);
        Ok(())
    }
}
