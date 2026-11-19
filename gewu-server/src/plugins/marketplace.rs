//! Signed marketplace snapshot client.

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use gewu_protocol::release::{Artifact, MarketplaceIndex, Release};
use ring::signature::{ED25519, UnparsedPublicKey};
use std::{fmt, path::{Path, PathBuf}, time::Duration};
use time::format_description::well_known::Rfc3339;

use super::artifacts::ArtifactStore;

const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const OFFICIAL_KEY_ID: &str = "official-v1";
pub const OFFICIAL_PUBLIC_KEY_B64: &str = include_str!("../../resources/marketplace/official-v1.pub.b64");
pub const OFFICIAL_INDEX_BASE: &str =
    "http://172.16.211.53:3000/gilbert/gewu-marketplace/raw/branch/master/public";

#[derive(Debug)]
pub enum MarketplaceError {
    Network(String),
    HttpStatus(u16),
    Redirect,
    TooLarge,
    InvalidSignature,
    InvalidIndex(String),
    Cache(String),
    Expired,
}

impl fmt::Display for MarketplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(message) => write!(f, "marketplace network error: {message}"),
            Self::HttpStatus(status) => write!(f, "marketplace returned HTTP {status}"),
            Self::Redirect => f.write_str("marketplace redirects are not allowed"),
            Self::TooLarge => f.write_str("marketplace response exceeds the configured limit"),
            Self::InvalidSignature => f.write_str("marketplace signature is invalid"),
            Self::InvalidIndex(message) => write!(f, "marketplace index is invalid: {message}"),
            Self::Cache(message) => write!(f, "marketplace cache error: {message}"),
            Self::Expired => f.write_str("marketplace index has expired"),
        }
    }
}

impl std::error::Error for MarketplaceError {}

#[derive(Debug, Clone)]
pub struct MarketplaceRepository {
    pub id: String,
    pub index_url: String,
    pub signature_url: String,
    pub public_key: Vec<u8>,
    pub key_id: String,
}

#[derive(Debug, Clone)]
pub struct MarketplaceClient {
    repository: MarketplaceRepository,
    cache_dir: PathBuf,
    max_bytes: usize,
}

impl MarketplaceClient {
    pub fn new(repository: MarketplaceRepository, cache_dir: impl Into<PathBuf>) -> Self {
        Self { repository, cache_dir: cache_dir.into(), max_bytes: DEFAULT_MAX_BYTES }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn repository(&self) -> &MarketplaceRepository { &self.repository }

    pub fn verify(index_bytes: &[u8], signature_bytes: &[u8], repository: &MarketplaceRepository) -> Result<MarketplaceIndex, MarketplaceError> {
        let signature_text = std::str::from_utf8(signature_bytes).map_err(|_| MarketplaceError::InvalidSignature)?.trim();
        let signature = BASE64.decode(signature_text).map_err(|_| MarketplaceError::InvalidSignature)?;
        UnparsedPublicKey::new(&ED25519, &repository.public_key)
            .verify(index_bytes, &signature)
            .map_err(|_| MarketplaceError::InvalidSignature)?;
        let index: MarketplaceIndex = serde_json::from_slice(index_bytes)
            .map_err(|error| MarketplaceError::InvalidIndex(error.to_string()))?;
        if index.repository != repository.id || index.key_id != repository.key_id {
            return Err(MarketplaceError::InvalidIndex("repository or key identity mismatch".into()));
        }
        Ok(index)
    }

    pub fn cache(&self, index_bytes: &[u8], signature_bytes: &[u8]) -> Result<MarketplaceIndex, MarketplaceError> {
        if index_bytes.len() > self.max_bytes || signature_bytes.len() > self.max_bytes {
            return Err(MarketplaceError::TooLarge);
        }
        let index = Self::verify(index_bytes, signature_bytes, &self.repository)?;
        std::fs::create_dir_all(&self.cache_dir).map_err(|e| MarketplaceError::Cache(e.to_string()))?;
        atomic_bytes(&self.cache_dir.join("index.v1.json"), index_bytes)?;
        atomic_bytes(&self.cache_dir.join("index.v1.json.sig"), signature_bytes)?;
        Ok(index)
    }

    pub fn load_cached(&self) -> Result<MarketplaceIndex, MarketplaceError> {
        let index = read_bounded(&self.cache_dir.join("index.v1.json"), self.max_bytes)?;
        let signature = read_bounded(&self.cache_dir.join("index.v1.json.sig"), self.max_bytes)?;
        Self::verify(&index, &signature, &self.repository)
    }

    pub async fn refresh(&self) -> Result<MarketplaceIndex, MarketplaceError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| MarketplaceError::Network(e.to_string()))?;
        let index = fetch_bounded(&client, &self.repository.index_url, self.max_bytes).await?;
        let signature = fetch_bounded(&client, &self.repository.signature_url, self.max_bytes).await?;
        self.cache(&index, &signature)
    }

    pub async fn download_artifact(&self, artifact: &Artifact, store: &ArtifactStore) -> Result<PathBuf, MarketplaceError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| MarketplaceError::Network(e.to_string()))?;
        let url = match std::env::var("GEWU_MARKETPLACE_ASSET_ORIGIN") {
            Ok(origin) if !origin.trim().is_empty() => rewrite_origin(&artifact.url, origin.trim())?,
            _ => artifact.url.clone(),
        };
        let limit = usize::try_from(artifact.bytes.saturating_add(1))
            .ok()
            .filter(|limit| *limit <= 4 * 1024 * 1024 * 1024)
            .ok_or_else(|| MarketplaceError::InvalidIndex("artifact size is out of range".into()))?;
        let bytes = fetch_bounded(&client, &url, limit).await?;
        if bytes.len() as u64 != artifact.bytes {
            return Err(MarketplaceError::InvalidIndex("artifact size does not match its metadata".into()));
        }
        if super::artifacts::sha256_bytes(&bytes) != artifact.sha256 {
            return Err(MarketplaceError::InvalidSignature);
        }
        store.put_bytes(&bytes, &artifact.sha256).map_err(|error| MarketplaceError::Cache(error.to_string()))
    }

    pub fn search<'a>(index: &'a MarketplaceIndex, query: &str) -> Vec<&'a Release> {
        let query = query.to_ascii_lowercase();
        index.releases.iter().filter(|release| {
            query.is_empty() || release.plugin.to_ascii_lowercase().contains(&query)
                || release.release_notes.as_deref().unwrap_or_default().to_ascii_lowercase().contains(&query)
        }).collect()
    }

    pub fn info<'a>(index: &'a MarketplaceIndex, plugin: &str, version: Option<&str>) -> Option<&'a Release> {
        index.releases.iter().find(|release| release.plugin == plugin && version.is_none_or(|version| release.version == version))
    }
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// A mirror or tunnel substitutes only scheme/host/port. The artifact path and
/// its SHA-256 stay authoritative, so a rewritten URL cannot change content.
pub fn rewrite_origin(value: &str, origin: &str) -> Result<String, MarketplaceError> {
    let mut parsed = url::Url::parse(value).map_err(|e| MarketplaceError::InvalidIndex(e.to_string()))?;
    let base = url::Url::parse(origin).map_err(|e| MarketplaceError::InvalidIndex(e.to_string()))?;
    parsed
        .set_scheme(base.scheme())
        .map_err(|_| MarketplaceError::InvalidIndex("unsupported mirror scheme".into()))?;
    parsed
        .set_host(base.host_str())
        .map_err(|_| MarketplaceError::InvalidIndex("unsupported mirror host".into()))?;
    parsed
        .set_port(base.port())
        .map_err(|_| MarketplaceError::InvalidIndex("unsupported mirror port".into()))?;
    Ok(parsed.to_string())
}

async fn fetch_bounded(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>, MarketplaceError> {
    let response = client.get(url).send().await.map_err(|e| MarketplaceError::Network(e.to_string()))?;
    if response.status().is_redirection() { return Err(MarketplaceError::Redirect); }
    if !response.status().is_success() { return Err(MarketplaceError::HttpStatus(response.status().as_u16())); }
    if response.content_length().is_some_and(|length| length > limit as u64) { return Err(MarketplaceError::TooLarge); }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| MarketplaceError::Network(e.to_string()))?;
        if output.len().saturating_add(chunk.len()) > limit { return Err(MarketplaceError::TooLarge); }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, MarketplaceError> {
    let bytes = std::fs::read(path).map_err(|e| MarketplaceError::Cache(e.to_string()))?;
    if bytes.len() > limit { return Err(MarketplaceError::TooLarge); }
    Ok(bytes)
}

fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), MarketplaceError> {
    let parent = path.parent().ok_or_else(|| MarketplaceError::Cache("cache path has no parent".into()))?;
    let temporary = tempfile::Builder::new().prefix(".marketplace-").tempfile_in(parent)
        .map_err(|e| MarketplaceError::Cache(e.to_string()))?;
    std::fs::write(temporary.path(), bytes).map_err(|e| MarketplaceError::Cache(e.to_string()))?;
    temporary.persist(path).map_err(|e| MarketplaceError::Cache(e.error.to_string()))?;
    Ok(())
}
