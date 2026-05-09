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
        .map(|(name, type_)| format!("  - {name}: {type_}"))
        .collect();
    format!(
        "Arguments are required for {tool_name}.\n\nRequired parameters:\n{}",
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
        let gewu_dir = default_home_dir().map(|home| home.join(".gewu"));
        let is_gewu_config_path = gewu_dir
            .as_ref()
            .is_some_and(|allowed| normalized.starts_with(allowed));
        if normalized.starts_with(root.as_ref()) || is_gewu_config_path {
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

/// Returns the user's home directory when it can be determined.
fn default_home_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|path| normalize_path(&path))
}

/// Returns all configured read roots for the given project root.
/// Combines: project root, detected home directory, and paths from GEWU_READ_ROOTS env var.
/// If the home directory cannot be detected, it is not added; this avoids accidentally
/// broadening read access to the filesystem root.
pub fn configured_read_roots(root: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut roots = vec![normalize_path(root.as_ref())];

    if let Some(home) = default_home_dir() {
        roots.push(home);
    }

    if let Ok(read_roots_var) = std::env::var("GEWU_READ_ROOTS") {
        for path_str in std::env::split_paths(&read_roots_var) {
            if path_str.as_os_str().is_empty() {
                continue;
            }
            let normalized = normalize_path(&path_str);
            roots.push(normalized);
        }
    }

    roots.sort();
    roots.dedup();
    roots
}

/// Checks if reading the given path is allowed based on configured read roots.
pub fn is_read_allowed(root: impl AsRef<Path>, path: impl AsRef<Path>) -> bool {
    let roots = configured_read_roots(root);
    let normalized = normalize_path(path.as_ref());
    roots.iter().any(|r| normalized.starts_with(r))
}

/// Formats a read denial error message listing all allowed roots.
fn format_read_denial(path: &Path, roots: &[PathBuf]) -> String {
    let roots_list: Vec<String> = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();

    format!(
        "Read access denied: {path:?} is outside of allowed read roots.\n\nAllowed read roots:\n{}\n\nTo add more paths, set the GEWU_READ_ROOTS environment variable (colon-separated on Unix, semicolon-separated on Windows).",
        roots_list
            .iter()
            .map(|r| format!("  - {r:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Resolves a user-provided path for read-only access.
/// Allows reading from the project root, home directory, or paths configured via GEWU_READ_ROOTS env var.
/// This is less restrictive than resolve_safe_path since reads are non-destructive.
pub fn resolve_read_path(root: impl AsRef<Path>, user_path: impl AsRef<Path>) -> Result<PathBuf> {
    let user_path = user_path.as_ref();

    if user_path.is_absolute() {
        let normalized = normalize_path(user_path);
        let roots = configured_read_roots(&root);

        // Allow absolute path only if it starts with one of the configured roots
        if is_read_allowed(&root, &normalized) {
            Ok(normalized)
        } else {
            Err(anyhow::anyhow!("{}", format_read_denial(&normalized, &roots)))
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
            "Path traversal detected: {normalized:?} is outside of root {root:?}",
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
        .with_context(|| format!("Failed to read text file: {path:?}"))
}

/// Reads a file into a byte vector.
pub async fn read_bytes(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    fs::read(path)
        .await
        .with_context(|| format!("Failed to read file: {path:?}"))
}

/// Reads a JSON file and deserializes it into type T.
pub async fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let content = read_text(path).await?;
    serde_json::from_str(&content).with_context(|| format!("Failed to parse JSON from: {path:?}"))
}

/// Writes byte content to a file, creating parent directories if needed.
pub async fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !exists(parent).await {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create directory: {parent:?}"))?;
        }
    }
    fs::write(path, content)
        .await
        .with_context(|| format!("Failed to write file: {path:?}"))?;
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
        .with_context(|| format!("Invalid glob pattern: {pattern}"))?;

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
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

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

    #[test]
    fn resolve_read_path_denies_absolute_path_outside_default_roots() {
        let dir = tempfile::tempdir().unwrap();
        let outside = std::path::PathBuf::from("/definitely-not-gewu-readable-root/example.txt");

        let err = crate::fs_utils::resolve_read_path(dir.path(), &outside)
            .expect_err("outside absolute path should be denied by default");

        let message = err.to_string();
        assert!(message.contains("Read access denied"));
        assert!(message.contains("Allowed read roots"));
    }

    #[test]
    fn resolve_read_path_allows_env_configured_read_root() {
        let _env_lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let project = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();
        let allowed_file = data_root.path().join("ATAC/ucf/P1-2_ATAC3.filtered.ucf");
        std::fs::create_dir_all(allowed_file.parent().unwrap()).unwrap();
        std::fs::write(&allowed_file, "ucf fixture").unwrap();

        let _guard = EnvVarGuard::set("GEWU_READ_ROOTS", data_root.path());
        let resolved = crate::fs_utils::resolve_read_path(project.path(), &allowed_file).unwrap();

        assert_eq!(resolved, crate::fs_utils::normalize_path(&allowed_file));
    }
}
