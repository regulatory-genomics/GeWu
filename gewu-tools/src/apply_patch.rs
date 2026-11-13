use crate::fs_utils::{
    canonicalize_root, into_tool_error, resolve_read_path, resolve_safe_existing_path,
    resolve_safe_write_path, resolve_write_path,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{PathEnforcement, ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use std::collections::HashSet;
use std::error::Error;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::fs;
use tokio::io::AsyncWriteExt;

static TEMP_ID: AtomicU64 = AtomicU64::new(1);
const MAX_PATCH_BYTES: usize = 1024 * 1024;
const MAX_PATCH_LINES: usize = 100_000;
const MAX_OPERATIONS: usize = 256;
const MAX_CHUNKS: usize = 4_096;
const MAX_CHANGE_LINES: usize = 200_000;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILE_LINES: usize = 500_000;
const MAX_TRANSACTION_BYTES: usize = 32 * 1024 * 1024;
const MAX_DIFF_INPUT_BYTES: usize = 512 * 1024;
const MAX_DIFF_FILE_BYTES: usize = 256 * 1024;
const MAX_DIFF_TOTAL_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ApplyPatchArgs {
    /// Complete patch enclosed by *** Begin Patch and *** End Patch.
    #[serde(rename = "patchText", alias = "patch")]
    #[schemars(rename = "patchText")]
    pub patch_text: String,
}

pub struct ApplyPatchTool {
    pub working_directory: PathBuf,
}

#[derive(Debug)]
enum PatchOperation {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug)]
struct UpdateChunk {
    anchor: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    context_positions: Vec<(usize, usize)>,
    end_of_file: bool,
    patch_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Add,
    Update,
    Delete,
    Move,
}

struct PlannedChange {
    kind: ChangeKind,
    source: Option<PathBuf>,
    destination: Option<PathBuf>,
    source_label: Option<String>,
    destination_label: Option<String>,
    before: Vec<u8>,
    after: Vec<u8>,
    source_identity: Option<FileIdentity>,
    permissions: Option<std::fs::Permissions>,
    staged: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstalledFile {
    identity: FileIdentity,
    contents: Vec<u8>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    group: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    attributes: u32,
    size: u64,
    modified: u64,
    volume: Option<u32>,
    index: Option<u64>,
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    size: u64,
    modified: Option<std::time::SystemTime>,
    readonly: bool,
}

enum JournalEntry {
    Created {
        path: PathBuf,
        installed: InstalledFile,
    },
    Replaced {
        path: PathBuf,
        backup: PathBuf,
        installed: Option<InstalledFile>,
    },
    Removed {
        path: PathBuf,
        backup: PathBuf,
    },
    Moved {
        source: PathBuf,
        backup: PathBuf,
        destination: PathBuf,
        installed: InstalledFile,
    },
}

#[derive(Clone, Default)]
struct CommitOptions {
    #[cfg(test)]
    fail_after: Option<usize>,
    #[cfg(test)]
    pause: Option<CommitPause>,
    #[cfg(test)]
    completed: Option<std::sync::Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    fault: Option<CommitFault>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitPhase {
    BackupCreated,
    DestinationInstalled,
    StageRemoved,
    PermissionsRestored,
}

#[cfg(test)]
#[derive(Clone)]
struct CommitFault {
    change: usize,
    phase: CommitPhase,
}

#[cfg(test)]
#[derive(Clone)]
struct CommitPause {
    after_change: usize,
    reached: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        include_str!("prompts/apply_patch.txt")
    }

    fn schema(&self) -> RootSchema {
        schemars::schema_for!(ApplyPatchArgs)
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let args: ApplyPatchArgs = serde_json::from_value(args).map_err(into_tool_error)?;
        let operations = parse_patch(&args.patch_text).map_err(into_tool_error)?;
        let working_directory = ctx.active_working_directory(&self.working_directory);
        let root = canonicalize_root(working_directory)
            .await
            .map_err(into_tool_error)?;
        let changes = build_plan(&root, operations, ctx.enforcement)
            .await
            .map_err(into_tool_error)?;
        let (output, metadata) = describe_changes(&changes);

        run_commit_task(root, changes, ctx.clone(), CommitOptions::default())
            .await
            .map_err(into_tool_error)?;

        let mut result = ToolResult::new("apply_patch", output);
        result.metadata = metadata;
        Ok(result)
    }
}

async fn run_commit_task(
    root: PathBuf,
    mut changes: Vec<PlannedChange>,
    ctx: ToolContext,
    options: CommitOptions,
) -> Result<()> {
    #[cfg(test)]
    let completed = options.completed.clone();
    tokio::spawn(async move {
        let result = commit_changes(&root, &mut changes, &ctx, options).await;
        #[cfg(test)]
        if let Some(completed) = completed {
            completed.notify_one();
        }
        result
    })
    .await
    .context("apply_patch transaction task failed")?
}

fn parse_patch(input: &str) -> Result<Vec<PatchOperation>> {
    anyhow::ensure!(!input.contains('\0'), "Patch contains a NUL byte");
    anyhow::ensure!(
        input.len() <= MAX_PATCH_BYTES,
        "Patch is too large: {} bytes exceeds the {} byte limit",
        input.len(),
        MAX_PATCH_BYTES
    );
    let input = strip_heredoc(input.trim())?;
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim_matches('\n');
    let line_count = normalized.lines().count();
    anyhow::ensure!(
        line_count <= MAX_PATCH_LINES,
        "Patch has too many lines: {line_count} exceeds the {MAX_PATCH_LINES} line limit"
    );
    let lines = normalized.split('\n').collect::<Vec<_>>();

    anyhow::ensure!(
        lines.first().map(|line| line.trim()) == Some("*** Begin Patch"),
        "Invalid patch format: first line must be *** Begin Patch"
    );
    anyhow::ensure!(
        lines.last().map(|line| line.trim()) == Some("*** End Patch"),
        "Invalid patch format: last line must be *** End Patch"
    );
    anyhow::ensure!(lines.len() > 2, "Patch rejected: empty patch");

    let end = lines.len() - 1;
    let mut index = 1;
    let mut operations = Vec::new();
    let mut chunk_count: usize = 0;
    let mut change_line_count: usize = 0;

    while index < end {
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }

        if let Some(path) = lines[index].strip_prefix("*** Add File:") {
            let path = parse_path(path, index + 1)?;
            index += 1;
            let mut content = Vec::new();
            while index < end && !is_file_header(lines[index]) {
                if lines[index].is_empty() && index + 1 < end && is_file_header(lines[index + 1]) {
                    index += 1;
                    break;
                }
                anyhow::ensure!(
                    lines[index].starts_with('+'),
                    "Invalid add-file line {}: every content line must start with +",
                    index + 1
                );
                content.push(lines[index][1..].to_string());
                index += 1;
            }
            operations.push(PatchOperation::Add {
                path,
                lines: content,
            });
            ensure_operation_limit(operations.len())?;
            continue;
        }

        if let Some(path) = lines[index].strip_prefix("*** Delete File:") {
            operations.push(PatchOperation::Delete {
                path: parse_path(path, index + 1)?,
            });
            ensure_operation_limit(operations.len())?;
            index += 1;
            continue;
        }

        if let Some(path) = lines[index].strip_prefix("*** Update File:") {
            let path = parse_path(path, index + 1)?;
            index += 1;
            let move_to = if index < end {
                if let Some(destination) = lines[index].strip_prefix("*** Move to:") {
                    index += 1;
                    Some(parse_path(destination, index)?)
                } else {
                    None
                }
            } else {
                None
            };
            let mut chunks = Vec::new();

            while index < end && !is_file_header(lines[index]) {
                if lines[index].is_empty() && index + 1 < end && is_file_header(lines[index + 1]) {
                    index += 1;
                    break;
                }

                let header = lines[index];
                anyhow::ensure!(
                    header == "@@" || header.starts_with("@@ "),
                    "Invalid update directive at line {}: expected @@ chunk header",
                    index + 1
                );
                let anchor = header
                    .strip_prefix("@@ ")
                    .map(str::trim)
                    .filter(|anchor| !anchor.is_empty())
                    .map(str::to_string);
                let patch_line = index + 1;
                index += 1;
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut context_positions = Vec::new();
                let mut changed = false;
                let mut end_of_file = false;

                while index < end
                    && !is_file_header(lines[index])
                    && lines[index] != "@@"
                    && !lines[index].starts_with("@@ ")
                {
                    if lines[index] == "*** End of File" {
                        end_of_file = true;
                        index += 1;
                        break;
                    }

                    let line = lines[index];
                    let (prefix, content) = line.split_at_checked(1).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Invalid update line {}: expected space, +, or - prefix",
                            index + 1
                        )
                    })?;
                    match prefix {
                        " " => {
                            context_positions.push((new_lines.len(), old_lines.len()));
                            old_lines.push(content.to_string());
                            new_lines.push(content.to_string());
                        }
                        "-" => {
                            old_lines.push(content.to_string());
                            changed = true;
                        }
                        "+" => {
                            new_lines.push(content.to_string());
                            changed = true;
                        }
                        _ => anyhow::bail!(
                            "Invalid update line {}: expected space, +, or - prefix",
                            index + 1
                        ),
                    }
                    index += 1;
                }

                anyhow::ensure!(
                    changed,
                    "Update chunk at line {} does not change any lines",
                    patch_line
                );
                chunk_count += 1;
                anyhow::ensure!(
                    chunk_count <= MAX_CHUNKS,
                    "Patch has too many update chunks: {chunk_count} exceeds the {MAX_CHUNKS} chunk limit"
                );
                change_line_count = change_line_count
                    .checked_add(old_lines.len() + new_lines.len())
                    .context("Patch change-line count overflowed")?;
                anyhow::ensure!(
                    change_line_count <= MAX_CHANGE_LINES,
                    "Patch changes too many lines: {change_line_count} exceeds the {MAX_CHANGE_LINES} line limit"
                );
                chunks.push(UpdateChunk {
                    anchor,
                    old_lines,
                    new_lines,
                    context_positions,
                    end_of_file,
                    patch_line,
                });
            }

            anyhow::ensure!(
                !chunks.is_empty() || move_to.is_some(),
                "Update for {path} must contain a chunk or Move to directive"
            );
            operations.push(PatchOperation::Update {
                path,
                move_to,
                chunks,
            });
            ensure_operation_limit(operations.len())?;
            continue;
        }

        anyhow::bail!(
            "Unknown patch directive at line {}: {}",
            index + 1,
            lines[index]
        );
    }

    anyhow::ensure!(
        !operations.is_empty(),
        "Patch rejected: no file operations found"
    );
    Ok(operations)
}

fn ensure_operation_limit(count: usize) -> Result<()> {
    anyhow::ensure!(
        count <= MAX_OPERATIONS,
        "Patch has too many file operations: {count} exceeds the {MAX_OPERATIONS} operation limit"
    );
    Ok(())
}

fn strip_heredoc(input: &str) -> Result<&str> {
    let Some((header, rest)) = input.split_once('\n') else {
        return Ok(input);
    };
    let header = header.trim();
    let header = header.strip_prefix("cat ").unwrap_or(header).trim();
    let Some(delimiter) = header.strip_prefix("<<") else {
        return Ok(input);
    };
    let delimiter = delimiter.trim();
    let delimiter = match delimiter.as_bytes().first().copied() {
        Some(quote @ (b'\'' | b'"')) => {
            anyhow::ensure!(
                delimiter.as_bytes().last().copied() == Some(quote) && delimiter.len() >= 2,
                "Invalid heredoc wrapper: delimiter quotes do not match"
            );
            &delimiter[1..delimiter.len() - 1]
        }
        _ => {
            anyhow::ensure!(
                !delimiter.ends_with('\'') && !delimiter.ends_with('"'),
                "Invalid heredoc wrapper: delimiter quotes do not match"
            );
            delimiter
        }
    };
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Ok(input);
    }
    let Some((body, footer)) = rest.rsplit_once('\n') else {
        return Ok(input);
    };
    anyhow::ensure!(
        footer.trim() == delimiter,
        "Invalid heredoc wrapper: closing delimiter does not match {delimiter}"
    );
    Ok(body)
}

fn parse_path(path: &str, line: usize) -> Result<String> {
    let path = path.trim();
    anyhow::ensure!(!path.is_empty(), "Patch path is empty at line {line}");
    anyhow::ensure!(
        !path.contains('\0'),
        "Patch path contains a NUL byte at line {line}"
    );
    Ok(path.to_string())
}

fn is_file_header(line: &str) -> bool {
    line.starts_with("*** Add File:")
        || line.starts_with("*** Delete File:")
        || line.starts_with("*** Update File:")
        || line == "*** End Patch"
}

async fn build_plan(
    root: &Path,
    operations: Vec<PatchOperation>,
    enforcement: PathEnforcement,
) -> Result<Vec<PlannedChange>> {
    let mut sources = HashSet::new();
    let mut destinations = HashSet::new();
    let mut changes = Vec::with_capacity(operations.len());
    let mut transaction_bytes = 0;

    for operation in operations {
        match operation {
            PatchOperation::Add { path, lines } => {
                let destination = resolve_write_path(root, &path, enforcement).await?;
                anyhow::ensure!(
                    destinations.insert(destination.clone()),
                    "Duplicate patch destination: {path}"
                );
                anyhow::ensure!(
                    path_is_missing(&destination).await?,
                    "Add destination already exists: {path}"
                );
                let mut content = lines.join("\n").into_bytes();
                if !content.is_empty() {
                    content.push(b'\n');
                }
                anyhow::ensure!(
                    content.len() as u64 <= MAX_FILE_BYTES,
                    "Added file is too large: {path} exceeds the {MAX_FILE_BYTES} byte limit"
                );
                push_planned_change(
                    &mut changes,
                    &mut transaction_bytes,
                    PlannedChange {
                        kind: ChangeKind::Add,
                        source: None,
                        destination: Some(destination),
                        source_label: None,
                        destination_label: Some(path),
                        before: Vec::new(),
                        after: content,
                        source_identity: None,
                        permissions: None,
                        staged: None,
                    },
                )?;
            }
            PatchOperation::Delete { path } => {
                let source = resolve_read_path(root, &path, enforcement).await?;
                anyhow::ensure!(
                    sources.insert(source.clone()),
                    "Duplicate patch source: {path}"
                );
                let (metadata, before) = read_source_snapshot(&source, &path).await?;
                push_planned_change(
                    &mut changes,
                    &mut transaction_bytes,
                    PlannedChange {
                        kind: ChangeKind::Delete,
                        source: Some(source),
                        destination: None,
                        source_label: Some(path),
                        destination_label: None,
                        before,
                        after: Vec::new(),
                        source_identity: Some(file_identity(&metadata)),
                        permissions: None,
                        staged: None,
                    },
                )?;
            }
            PatchOperation::Update {
                path,
                move_to,
                chunks,
            } => {
                let source = resolve_read_path(root, &path, enforcement).await?;
                anyhow::ensure!(
                    sources.insert(source.clone()),
                    "Duplicate patch source: {path}"
                );
                let (metadata, before) = read_source_snapshot(&source, &path).await?;
                let text = String::from_utf8(before.clone())?;
                let after = apply_chunks(&text, &chunks, &path)?.into_bytes();

                let (kind, destination, destination_label) = if let Some(move_to) = move_to {
                    let destination = resolve_write_path(root, &move_to, enforcement).await?;
                    anyhow::ensure!(
                        destination != source,
                        "Move destination must differ from source: {path}"
                    );
                    anyhow::ensure!(
                        path_is_missing(&destination).await?,
                        "Move destination already exists: {move_to}"
                    );
                    (ChangeKind::Move, destination, move_to)
                } else {
                    anyhow::ensure!(before != after, "Update produces no changes: {path}");
                    (ChangeKind::Update, source.clone(), path.clone())
                };

                anyhow::ensure!(
                    destinations.insert(destination.clone()),
                    "Duplicate patch destination: {destination_label}"
                );
                push_planned_change(
                    &mut changes,
                    &mut transaction_bytes,
                    PlannedChange {
                        kind,
                        source: Some(source),
                        destination: Some(destination),
                        source_label: Some(path),
                        destination_label: Some(destination_label),
                        before,
                        after,
                        source_identity: Some(file_identity(&metadata)),
                        permissions: Some(metadata.permissions()),
                        staged: None,
                    },
                )?;
            }
        }
    }

    for change in &changes {
        if let Some(destination) = &change.destination
            && change.source.as_ref() != Some(destination)
        {
            anyhow::ensure!(
                !sources.contains(destination),
                "Patch destination conflicts with another source: {:?}",
                destination
            );
        }
    }

    Ok(changes)
}

fn push_planned_change(
    changes: &mut Vec<PlannedChange>,
    transaction_bytes: &mut usize,
    change: PlannedChange,
) -> Result<()> {
    if change.destination.is_some() {
        anyhow::ensure!(
            change.after.len() as u64 <= MAX_FILE_BYTES,
            "Patched file is too large: {} bytes exceeds the {} byte limit",
            change.after.len(),
            MAX_FILE_BYTES
        );
    }
    let bytes = change
        .before
        .len()
        .checked_add(change.after.len())
        .context("Patch transaction size overflowed")?;
    *transaction_bytes = checked_transaction_size(*transaction_bytes, bytes)?;
    changes.push(change);
    Ok(())
}

fn checked_transaction_size(current: usize, additional: usize) -> Result<usize> {
    let total = current
        .checked_add(additional)
        .context("Patch transaction size overflowed")?;
    anyhow::ensure!(
        total <= MAX_TRANSACTION_BYTES,
        "Patch transaction is too large: {total} bytes exceeds the {MAX_TRANSACTION_BYTES} byte limit"
    );
    Ok(total)
}

async fn validate_source(path: &Path, label: &str) -> Result<std::fs::Metadata> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("Source does not exist: {label}"))?;
    anyhow::ensure!(
        metadata.is_file(),
        "Patch source is not a regular file: {label}"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_FILE_BYTES,
        "Patch source is too large: {label} is {} bytes and exceeds the {} byte limit",
        metadata.len(),
        MAX_FILE_BYTES
    );
    Ok(metadata)
}

async fn read_text_bytes(path: &Path, label: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("Failed to read patch source: {label}"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "Patch source is too large: {label} is {} bytes and exceeds the {} byte limit",
        bytes.len(),
        MAX_FILE_BYTES
    );
    anyhow::ensure!(
        !bytes.contains(&0),
        "Patch source appears to be binary: {label}"
    );
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("Patch source is not valid UTF-8: {label}"))?;
    validate_text_lines(text, label)?;
    Ok(bytes)
}

async fn read_source_snapshot(path: &Path, label: &str) -> Result<(std::fs::Metadata, Vec<u8>)> {
    let before = validate_source(path, label).await?;
    let bytes = read_text_bytes(path, label).await?;
    let after = validate_source(path, label).await?;
    anyhow::ensure!(
        file_identity(&before) == file_identity(&after),
        "Patch source changed while it was being read: {label}"
    );
    Ok((after, bytes))
}

fn validate_text_lines(input: &str, label: &str) -> Result<&'static str> {
    let ending = detect_line_ending(input)?;
    let final_newline = input.ends_with(ending);
    let line_count =
        input.matches(ending).count() + usize::from(!input.is_empty() && !final_newline);
    anyhow::ensure!(
        line_count <= MAX_FILE_LINES,
        "Patch source has too many lines: {label} has {line_count} lines and exceeds the {MAX_FILE_LINES} line limit"
    );
    Ok(ending)
}

async fn path_is_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path).await {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect {:?}", path)),
    }
}

async fn installed_file(path: &Path) -> Result<InstalledFile> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("Failed to inspect installed patch file: {:?}", path))?;
    anyhow::ensure!(
        metadata.is_file(),
        "Installed patch path is not a regular file: {:?}",
        path
    );
    let contents = fs::read(path)
        .await
        .with_context(|| format!("Failed to read installed patch file: {:?}", path))?;
    Ok(InstalledFile {
        identity: file_identity(&metadata),
        contents,
    })
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;

    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        group: metadata.gid(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    }
}

#[cfg(windows)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::windows::fs::MetadataExt;

    FileIdentity {
        attributes: metadata.file_attributes(),
        size: metadata.file_size(),
        modified: metadata.last_write_time(),
        volume: metadata.volume_serial_number(),
        index: metadata.file_index(),
    }
}

#[cfg(not(any(unix, windows)))]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        readonly: metadata.permissions().readonly(),
    }
}

async fn remove_installed_file(path: &Path, expected: &InstalledFile) -> Result<()> {
    match fs::symlink_metadata(path).await {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("Failed to inspect {:?}", path)),
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "Refusing to remove changed rollback path: {:?}",
            path
        ),
    }

    let current = installed_file(path).await?;
    anyhow::ensure!(
        current == *expected,
        "Refusing to remove {:?}: it changed after apply_patch installed it",
        path
    );
    remove_if_exists(path).await?;
    Ok(())
}

struct TextLines {
    lines: Vec<String>,
    ending: &'static str,
    final_newline: bool,
}

impl TextLines {
    fn parse(input: &str) -> Result<Self> {
        let ending = validate_text_lines(input, "source")?;
        let final_newline = input.ends_with(ending);
        let normalized = match ending {
            "\r\n" => input.replace("\r\n", "\n"),
            "\r" => input.replace('\r', "\n"),
            _ => input.to_string(),
        };
        let mut lines = if normalized.is_empty() {
            Vec::new()
        } else {
            normalized.split('\n').map(str::to_string).collect()
        };
        if final_newline && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Ok(Self {
            lines,
            ending,
            final_newline,
        })
    }

    fn render(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let mut output = self.lines.join(self.ending);
        if self.final_newline {
            output.push_str(self.ending);
        }
        output
    }
}

fn detect_line_ending(input: &str) -> Result<&'static str> {
    let bytes = input.as_bytes();
    let mut ending = None;
    let mut index = 0;
    while index < bytes.len() {
        let current = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                index += 2;
                Some("\r\n")
            }
            b'\r' => {
                index += 1;
                Some("\r")
            }
            b'\n' => {
                index += 1;
                Some("\n")
            }
            _ => {
                index += 1;
                None
            }
        };
        if let Some(current) = current {
            if let Some(previous) = ending {
                anyhow::ensure!(
                    previous == current,
                    "Mixed line endings are not supported by apply_patch"
                );
            } else {
                ending = Some(current);
            }
        }
    }
    Ok(ending.unwrap_or("\n"))
}

fn apply_chunks(input: &str, chunks: &[UpdateChunk], label: &str) -> Result<String> {
    if chunks.is_empty() {
        return Ok(input.to_string());
    }

    let mut text = TextLines::parse(input)?;
    let mut cursor = 0;

    for chunk in chunks {
        let start = if let Some(anchor) = &chunk.anchor {
            seek_unique(&text.lines, std::slice::from_ref(anchor), cursor, false).with_context(
                || {
                    format!(
                        "Failed to find context {:?} in {label} for patch line {}",
                        anchor, chunk.patch_line
                    )
                },
            )? + 1
        } else {
            cursor
        };

        let position = if chunk.old_lines.is_empty() {
            if chunk.end_of_file || chunk.anchor.is_none() {
                text.lines.len()
            } else {
                start
            }
        } else {
            seek_unique(&text.lines, &chunk.old_lines, start, chunk.end_of_file).with_context(
                || {
                    format!(
                        "Failed to find expected lines in {label} for patch line {}:\n{}",
                        chunk.patch_line,
                        chunk.old_lines.join("\n")
                    )
                },
            )?
        };

        let mut replacement = chunk.new_lines.clone();
        for (new_index, old_index) in &chunk.context_positions {
            replacement[*new_index] = text.lines[position + old_index].clone();
        }
        text.lines
            .splice(position..position + chunk.old_lines.len(), replacement);
        anyhow::ensure!(
            text.lines.len() <= MAX_FILE_LINES,
            "Patched file has too many lines: {} exceeds the {} line limit",
            text.lines.len(),
            MAX_FILE_LINES
        );
        cursor = position + chunk.new_lines.len();
    }

    Ok(text.render())
}

fn seek_unique(
    lines: &[String],
    pattern: &[String],
    start: usize,
    end_of_file: bool,
) -> Result<usize> {
    anyhow::ensure!(
        !pattern.is_empty(),
        "Cannot search for an empty line sequence"
    );
    anyhow::ensure!(
        pattern.len() <= lines.len(),
        "Line sequence is longer than the file"
    );

    type Normalize = fn(&str) -> String;
    let passes: [Normalize; 4] = [
        |line| line.to_string(),
        |line| line.trim_end().to_string(),
        |line| line.trim().to_string(),
        normalize_unicode,
    ];

    for normalize in passes {
        let expected = pattern
            .iter()
            .map(|line| normalize(line))
            .collect::<Vec<_>>();
        let candidates = if end_of_file {
            let position = lines.len() - pattern.len();
            if position >= start {
                vec![position]
            } else {
                Vec::new()
            }
        } else {
            (start..=lines.len() - pattern.len()).collect::<Vec<_>>()
        };
        let matches = candidates
            .into_iter()
            .filter(|position| {
                lines[*position..*position + pattern.len()]
                    .iter()
                    .map(|line| normalize(line))
                    .eq(expected.iter().cloned())
            })
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [position] => return Ok(*position),
            [] => {}
            _ => anyhow::bail!("Patch context is ambiguous; add a more specific @@ anchor"),
        }
    }

    anyhow::bail!("Line sequence was not found")
}

fn normalize_unicode(line: &str) -> String {
    line.trim()
        .replace(['‘', '’'], "'")
        .replace(['“', '”'], "\"")
        .replace(['‐', '‑', '‒', '–', '—', '―'], "-")
        .replace('…', "...")
        .replace('\u{00a0}', " ")
}

fn describe_changes(
    changes: &[PlannedChange],
) -> (String, std::collections::HashMap<String, Value>) {
    let mut summaries = Vec::new();
    let mut files = Vec::new();
    let mut combined_diff = String::new();
    let mut combined_diff_truncated = false;

    for change in changes {
        let source = change.source_label.as_deref().unwrap_or("/dev/null");
        let destination = change.destination_label.as_deref().unwrap_or("/dev/null");
        let diff_input_bytes = change.before.len().saturating_add(change.after.len());
        let (additions, deletions, mut unified, mut diff_omitted) = if diff_input_bytes
            <= MAX_DIFF_INPUT_BYTES
        {
            let before = String::from_utf8_lossy(&change.before);
            let after = String::from_utf8_lossy(&change.after);
            let diff = TextDiff::from_lines(&before, &after);
            let additions = diff
                .iter_all_changes()
                .filter(|change| change.tag() == ChangeTag::Insert)
                .count();
            let deletions = diff
                .iter_all_changes()
                .filter(|change| change.tag() == ChangeTag::Delete)
                .count();
            (
                Some(additions),
                Some(deletions),
                diff.unified_diff().header(source, destination).to_string(),
                false,
            )
        } else {
            (
                None,
                None,
                format!(
                    "Diff omitted for {destination}: {diff_input_bytes} input bytes exceeds the {MAX_DIFF_INPUT_BYTES} byte diff limit\n"
                ),
                true,
            )
        };
        if unified.is_empty() {
            unified = match change.kind {
                ChangeKind::Move => format!("rename from {source}\nrename to {destination}\n"),
                ChangeKind::Add => format!("new file {destination}\n"),
                ChangeKind::Delete => format!("deleted file {source}\n"),
                _ => unified,
            };
        }
        if unified.len() > MAX_DIFF_FILE_BYTES {
            unified = format!(
                "Diff omitted for {destination}: generated diff exceeds the {MAX_DIFF_FILE_BYTES} byte per-file limit\n"
            );
            diff_omitted = true;
        }
        if !combined_diff_truncated {
            let separator_bytes = usize::from(!combined_diff.is_empty() && !unified.is_empty());
            if combined_diff
                .len()
                .saturating_add(separator_bytes)
                .saturating_add(unified.len())
                <= MAX_DIFF_TOTAL_BYTES
            {
                if separator_bytes == 1 {
                    combined_diff.push('\n');
                }
                combined_diff.push_str(&unified);
            } else {
                combined_diff = format!(
                    "Combined diff omitted: exceeds the {MAX_DIFF_TOTAL_BYTES} byte metadata limit.\n"
                );
                combined_diff_truncated = true;
            }
        }

        let summary = match change.kind {
            ChangeKind::Add => format!("A {destination}"),
            ChangeKind::Update => format!("M {destination}"),
            ChangeKind::Delete => format!("D {source}"),
            ChangeKind::Move => format!("R {source} -> {destination}"),
        };
        summaries.push(summary);
        files.push(json!({
            "operation": match change.kind {
                ChangeKind::Add => "add",
                ChangeKind::Update => "update",
                ChangeKind::Delete => "delete",
                ChangeKind::Move => "move",
            },
            "source": change.source_label,
            "destination": change.destination_label,
            "additions": additions,
            "deletions": deletions,
            "diff": unified,
            "diff_omitted": diff_omitted,
        }));
    }

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("files".to_string(), Value::Array(files));
    metadata.insert("diff".to_string(), Value::String(combined_diff));
    metadata.insert(
        "diff_truncated".to_string(),
        Value::Bool(combined_diff_truncated),
    );
    (
        format!(
            "Success. Updated the following files:\n{}",
            summaries.join("\n")
        ),
        metadata,
    )
}

async fn commit_changes(
    root: &Path,
    changes: &mut [PlannedChange],
    ctx: &ToolContext,
    _options: CommitOptions,
) -> Result<()> {
    anyhow::ensure!(!ctx.is_aborted(), "Patch execution was aborted");
    validate_current_state(root, changes, ctx.enforcement).await?;
    anyhow::ensure!(!ctx.is_aborted(), "Patch execution was aborted");

    let mut created_dirs = Vec::new();
    if let Err(error) = stage_changes(root, changes, &mut created_dirs, ctx.enforcement).await {
        cleanup_staged(changes).await;
        cleanup_directories(&created_dirs).await;
        return Err(error);
    }
    if ctx.is_aborted() {
        cleanup_staged(changes).await;
        cleanup_directories(&created_dirs).await;
        anyhow::bail!("Patch execution was aborted");
    }

    let mut journal = Vec::new();
    for (index, change) in changes.iter_mut().enumerate() {
        let result = async {
            anyhow::ensure!(!ctx.is_aborted(), "Patch execution was aborted");
            validate_change_current(root, change, ctx.enforcement).await?;
            anyhow::ensure!(!ctx.is_aborted(), "Patch execution was aborted");
            #[cfg(test)]
            if _options.fail_after == Some(index) {
                anyhow::bail!("Injected patch commit failure");
            }
            commit_change(change, &mut journal, &_options, index).await?;
            #[cfg(test)]
            if let Some(pause) = &_options.pause
                && pause.after_change == index
            {
                pause.reached.notify_one();
                pause.resume.notified().await;
            }
            anyhow::ensure!(!ctx.is_aborted(), "Patch execution was aborted");
            Ok(())
        }
        .await;

        if let Err(error) = result {
            let rollback = rollback_changes(&mut journal).await;
            cleanup_staged(changes).await;
            cleanup_directories(&created_dirs).await;
            if let Err(rollback_error) = rollback {
                return Err(error).context(format!("Rollback also failed: {rollback_error:#}"));
            }
            return Err(error);
        }
    }

    if ctx.is_aborted() {
        let rollback = rollback_changes(&mut journal).await;
        cleanup_staged(changes).await;
        cleanup_directories(&created_dirs).await;
        rollback.context("Patch execution was aborted and rollback failed")?;
        anyhow::bail!("Patch execution was aborted");
    }

    cleanup_backups(&journal).await;
    cleanup_directories(&created_dirs).await;
    Ok(())
}

async fn validate_current_state(
    root: &Path,
    changes: &[PlannedChange],
    enforcement: PathEnforcement,
) -> Result<()> {
    for change in changes {
        validate_change_current(root, change, enforcement).await?;
    }
    Ok(())
}

async fn validate_change_current(
    root: &Path,
    change: &PlannedChange,
    enforcement: PathEnforcement,
) -> Result<()> {
    if let Some(source) = &change.source {
        let label = change.source_label.as_deref().unwrap_or("source");
        if enforcement != PathEnforcement::None {
            let resolved = resolve_safe_existing_path(root, source).await?;
            anyhow::ensure!(
                &resolved == source,
                "Patch source changed location: {label}"
            );
        }
        let (metadata, current) = read_source_snapshot(source, label).await?;
        if let Some(expected) = &change.source_identity {
            anyhow::ensure!(
                file_identity(&metadata) == *expected,
                "Patch source identity or metadata changed after validation: {label}"
            );
        }
        anyhow::ensure!(
            current == change.before,
            "Patch source changed after validation: {label}"
        );
    }

    if matches!(change.kind, ChangeKind::Add | ChangeKind::Move) {
        let destination = change.destination.as_ref().expect("destination");
        let label = change.destination_label.as_deref().unwrap_or("destination");
        if enforcement != PathEnforcement::None {
            let resolved = resolve_safe_write_path(root, destination).await?;
            anyhow::ensure!(
                &resolved == destination,
                "Patch destination changed location: {label}"
            );
        }
        anyhow::ensure!(
            path_is_missing(destination).await?,
            "Patch destination was created after validation: {label}"
        );
    }
    Ok(())
}

async fn stage_changes(
    root: &Path,
    changes: &mut [PlannedChange],
    created_dirs: &mut Vec<PathBuf>,
    enforcement: PathEnforcement,
) -> Result<()> {
    for change in changes {
        if !matches!(
            change.kind,
            ChangeKind::Add | ChangeKind::Update | ChangeKind::Move
        ) {
            continue;
        }
        let destination = change.destination.as_ref().expect("destination");
        create_parent_directories(root, destination, created_dirs, enforcement).await?;
        if enforcement != PathEnforcement::None {
            resolve_safe_write_path(root, destination).await?;
        }
        let staged = create_temp_file(
            destination,
            "stage",
            &change.after,
            change.permissions.as_ref(),
        )
        .await?;
        change.staged = Some(staged.clone());
    }
    Ok(())
}

async fn create_parent_directories(
    root: &Path,
    path: &Path,
    created: &mut Vec<PathBuf>,
    enforcement: PathEnforcement,
) -> Result<()> {
    let mut missing = Vec::new();
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if fs::symlink_metadata(directory).await.is_ok() {
            break;
        }
        missing.push(directory.to_path_buf());
        parent = directory.parent();
    }

    for directory in missing.into_iter().rev() {
        if enforcement != PathEnforcement::None {
            resolve_safe_write_path(root, &directory).await?;
        }
        match fs::create_dir(&directory).await {
            Ok(()) => created.push(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("Failed to create patch destination directory");
            }
        }
    }
    Ok(())
}

async fn create_temp_file(
    destination: &Path,
    purpose: &str,
    contents: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .context("Patch destination has no parent")?;
    for _ in 0..100 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".gewu-{purpose}-{}-{id}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                if let Some(permissions) = permissions {
                    #[cfg(windows)]
                    let mut staged_permissions = permissions.clone();
                    #[cfg(windows)]
                    staged_permissions.set_readonly(false);
                    #[cfg(not(windows))]
                    let staged_permissions = permissions.clone();
                    if let Err(error) = fs::set_permissions(&path, staged_permissions).await {
                        drop(file);
                        let _ = remove_if_exists(&path).await;
                        return Err(error).with_context(|| {
                            format!("Failed to secure patch staging file: {:?}", path)
                        });
                    }
                }
                let result = async {
                    file.write_all(contents).await?;
                    file.flush().await?;
                    file.sync_all().await?;
                    Ok::<_, std::io::Error>(())
                }
                .await;
                drop(file);
                if let Err(error) = result {
                    let _ = remove_if_exists(&path).await;
                    return Err(error).context("Failed to write patch staging file");
                }
                return Ok(path);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("Failed to create patch staging file"),
        }
    }
    anyhow::bail!("Unable to create a unique patch staging file")
}

async fn unused_backup_path(source: &Path) -> Result<PathBuf> {
    let parent = source.parent().context("Patch source has no parent")?;
    for _ in 0..100 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".gewu-backup-{}-{id}", std::process::id()));
        if path_is_missing(&path).await? {
            return Ok(path);
        }
    }
    anyhow::bail!("Unable to create a unique patch backup path")
}

async fn commit_change(
    change: &mut PlannedChange,
    journal: &mut Vec<JournalEntry>,
    options: &CommitOptions,
    index: usize,
) -> Result<()> {
    match change.kind {
        ChangeKind::Add => {
            let destination = change.destination.as_ref().expect("destination").clone();
            let staged = change.staged.as_ref().expect("staged patch").clone();
            let staged_snapshot = installed_file(&staged).await?;
            fs::hard_link(&staged, &destination)
                .await
                .context("Failed to install added file")?;
            journal.push(JournalEntry::Created {
                path: destination,
                installed: staged_snapshot,
            });
            inject_commit_failure(options, index, CommitPhase::DestinationInstalled)?;
            remove_if_exists(&staged).await?;
            change.staged = None;
            inject_commit_failure(options, index, CommitPhase::StageRemoved)?;
        }
        ChangeKind::Update => {
            let source = change.source.as_ref().expect("source").clone();
            let staged = change.staged.as_ref().expect("staged patch").clone();
            let staged_snapshot = installed_file(&staged).await?;
            let backup = unused_backup_path(&source).await?;
            fs::rename(&source, &backup).await?;
            journal.push(JournalEntry::Replaced {
                path: source.clone(),
                backup,
                installed: None,
            });
            inject_commit_failure(options, index, CommitPhase::BackupCreated)?;
            fs::hard_link(&staged, &source)
                .await
                .context("Failed to install updated file")?;
            if let Some(JournalEntry::Replaced { installed, .. }) = journal.last_mut() {
                *installed = Some(staged_snapshot);
            }
            inject_commit_failure(options, index, CommitPhase::DestinationInstalled)?;
            remove_if_exists(&staged).await?;
            change.staged = None;
            inject_commit_failure(options, index, CommitPhase::StageRemoved)?;
            if let Some(permissions) = &change.permissions {
                fs::set_permissions(&source, permissions.clone()).await?;
                if let Some(JournalEntry::Replaced {
                    installed: Some(installed),
                    ..
                }) = journal.last_mut()
                {
                    update_expected_permissions(installed, permissions);
                }
            }
            inject_commit_failure(options, index, CommitPhase::PermissionsRestored)?;
        }
        ChangeKind::Delete => {
            let source = change.source.as_ref().expect("source").clone();
            let backup = unused_backup_path(&source).await?;
            fs::rename(&source, &backup).await?;
            journal.push(JournalEntry::Removed {
                path: source,
                backup,
            });
            inject_commit_failure(options, index, CommitPhase::BackupCreated)?;
        }
        ChangeKind::Move => {
            let source = change.source.as_ref().expect("source").clone();
            let destination = change.destination.as_ref().expect("destination").clone();
            let staged = change.staged.as_ref().expect("staged patch").clone();
            let mut installed = installed_file(&staged).await?;
            fs::hard_link(&staged, &destination)
                .await
                .context("Failed to install moved file")?;
            journal.push(JournalEntry::Created {
                path: destination.clone(),
                installed: installed.clone(),
            });
            inject_commit_failure(options, index, CommitPhase::DestinationInstalled)?;
            remove_if_exists(&staged).await?;
            change.staged = None;
            inject_commit_failure(options, index, CommitPhase::StageRemoved)?;
            if let Some(permissions) = &change.permissions {
                fs::set_permissions(&destination, permissions.clone()).await?;
                update_expected_permissions(&mut installed, permissions);
            }
            if let Some(JournalEntry::Created {
                installed: recorded,
                ..
            }) = journal.last_mut()
            {
                *recorded = installed.clone();
            }
            inject_commit_failure(options, index, CommitPhase::PermissionsRestored)?;
            let backup = unused_backup_path(&source).await?;
            fs::rename(&source, &backup).await?;
            journal.pop();
            journal.push(JournalEntry::Moved {
                source,
                backup,
                destination,
                installed,
            });
            inject_commit_failure(options, index, CommitPhase::BackupCreated)?;
        }
    }
    Ok(())
}

fn update_expected_permissions(installed: &mut InstalledFile, permissions: &std::fs::Permissions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        installed.identity.mode =
            (installed.identity.mode & !0o7777) | (permissions.mode() & 0o7777);
    }
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
        if permissions.readonly() {
            installed.identity.attributes |= FILE_ATTRIBUTE_READONLY;
        } else {
            installed.identity.attributes &= !FILE_ATTRIBUTE_READONLY;
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        installed.identity.readonly = permissions.readonly();
    }
}

fn inject_commit_failure(options: &CommitOptions, index: usize, phase: CommitPhase) -> Result<()> {
    #[cfg(test)]
    if options
        .fault
        .as_ref()
        .is_some_and(|fault| fault.change == index && fault.phase == phase)
    {
        anyhow::bail!("Injected patch commit failure at {phase:?}");
    }
    #[cfg(not(test))]
    let _ = (options, index, phase);
    Ok(())
}

async fn rollback_changes(journal: &mut Vec<JournalEntry>) -> Result<()> {
    let mut failures = Vec::new();
    while let Some(entry) = journal.pop() {
        let result = match entry {
            JournalEntry::Created { path, installed } => {
                async {
                    remove_installed_file(&path, &installed).await?;
                    Ok(())
                }
                .await
            }
            JournalEntry::Replaced {
                path,
                backup,
                installed,
            } => {
                async {
                    if let Some(installed) = installed {
                        remove_installed_file(&path, &installed).await?;
                    } else {
                        anyhow::ensure!(
                            path_is_missing(&path).await?,
                            "Cannot restore {:?}: another file appeared at that path",
                            path
                        );
                    }
                    fs::rename(backup, path).await?;
                    Ok(())
                }
                .await
            }
            JournalEntry::Removed { path, backup } => {
                async {
                    anyhow::ensure!(
                        path_is_missing(&path).await?,
                        "Cannot restore {:?}: another file appeared at that path",
                        path
                    );
                    fs::rename(backup, path).await?;
                    Ok(())
                }
                .await
            }
            JournalEntry::Moved {
                source,
                backup,
                destination,
                installed,
            } => {
                async {
                    remove_installed_file(&destination, &installed).await?;
                    fs::rename(backup, source).await?;
                    Ok(())
                }
                .await
            }
        };
        if let Err(error) = result {
            failures.push(error.to_string());
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

async fn cleanup_backups(journal: &[JournalEntry]) {
    for entry in journal {
        let backup = match entry {
            JournalEntry::Replaced { backup, .. }
            | JournalEntry::Removed { backup, .. }
            | JournalEntry::Moved { backup, .. } => Some(backup),
            JournalEntry::Created { .. } => None,
        };
        if let Some(backup) = backup
            && let Err(error) = remove_if_exists(backup).await
        {
            tracing::warn!(path = ?backup, %error, "failed to remove apply_patch backup");
        }
    }
}

async fn cleanup_staged(changes: &[PlannedChange]) {
    for change in changes {
        if let Some(path) = &change.staged {
            let _ = remove_if_exists(path).await;
        }
    }
}

async fn cleanup_directories(directories: &[PathBuf]) {
    for directory in directories.iter().rev() {
        let _ = fs::remove_dir(directory).await;
    }
}

async fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if let Ok(metadata) = fs::metadata(path).await {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions).await?;
        }
    }
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gewu_core::traits::Tool;
    use tempfile::tempdir;

    async fn run_patch(
        root: &Path,
        patch: &str,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        run_patch_with(root, patch, PathEnforcement::Strict).await
    }

    async fn run_patch_with(
        root: &Path,
        patch: &str,
        enforcement: PathEnforcement,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        ApplyPatchTool {
            working_directory: root.to_path_buf(),
        }
        .execute(
            json!({ "patch": patch }),
            &ToolContext::new("test", "message").with_enforcement(enforcement),
        )
        .await
    }

    fn patch_artifacts(root: &Path) -> Vec<PathBuf> {
        let mut artifacts = Vec::new();
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(".gewu-stage-") || name.starts_with(".gewu-backup-")
                    })
                {
                    artifacts.push(path);
                }
            }
        }
        artifacts
    }

    #[tokio::test]
    async fn accepts_patch_text_and_keeps_patch_alias() {
        let dir = tempdir().unwrap();
        let tool = ApplyPatchTool {
            working_directory: dir.path().to_path_buf(),
        };
        let schema = tool.schema();
        assert!(
            schema
                .schema
                .object
                .as_ref()
                .unwrap()
                .properties
                .contains_key("patchText")
        );

        tool.execute(
            json!({
                "patchText": "*** Begin Patch\n*** Add File: first.txt\n+first\n*** End Patch"
            }),
            &ToolContext::new("test", "message"),
        )
        .await
        .unwrap();
        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: second.txt\n+second\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("first.txt"))
                .await
                .unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("second.txt"))
                .await
                .unwrap(),
            "second\n"
        );
    }

    #[tokio::test]
    async fn accepts_heredoc_wrapped_patches() {
        let dir = tempdir().unwrap();
        for (name, patch) in [
            (
                "cat.txt",
                "cat <<'PATCH'\n*** Begin Patch\n*** Add File: cat.txt\n+cat\n*** End Patch\nPATCH",
            ),
            (
                "plain.txt",
                "<<EOF\n*** Begin Patch\n*** Add File: plain.txt\n+plain\n*** End Patch\nEOF",
            ),
        ] {
            run_patch(dir.path(), patch).await.unwrap();
            assert!(dir.path().join(name).exists());
        }
    }

    #[tokio::test]
    async fn pure_insertion_appends_to_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "first\n")
            .await
            .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: file.txt\n@@\n+second\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt"))
                .await
                .unwrap(),
            "first\nsecond\n"
        );
    }

    #[test]
    fn parses_all_operations() {
        let operations = parse_patch(
            "*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n-old\n+updated\n*** Delete File: gone.txt\n*** End Patch",
        )
        .unwrap();

        assert_eq!(operations.len(), 3);
        assert!(matches!(operations[0], PatchOperation::Add { .. }));
        assert!(matches!(operations[1], PatchOperation::Update { .. }));
        assert!(matches!(operations[2], PatchOperation::Delete { .. }));
    }

    #[test]
    fn rejects_malformed_patch_lines() {
        let error =
            parse_patch("*** Begin Patch\n*** Add File: new.txt\nmissing-prefix\n*** End Patch")
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("every content line must start with +")
        );

        let error = parse_patch("*** Begin Patch\n*** Unknown File: x\n*** End Patch").unwrap_err();
        assert!(error.to_string().contains("Unknown patch directive"));
    }

    #[test]
    fn parser_does_not_panic_on_generated_utf8() {
        let alphabet = [
            'a', '*', '+', '-', '@', ':', ' ', '\n', '\r', '\0', '“', '—', '中', '\'', '"',
        ];
        let mut state = 0x9e37_79b9_u64;
        for _ in 0..1_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let length = (state as usize) % 512;
            let mut input = String::new();
            for _ in 0..length {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                input.push(alphabet[(state as usize) % alphabet.len()]);
            }
            let _ = parse_patch(&input);
        }
    }

    #[test]
    fn rejects_patch_text_over_size_limit() {
        let input = "x".repeat(MAX_PATCH_BYTES + 1);
        let error = parse_patch(&input).unwrap_err();
        assert!(error.to_string().contains("Patch is too large"));
    }

    #[test]
    fn rejects_too_many_file_operations() {
        let mut patch = String::from("*** Begin Patch\n");
        for index in 0..=MAX_OPERATIONS {
            patch.push_str(&format!("*** Delete File: file-{index}.txt\n"));
        }
        patch.push_str("*** End Patch");

        let error = parse_patch(&patch).unwrap_err();
        assert!(error.to_string().contains("too many file operations"));
    }

    #[test]
    fn rejects_transaction_size_over_limit() {
        assert_eq!(
            checked_transaction_size(0, MAX_TRANSACTION_BYTES).unwrap(),
            MAX_TRANSACTION_BYTES
        );
        assert!(checked_transaction_size(MAX_TRANSACTION_BYTES, 1).is_err());
    }

    #[test]
    fn rejects_source_with_too_many_lines() {
        let input = "\n".repeat(MAX_FILE_LINES + 1);
        let error = TextLines::parse(&input).err().unwrap();
        assert!(error.to_string().contains("too many lines"));
    }

    #[test]
    fn omits_large_diff_metadata() {
        let change = PlannedChange {
            kind: ChangeKind::Update,
            source: None,
            destination: None,
            source_label: Some("large.txt".to_string()),
            destination_label: Some("large.txt".to_string()),
            before: vec![b'a'; MAX_DIFF_INPUT_BYTES / 2 + 1],
            after: vec![b'b'; MAX_DIFF_INPUT_BYTES / 2 + 1],
            source_identity: None,
            permissions: None,
            staged: None,
        };

        let (_, metadata) = describe_changes(&[change]);
        let file = &metadata["files"].as_array().unwrap()[0];
        assert_eq!(file["diff_omitted"], true);
        assert!(metadata["diff"].as_str().unwrap().len() < 256);
    }

    #[test]
    fn rejects_ambiguous_update_context() {
        let chunk = UpdateChunk {
            anchor: None,
            old_lines: vec!["same".to_string()],
            new_lines: vec!["changed".to_string()],
            context_positions: Vec::new(),
            end_of_file: false,
            patch_line: 3,
        };
        let error = apply_chunks("same\nother\nsame\n", &[chunk], "file.txt").unwrap_err();
        assert!(format!("{error:#}").contains("ambiguous"));
    }

    #[test]
    fn context_anchor_disambiguates_update() {
        let chunk = UpdateChunk {
            anchor: Some("second".to_string()),
            old_lines: vec!["value".to_string()],
            new_lines: vec!["updated".to_string()],
            context_positions: Vec::new(),
            end_of_file: false,
            patch_line: 3,
        };
        let output = apply_chunks("first\nvalue\nsecond\nvalue\n", &[chunk], "file.txt").unwrap();
        assert_eq!(output, "first\nvalue\nsecond\nupdated\n");
    }

    #[test]
    fn fuzzy_matching_preserves_unchanged_context_text() {
        let chunk = UpdateChunk {
            anchor: None,
            old_lines: vec!["keep".to_string(), "old".to_string()],
            new_lines: vec!["keep".to_string(), "new".to_string()],
            context_positions: vec![(0, 0)],
            end_of_file: false,
            patch_line: 3,
        };
        let output = apply_chunks("  keep\nold\n", &[chunk], "file.txt").unwrap();
        assert_eq!(output, "  keep\nnew\n");
    }

    #[test]
    fn supports_eof_and_unicode_normalized_matching() {
        let chunks = vec![
            UpdateChunk {
                anchor: None,
                old_lines: vec!["quote: \"hello\"".to_string()],
                new_lines: vec!["quote: \"updated\"".to_string()],
                context_positions: Vec::new(),
                end_of_file: false,
                patch_line: 3,
            },
            UpdateChunk {
                anchor: None,
                old_lines: vec!["last".to_string()],
                new_lines: vec!["final".to_string()],
                context_positions: Vec::new(),
                end_of_file: true,
                patch_line: 6,
            },
        ];
        let output = apply_chunks("quote: “hello”\nlast\n", &chunks, "file.txt").unwrap();
        assert_eq!(output, "quote: \"updated\"\nfinal\n");
    }

    #[tokio::test]
    async fn applies_add_update_delete_and_move() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("update.txt"), "before\n")
            .await
            .unwrap();
        fs::write(dir.path().join("delete.txt"), "delete me\n")
            .await
            .unwrap();
        fs::write(dir.path().join("move.txt"), "move me\n")
            .await
            .unwrap();

        let result = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Update File: update.txt\n@@\n-before\n+after\n*** Update File: move.txt\n*** Move to: moved.txt\n@@\n-move me\n+moved and changed\n*** Delete File: delete.txt\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("nested/new.txt"))
                .await
                .unwrap(),
            "created\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("update.txt"))
                .await
                .unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("moved.txt"))
                .await
                .unwrap(),
            "moved and changed\n"
        );
        assert!(!dir.path().join("move.txt").exists());
        assert!(!dir.path().join("delete.txt").exists());
        assert!(result.output.contains("A nested/new.txt"));
        assert!(result.output.contains("R move.txt -> moved.txt"));
        assert!(result.metadata.contains_key("diff"));
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn validation_failure_leaves_all_files_unchanged() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("first.txt"), "first\n")
            .await
            .unwrap();
        fs::write(dir.path().join("second.txt"), "second\n")
            .await
            .unwrap();

        let result = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: first.txt\n@@\n-first\n+changed\n*** Update File: second.txt\n@@\n-missing\n+changed\n*** End Patch",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("first.txt"))
                .await
                .unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("second.txt"))
                .await
                .unwrap(),
            "second\n"
        );
    }

    #[tokio::test]
    async fn blank_context_before_next_file_header_is_required() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("first.txt"), "old\n")
            .await
            .unwrap();

        let result = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: first.txt\n@@\n-old\n+new\n \n*** Add File: second.txt\n+second\n*** End Patch",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("first.txt"))
                .await
                .unwrap(),
            "old\n"
        );
        assert!(!dir.path().join("second.txt").exists());
    }

    #[tokio::test]
    async fn deleting_all_lines_produces_an_empty_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("only.txt"), "only\n")
            .await
            .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: only.txt\n@@\n-only\n*** End Patch",
        )
        .await
        .unwrap();

        assert!(
            fs::read(dir.path().join("only.txt"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn commit_failure_rolls_back_prior_changes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("first.txt"), "first\n")
            .await
            .unwrap();
        fs::write(dir.path().join("second.txt"), "second\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations = parse_patch(
            "*** Begin Patch\n*** Update File: first.txt\n@@\n-first\n+changed first\n*** Update File: second.txt\n@@\n-second\n+changed second\n*** End Patch",
        )
        .unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();

        let error = commit_changes(
            &root,
            &mut changes,
            &ToolContext::new("test", "message"),
            CommitOptions {
                fail_after: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("Injected"));
        assert_eq!(
            fs::read_to_string(dir.path().join("first.txt"))
                .await
                .unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("second.txt"))
                .await
                .unwrap(),
            "second\n"
        );
    }

    #[tokio::test]
    async fn update_rolls_back_at_every_commit_phase() {
        for phase in [
            CommitPhase::BackupCreated,
            CommitPhase::DestinationInstalled,
            CommitPhase::StageRemoved,
            CommitPhase::PermissionsRestored,
        ] {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join("file.txt"), "before\n")
                .await
                .unwrap();
            let root = canonicalize_root(dir.path()).await.unwrap();
            let mut changes = build_plan(
                &root,
                parse_patch(
                    "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch",
                )
                .unwrap(),
                PathEnforcement::Strict,
            )
            .await
            .unwrap();

            assert!(
                commit_changes(
                    &root,
                    &mut changes,
                    &ToolContext::new("test", "message"),
                    CommitOptions {
                        fault: Some(CommitFault { change: 0, phase }),
                        ..Default::default()
                    },
                )
                .await
                .is_err()
            );
            assert_eq!(
                fs::read_to_string(dir.path().join("file.txt"))
                    .await
                    .unwrap(),
                "before\n",
                "failed at {phase:?}"
            );
            assert!(
                patch_artifacts(dir.path()).is_empty(),
                "failed at {phase:?}"
            );
        }
    }

    #[tokio::test]
    async fn add_delete_and_move_roll_back_at_commit_boundaries() {
        for phase in [CommitPhase::DestinationInstalled, CommitPhase::StageRemoved] {
            let dir = tempdir().unwrap();
            let root = canonicalize_root(dir.path()).await.unwrap();
            let mut changes = build_plan(
                &root,
                parse_patch("*** Begin Patch\n*** Add File: file.txt\n+added\n*** End Patch")
                    .unwrap(),
                PathEnforcement::Strict,
            )
            .await
            .unwrap();
            assert!(
                commit_changes(
                    &root,
                    &mut changes,
                    &ToolContext::new("test", "message"),
                    CommitOptions {
                        fault: Some(CommitFault { change: 0, phase }),
                        ..Default::default()
                    },
                )
                .await
                .is_err()
            );
            assert!(!dir.path().join("file.txt").exists());
            assert!(patch_artifacts(dir.path()).is_empty());
        }

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("delete.txt"), "before\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let mut changes = build_plan(
            &root,
            parse_patch("*** Begin Patch\n*** Delete File: delete.txt\n*** End Patch").unwrap(),
            PathEnforcement::Strict,
        )
        .await
        .unwrap();
        assert!(
            commit_changes(
                &root,
                &mut changes,
                &ToolContext::new("test", "message"),
                CommitOptions {
                    fault: Some(CommitFault {
                        change: 0,
                        phase: CommitPhase::BackupCreated,
                    }),
                    ..Default::default()
                },
            )
            .await
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("delete.txt"))
                .await
                .unwrap(),
            "before\n"
        );
        assert!(patch_artifacts(dir.path()).is_empty());

        for phase in [
            CommitPhase::DestinationInstalled,
            CommitPhase::StageRemoved,
            CommitPhase::PermissionsRestored,
            CommitPhase::BackupCreated,
        ] {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join("source.txt"), "before\n")
                .await
                .unwrap();
            let root = canonicalize_root(dir.path()).await.unwrap();
            let mut changes = build_plan(
                &root,
                parse_patch(
                    "*** Begin Patch\n*** Update File: source.txt\n*** Move to: moved.txt\n*** End Patch",
                )
                .unwrap(),
                PathEnforcement::Strict,
            )
            .await
            .unwrap();
            assert!(
                commit_changes(
                    &root,
                    &mut changes,
                    &ToolContext::new("test", "message"),
                    CommitOptions {
                        fault: Some(CommitFault { change: 0, phase }),
                        ..Default::default()
                    },
                )
                .await
                .is_err()
            );
            assert_eq!(
                fs::read_to_string(dir.path().join("source.txt"))
                    .await
                    .unwrap(),
                "before\n",
                "failed at {phase:?}"
            );
            assert!(!dir.path().join("moved.txt").exists());
            assert!(
                patch_artifacts(dir.path()).is_empty(),
                "failed at {phase:?}"
            );
        }
    }

    #[tokio::test]
    async fn cancelling_waiter_does_not_cancel_transaction() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("first.txt"), "first\n")
            .await
            .unwrap();
        fs::write(dir.path().join("second.txt"), "second\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let changes = build_plan(
            &root,
            parse_patch(
                "*** Begin Patch\n*** Update File: first.txt\n@@\n-first\n+changed first\n*** Update File: second.txt\n@@\n-second\n+changed second\n*** End Patch",
            )
            .unwrap(),
            PathEnforcement::Strict,
        )
        .await
        .unwrap();
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let resume = std::sync::Arc::new(tokio::sync::Notify::new());
        let completed = std::sync::Arc::new(tokio::sync::Notify::new());
        let outer = tokio::spawn(run_commit_task(
            root,
            changes,
            ToolContext::new("test", "message"),
            CommitOptions {
                pause: Some(CommitPause {
                    after_change: 0,
                    reached: reached.clone(),
                    resume: resume.clone(),
                }),
                completed: Some(completed.clone()),
                ..Default::default()
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(2), reached.notified())
            .await
            .unwrap();
        outer.abort();
        let _ = outer.await;
        resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), completed.notified())
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("first.txt"))
                .await
                .unwrap(),
            "changed first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("second.txt"))
                .await
                .unwrap(),
            "changed second\n"
        );
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn explicit_abort_rolls_back_partial_transaction() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("first.txt"), "first\n")
            .await
            .unwrap();
        fs::write(dir.path().join("second.txt"), "second\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let changes = build_plan(
            &root,
            parse_patch(
                "*** Begin Patch\n*** Update File: first.txt\n@@\n-first\n+changed first\n*** Update File: second.txt\n@@\n-second\n+changed second\n*** End Patch",
            )
            .unwrap(),
            PathEnforcement::Strict,
        )
        .await
        .unwrap();
        let reached = std::sync::Arc::new(tokio::sync::Notify::new());
        let resume = std::sync::Arc::new(tokio::sync::Notify::new());
        let ctx = ToolContext::new("test", "message");
        let outer = tokio::spawn(run_commit_task(
            root,
            changes,
            ctx.clone(),
            CommitOptions {
                pause: Some(CommitPause {
                    after_change: 0,
                    reached: reached.clone(),
                    resume: resume.clone(),
                }),
                ..Default::default()
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(2), reached.notified())
            .await
            .unwrap();
        ctx.abort();
        resume.notify_one();
        let error = outer.await.unwrap().unwrap_err();

        assert!(error.to_string().contains("aborted"));
        assert_eq!(
            fs::read_to_string(dir.path().join("first.txt"))
                .await
                .unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("second.txt"))
                .await
                .unwrap(),
            "second\n"
        );
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn rollback_refuses_to_delete_a_concurrently_changed_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let backup = dir.path().join("backup.txt");
        fs::write(&path, "installed\n").await.unwrap();
        fs::write(&backup, "original\n").await.unwrap();
        let installed = installed_file(&path).await.unwrap();
        fs::write(&path, "external change\n").await.unwrap();
        let mut journal = vec![JournalEntry::Replaced {
            path: path.clone(),
            backup: backup.clone(),
            installed: Some(installed),
        }];

        let error = rollback_changes(&mut journal).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("changed after apply_patch installed it")
        );
        assert_eq!(fs::read_to_string(path).await.unwrap(), "external change\n");
        assert_eq!(fs::read_to_string(backup).await.unwrap(), "original\n");
    }

    #[tokio::test]
    async fn commit_failure_removes_added_files_and_directories() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("existing.txt"), "before\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations = parse_patch(
            "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Update File: existing.txt\n@@\n-before\n+after\n*** End Patch",
        )
        .unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();

        commit_changes(
            &root,
            &mut changes,
            &ToolContext::new("test", "message"),
            CommitOptions {
                fail_after: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

        assert!(!dir.path().join("nested/new.txt").exists());
        assert!(!dir.path().join("nested").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("existing.txt"))
                .await
                .unwrap(),
            "before\n"
        );
    }

    #[tokio::test]
    async fn rejects_source_changed_after_planning() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "before\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch",
        )
        .unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();
        fs::write(dir.path().join("file.txt"), "external change\n")
            .await
            .unwrap();

        let error = commit_changes(
            &root,
            &mut changes,
            &ToolContext::new("test", "message"),
            CommitOptions::default(),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("changed after validation"));
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt"))
                .await
                .unwrap(),
            "external change\n"
        );
    }

    #[tokio::test]
    async fn preserves_crlf_and_missing_final_newline() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("crlf.txt"), b"one\r\ntwo\r\n")
            .await
            .unwrap();
        fs::write(dir.path().join("no-newline.txt"), b"one\ntwo")
            .await
            .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: crlf.txt\n@@\n-two\n+changed\n*** Update File: no-newline.txt\n@@\n-two\n+changed\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read(dir.path().join("crlf.txt")).await.unwrap(),
            b"one\r\nchanged\r\n"
        );
        assert_eq!(
            fs::read(dir.path().join("no-newline.txt")).await.unwrap(),
            b"one\nchanged"
        );
    }

    #[tokio::test]
    async fn rejects_mixed_line_endings_without_modifying_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        let original = b"one\r\ntwo\n";
        fs::write(&path, original).await.unwrap();

        let result = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: mixed.txt\n@@\n-two\n+changed\n*** End Patch",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(fs::read(path).await.unwrap(), original);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_source_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("script.sh");
        fs::write(&path, "old\n").await.unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: script.sh\n@@\n-old\n+new\n*** End Patch",
        )
        .await
        .unwrap();

        let mode = fs::metadata(path).await.unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_move_permissions_and_rejects_metadata_changes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let move_path = dir.path().join("move.sh");
        fs::write(&move_path, "move\n").await.unwrap();
        fs::set_permissions(&move_path, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: move.sh\n*** Move to: moved.sh\n*** End Patch",
        )
        .await
        .unwrap();
        let moved_mode = fs::metadata(dir.path().join("moved.sh"))
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(moved_mode, 0o755);

        let path = dir.path().join("planned.txt");
        fs::write(&path, "before\n").await.unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let mut changes = build_plan(
            &root,
            parse_patch(
                "*** Begin Patch\n*** Update File: planned.txt\n@@\n-before\n+after\n*** End Patch",
            )
            .unwrap(),
            PathEnforcement::Strict,
        )
        .await
        .unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();

        let error = commit_changes(
            &root,
            &mut changes,
            &ToolContext::new("test", "message"),
            CommitOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("identity or metadata changed"));
        assert_eq!(fs::read_to_string(path).await.unwrap(), "before\n");
    }

    #[tokio::test]
    async fn rejects_traversal_and_existing_add_destination() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("exists.txt"), "original\n")
            .await
            .unwrap();

        let traversal = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: ../outside.txt\n+bad\n*** End Patch",
        )
        .await;
        assert!(traversal.is_err());

        let existing = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: exists.txt\n+replacement\n*** End Patch",
        )
        .await;
        assert!(existing.is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("exists.txt"))
                .await
                .unwrap(),
            "original\n"
        );
    }

    #[tokio::test]
    async fn rejects_source_file_over_size_limit_before_reading() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let file = fs::File::create(&path).await.unwrap();
        file.set_len(MAX_FILE_BYTES + 1).await.unwrap();

        let result = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: large.txt\n@@\n-old\n+new\n*** End Patch",
        )
        .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Patch source is too large")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        let result = run_patch(
            root.path(),
            "*** Begin Patch\n*** Add File: escape/outside.txt\n+bad\n*** End Patch",
        )
        .await;

        assert!(result.is_err());
        assert!(!outside.path().join("outside.txt").exists());
    }

    #[tokio::test]
    async fn aborted_context_does_not_modify_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "before\n")
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch",
        )
        .unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();
        let ctx = ToolContext::new("test", "message");
        ctx.abort();

        assert!(
            commit_changes(&root, &mut changes, &ctx, CommitOptions::default())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt"))
                .await
                .unwrap(),
            "before\n"
        );
    }

    #[test]
    fn rejects_invalid_patch_envelopes_and_empty_paths() {
        for (patch, expected) in [
            ("", "first line must be"),
            ("*** Begin Patch\n*** End Patch", "empty patch"),
            ("*** Add File: x\n+x\n*** End Patch", "first line must be"),
            ("*** Begin Patch\n*** Add File: x\n+x", "last line must be"),
            (
                "*** End Patch\n*** Begin Patch\n*** Add File: x\n+x",
                "first line must be",
            ),
            (
                "*** Begin Patch\n*** Add File:   \n+x\n*** End Patch",
                "path is empty",
            ),
        ] {
            let error = parse_patch(patch).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {patch:?}: {error:#}"
            );
        }
    }

    #[test]
    fn rejects_malformed_heredocs() {
        for patch in [
            "cat <<'PATCH\n*** Begin Patch\n*** End Patch\nPATCH",
            "cat <<'PATCH\"\n*** Begin Patch\n*** End Patch\nPATCH",
            "<<PATCH\n*** Begin Patch\n*** End Patch\nOTHER",
        ] {
            assert!(strip_heredoc(patch).is_err());
        }
    }

    #[test]
    fn enforces_exact_operation_and_patch_line_boundaries() {
        let mut accepted = String::from("*** Begin Patch\n");
        for index in 0..MAX_OPERATIONS {
            accepted.push_str(&format!("*** Delete File: file-{index}.txt\n"));
        }
        accepted.push_str("*** End Patch");
        assert_eq!(parse_patch(&accepted).unwrap().len(), MAX_OPERATIONS);

        let too_many_lines = format!(
            "*** Begin Patch\n{}*** End Patch",
            "\n".repeat(MAX_PATCH_LINES)
        );
        assert!(
            parse_patch(&too_many_lines)
                .unwrap_err()
                .to_string()
                .contains("too many lines")
        );
    }

    #[tokio::test]
    async fn rejects_missing_directory_binary_and_invalid_utf8_sources() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        fs::write(dir.path().join("binary.bin"), b"text\0binary")
            .await
            .unwrap();
        fs::write(dir.path().join("invalid.txt"), [0xff, 0xfe])
            .await
            .unwrap();

        for patch in [
            "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
            "*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch",
            "*** Begin Patch\n*** Delete File: directory\n*** End Patch",
            "*** Begin Patch\n*** Delete File: binary.bin\n*** End Patch",
            "*** Begin Patch\n*** Delete File: invalid.txt\n*** End Patch",
        ] {
            assert!(
                run_patch(dir.path(), patch).await.is_err(),
                "patch should fail: {patch}"
            );
        }
        assert!(dir.path().join("directory").is_dir());
        assert!(dir.path().join("binary.bin").exists());
        assert!(dir.path().join("invalid.txt").exists());
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn rejects_move_to_self_existing_destination_and_conflicting_sources() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("source.txt"), "source\n")
            .await
            .unwrap();
        fs::write(dir.path().join("destination.txt"), "destination\n")
            .await
            .unwrap();

        for patch in [
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: source.txt\n*** End Patch",
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination.txt\n*** End Patch",
            "*** Begin Patch\n*** Update File: source.txt\n@@\n-source\n+one\n*** Update File: source.txt\n@@\n-source\n+two\n*** End Patch",
        ] {
            assert!(
                run_patch(dir.path(), patch).await.is_err(),
                "patch should fail: {patch}"
            );
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("source.txt"))
                .await
                .unwrap(),
            "source\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("destination.txt"))
                .await
                .unwrap(),
            "destination\n"
        );
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn applies_multiple_chunks_and_anchored_insertions() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("file.txt"),
            "one\ntwo\nanchor\nthree\nfour\n",
        )
        .await
        .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-one\n+ONE\n@@ anchor\n+inserted\n@@\n-four\n+FOUR\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt"))
                .await
                .unwrap(),
            "ONE\ntwo\nanchor\ninserted\nthree\nFOUR\n"
        );
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn eof_marker_selects_only_the_final_duplicate() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "same\nmiddle\nsame\n")
            .await
            .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-same\n+final\n*** End of File\n*** End Patch",
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt"))
                .await
                .unwrap(),
            "same\nmiddle\nfinal\n"
        );
    }

    #[tokio::test]
    async fn preserves_bare_cr_and_move_only_bytes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("cr.txt"), b"one\rtwo\r")
            .await
            .unwrap();
        fs::write(dir.path().join("move.txt"), b"no-final-newline")
            .await
            .unwrap();

        run_patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: cr.txt\n@@\n-two\n+changed\n*** Update File: move.txt\n*** Move to: moved.txt\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read(dir.path().join("cr.txt")).await.unwrap(),
            b"one\rchanged\r"
        );
        assert_eq!(
            fs::read(dir.path().join("moved.txt")).await.unwrap(),
            b"no-final-newline"
        );
    }

    #[tokio::test]
    async fn rejects_same_content_source_replacement_after_planning() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let original = dir.path().join("original.txt");
        fs::write(&path, "same\n").await.unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-same\n+changed\n*** End Patch",
        )
        .unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();
        fs::rename(&path, &original).await.unwrap();
        fs::write(&path, "same\n").await.unwrap();

        let error = commit_changes(
            &root,
            &mut changes,
            &ToolContext::new("test", "message"),
            CommitOptions::default(),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("identity or metadata changed"));
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "same\n");
        assert_eq!(fs::read_to_string(&original).await.unwrap(), "same\n");
        assert!(patch_artifacts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn rejects_destination_created_after_planning() {
        let dir = tempdir().unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations =
            parse_patch("*** Begin Patch\n*** Add File: new.txt\n+planned\n*** End Patch").unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();
        fs::write(dir.path().join("new.txt"), "external\n")
            .await
            .unwrap();

        assert!(
            commit_changes(
                &root,
                &mut changes,
                &ToolContext::new("test", "message"),
                CommitOptions::default(),
            )
            .await
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("new.txt"))
                .await
                .unwrap(),
            "external\n"
        );
    }

    #[tokio::test]
    async fn enforces_line_limit_for_delete_and_move_only() {
        let dir = tempdir().unwrap();
        let oversized = "\n".repeat(MAX_FILE_LINES + 1);
        fs::write(dir.path().join("delete.txt"), &oversized)
            .await
            .unwrap();
        fs::write(dir.path().join("move.txt"), &oversized)
            .await
            .unwrap();

        assert!(
            run_patch(
                dir.path(),
                "*** Begin Patch\n*** Delete File: delete.txt\n*** End Patch",
            )
            .await
            .is_err()
        );
        assert!(
            run_patch(
                dir.path(),
                "*** Begin Patch\n*** Update File: move.txt\n*** Move to: moved.txt\n*** End Patch",
            )
            .await
            .is_err()
        );
        assert!(dir.path().join("delete.txt").exists());
        assert!(dir.path().join("move.txt").exists());
        assert!(!dir.path().join("moved.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_file_uses_source_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("private.txt");
        fs::write(&path, "before\n").await.unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        let root = canonicalize_root(dir.path()).await.unwrap();
        let operations = parse_patch(
            "*** Begin Patch\n*** Update File: private.txt\n@@\n-before\n+after\n*** End Patch",
        )
        .unwrap();
        let mut changes = build_plan(&root, operations, PathEnforcement::Strict)
            .await
            .unwrap();
        let mut directories = Vec::new();

        stage_changes(
            &root,
            &mut changes,
            &mut directories,
            PathEnforcement::Strict,
        )
        .await
        .unwrap();
        let staged = changes[0].staged.as_ref().unwrap();
        let mode = fs::metadata(staged).await.unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        cleanup_staged(&changes).await;
        cleanup_directories(&directories).await;
    }

    #[tokio::test]
    async fn supports_near_limit_filename_with_short_temp_names() {
        let dir = tempdir().unwrap();
        let name = format!("{}.txt", "a".repeat(240));
        fs::write(dir.path().join(&name), "before\n").await.unwrap();
        run_patch(
            dir.path(),
            &format!(
                "*** Begin Patch\n*** Update File: {name}\n@@\n-before\n+after\n*** End Patch"
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(name)).await.unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn reports_empty_delete_and_complete_metadata_contract() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("empty.txt"), "").await.unwrap();
        let result = run_patch(
            dir.path(),
            "*** Begin Patch\n*** Delete File: empty.txt\n*** End Patch",
        )
        .await
        .unwrap();

        assert_eq!(result.title, "apply_patch");
        assert!(result.output.contains("D empty.txt"));
        assert_eq!(result.metadata["diff_truncated"], false);
        let files = result.metadata["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["operation"], "delete");
        assert_eq!(files[0]["source"], "empty.txt");
        assert!(files[0]["destination"].is_null());
        assert_eq!(files[0]["diff_omitted"], false);
        assert!(
            files[0]["diff"]
                .as_str()
                .unwrap()
                .contains("deleted file empty.txt")
        );
        assert!(
            result.metadata["diff"]
                .as_str()
                .unwrap()
                .contains("deleted file empty.txt")
        );
    }
}
