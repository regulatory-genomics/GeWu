//! Content-addressed plugin/package artifacts and bounded archive extraction.

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use zip::ZipArchive;

const DIGEST_BYTES: usize = 64;

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
    max_bytes: u64,
    max_files: usize,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: 4 * 1024 * 1024 * 1024,
            max_files: 100_000,
        }
    }

    pub fn with_limits(mut self, max_bytes: u64, max_files: usize) -> Self {
        self.max_bytes = max_bytes;
        self.max_files = max_files;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, sha256: &str) -> Result<PathBuf> {
        validate_digest(sha256)?;
        Ok(self.root.join("sha256").join(sha256))
    }

    pub fn put_file(&self, source: &Path, expected_sha256: &str, expected_bytes: u64) -> Result<PathBuf> {
        validate_digest(expected_sha256)?;
        let metadata = fs::metadata(source).with_context(|| format!("could not stat {}", source.display()))?;
        ensure!(metadata.is_file(), "artifact source is not a regular file");
        ensure!(metadata.len() == expected_bytes, "artifact size does not match its metadata");
        let actual = sha256_file(source)?;
        ensure!(actual == expected_sha256, "artifact SHA-256 does not match its metadata");

        let destination = self.blob_path(expected_sha256)?;
        if destination.exists() {
            ensure!(sha256_file(&destination)? == expected_sha256, "existing artifact blob is corrupt");
            ensure!(fs::metadata(&destination)?.len() == expected_bytes, "existing artifact blob has wrong size");
            return Ok(destination);
        }
        fs::create_dir_all(destination.parent().unwrap())?;
        let temporary = tempfile::Builder::new().prefix(".artifact-").tempfile_in(destination.parent().unwrap())?;
        let mut output = temporary.as_file().try_clone()?;
        let mut input = File::open(source)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        temporary.persist(&destination).map_err(|e| e.error)?;
        Ok(destination)
    }

    pub fn put_bytes(&self, bytes: &[u8], expected_sha256: &str) -> Result<PathBuf> {
        let actual = sha256_bytes(bytes);
        ensure!(actual == expected_sha256, "artifact SHA-256 does not match its metadata");
        let temporary = tempfile::Builder::new().prefix(".artifact-input-").tempfile()?;
        temporary.as_file().write_all(bytes)?;
        temporary.as_file().sync_all()?;
        let source = temporary.path().to_path_buf();
        self.put_file(&source, expected_sha256, bytes.len() as u64)
    }

    pub fn extract_zip(&self, archive: &Path, destination: &Path) -> Result<()> {
        let metadata = fs::metadata(archive)?;
        ensure!(metadata.is_file(), "archive is not a regular file");
        fs::create_dir_all(destination)?;
        let file = File::open(archive)?;
        let mut zip = ZipArchive::new(file).context("invalid ZIP artifact")?;
        ensure!(zip.len() <= self.max_files, "archive contains too many entries");
        let mut total = 0u64;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            let relative = safe_archive_path(entry.name())?;
            let target = destination.join(relative);
            if entry.is_dir() {
                fs::create_dir_all(&target)?;
                continue;
            }
            let mode = entry.unix_mode();
            if mode.is_some_and(|mode| mode & 0o170000 == 0o120000) {
                bail!("archive contains a symlink");
            }
            total = total.checked_add(entry.size()).context("archive size overflow")?;
            ensure!(total <= self.max_bytes, "archive expands beyond configured limit");
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = OpenOptions::new().write(true).create_new(true).open(&target)
                .with_context(|| format!("could not create extracted file {}", target.display()))?;
            std::io::copy(&mut entry, &mut output)?;
            output.sync_all()?;
            #[cfg(unix)]
            if let Some(mode) = mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777))?;
            }
        }
        Ok(())
    }
}

pub fn validate_digest(value: &str) -> Result<()> {
    ensure!(value.len() == DIGEST_BYTES, "SHA-256 must be 64 lowercase hexadecimal characters");
    ensure!(value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()), "SHA-256 must be lowercase hexadecimal");
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn safe_archive_path(name: &str) -> Result<PathBuf> {
    ensure!(!name.is_empty() && !name.contains('\\'), "archive contains an invalid path");
    let path = Path::new(name);
    ensure!(path.components().all(|component| matches!(component, Component::Normal(_))), "archive path escapes its destination");
    Ok(path.to_path_buf())
}
