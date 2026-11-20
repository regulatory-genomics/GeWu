//! Signed marketplace snapshot client.

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use gewu_protocol::release::{
    Artifact, MARKETPLACE_SCHEMA, MARKETPLACE_V2_SCHEMA, MarketplaceIndex, Release,
};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::Value;
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};
use time::format_description::well_known::Rfc3339;

use super::artifacts::ArtifactStore;

const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Large artifacts are fetched in bounded ranges so a proxy or tunnel cannot
/// truncate one long response, and a failed chunk restarts only that chunk.
const DOWNLOAD_CHUNK: u64 = 32 * 1024 * 1024;
const DOWNLOAD_ATTEMPTS: u32 = 5;
pub const OFFICIAL_KEY_ID: &str = "official-v1";
pub const OFFICIAL_PUBLIC_KEY_B64: &str =
    include_str!("../../resources/marketplace/official-v1.pub.b64");
pub const OFFICIAL_INDEX_BASE: &str = "https://plugin.regulogen.cn";

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
        Self {
            repository,
            cache_dir: cache_dir.into(),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn repository(&self) -> &MarketplaceRepository {
        &self.repository
    }

    pub fn verify(
        index_bytes: &[u8],
        signature_bytes: &[u8],
        repository: &MarketplaceRepository,
    ) -> Result<MarketplaceIndex, MarketplaceError> {
        let signature_text = std::str::from_utf8(signature_bytes)
            .map_err(|_| MarketplaceError::InvalidSignature)?
            .trim();
        let signature = BASE64
            .decode(signature_text)
            .map_err(|_| MarketplaceError::InvalidSignature)?;
        UnparsedPublicKey::new(&ED25519, &repository.public_key)
            .verify(index_bytes, &signature)
            .map_err(|_| MarketplaceError::InvalidSignature)?;
        let value: Value = serde_json::from_slice(index_bytes)
            .map_err(|error| MarketplaceError::InvalidIndex(error.to_string()))?;
        let schema_version = value
            .get("schemaVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MarketplaceError::InvalidIndex("marketplace schemaVersion is missing".into())
            })?;
        let schema = match schema_version {
            "gewu.marketplace/v1" => MARKETPLACE_SCHEMA,
            "gewu.marketplace/v2" => MARKETPLACE_V2_SCHEMA,
            _ => {
                return Err(MarketplaceError::InvalidIndex(
                    "unsupported marketplace schema".into(),
                ));
            }
        };
        let signed = serde_json::json!({
            "index": value,
            "signature": {"algorithm": "ed25519", "keyId": repository.key_id, "value": signature_text},
        });
        let schema: Value = serde_json::from_str(schema)
            .map_err(|error| MarketplaceError::InvalidIndex(error.to_string()))?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| MarketplaceError::InvalidIndex(error.to_string()))?;
        if let Err(error) = validator.validate(&signed) {
            return Err(MarketplaceError::InvalidIndex(format!(
                "schema validation failed at {}",
                error.instance_path()
            )));
        }
        let index: MarketplaceIndex = serde_json::from_value(signed["index"].clone())
            .map_err(|error| MarketplaceError::InvalidIndex(error.to_string()))?;
        if index.repository != repository.id || index.key_id != repository.key_id {
            return Err(MarketplaceError::InvalidIndex(
                "repository or key identity mismatch".into(),
            ));
        }
        Ok(index)
    }

    pub fn cache(
        &self,
        index_bytes: &[u8],
        signature_bytes: &[u8],
    ) -> Result<MarketplaceIndex, MarketplaceError> {
        if index_bytes.len() > self.max_bytes || signature_bytes.len() > self.max_bytes {
            return Err(MarketplaceError::TooLarge);
        }
        let index = Self::verify(index_bytes, signature_bytes, &self.repository)?;
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| MarketplaceError::Cache(e.to_string()))?;
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
        let signature =
            fetch_bounded(&client, &self.repository.signature_url, self.max_bytes).await?;
        self.cache(&index, &signature)
    }

    pub async fn download_artifact(
        &self,
        artifact: &Artifact,
        store: &ArtifactStore,
    ) -> Result<PathBuf, MarketplaceError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Artifacts can be hundreds of megabytes on a slow uplink. Bound
            // the connection attempt, not the whole transfer.
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .build()
            .map_err(|e| MarketplaceError::Network(e.to_string()))?;
        let url = match std::env::var("GEWU_MARKETPLACE_ASSET_ORIGIN") {
            Ok(origin) if !origin.trim().is_empty() => {
                rewrite_origin(&artifact.url, origin.trim())?
            }
            _ => artifact.url.clone(),
        };
        let limit = usize::try_from(artifact.bytes.saturating_add(1))
            .ok()
            .filter(|limit| *limit <= 4 * 1024 * 1024 * 1024)
            .ok_or_else(|| {
                MarketplaceError::InvalidIndex("artifact size is out of range".into())
            })?;
        if artifact.bytes > DOWNLOAD_CHUNK {
            return self.download_ranged(&client, &url, artifact, store).await;
        }
        let bytes = fetch_bounded(&client, &url, limit).await?;
        if bytes.len() as u64 != artifact.bytes {
            return Err(MarketplaceError::InvalidIndex(
                "artifact size does not match its metadata".into(),
            ));
        }
        if super::artifacts::sha256_bytes(&bytes) != artifact.sha256 {
            return Err(MarketplaceError::InvalidSignature);
        }
        store
            .put_bytes(&bytes, &artifact.sha256)
            .map_err(|error| MarketplaceError::Cache(error.to_string()))
    }

    async fn download_ranged(
        &self,
        client: &reqwest::Client,
        url: &str,
        artifact: &Artifact,
        store: &ArtifactStore,
    ) -> Result<PathBuf, MarketplaceError> {
        std::fs::create_dir_all(store.root())
            .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
        let mut file = tempfile::Builder::new()
            .prefix(".artifact-download-")
            .tempfile_in(store.root())
            .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
        let mut offset = 0u64;
        while offset < artifact.bytes {
            let end = (offset + DOWNLOAD_CHUNK - 1).min(artifact.bytes - 1);
            let mut last_error = None;
            for attempt in 0..DOWNLOAD_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
                }
                match fetch_chunk(client, url, offset, end, file.as_file_mut()).await {
                    Ok(()) => {
                        last_error = None;
                        break;
                    }
                    Err(error) if retryable(&error) => last_error = Some(error),
                    Err(error) => return Err(error),
                }
            }
            if let Some(error) = last_error {
                return Err(error);
            }
            offset = end + 1;
        }
        file.as_file()
            .sync_all()
            .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
        store
            .put_file(file.path(), &artifact.sha256, artifact.bytes)
            .map_err(|error| MarketplaceError::Cache(error.to_string()))
    }

    pub fn search<'a>(index: &'a MarketplaceIndex, query: &str) -> Vec<&'a Release> {
        let query = query.to_ascii_lowercase();
        index
            .releases
            .iter()
            .filter(|release| {
                query.is_empty()
                    || release.plugin.to_ascii_lowercase().contains(&query)
                    || release
                        .release_notes
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    pub fn info<'a>(
        index: &'a MarketplaceIndex,
        plugin: &str,
        version: Option<&str>,
    ) -> Option<&'a Release> {
        index.releases.iter().find(|release| {
            release.plugin == plugin && version.is_none_or(|version| release.version == version)
        })
    }
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// A mirror or tunnel substitutes only scheme/host/port. The artifact path and
/// its SHA-256 stay authoritative, so a rewritten URL cannot change content.
pub fn rewrite_origin(value: &str, origin: &str) -> Result<String, MarketplaceError> {
    let mut parsed =
        url::Url::parse(value).map_err(|e| MarketplaceError::InvalidIndex(e.to_string()))?;
    let base =
        url::Url::parse(origin).map_err(|e| MarketplaceError::InvalidIndex(e.to_string()))?;
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

fn retryable(error: &MarketplaceError) -> bool {
    match error {
        MarketplaceError::Network(_) => true,
        MarketplaceError::HttpStatus(status) => *status >= 500 || *status == 429,
        _ => false,
    }
}

async fn fetch_chunk(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    file: &mut std::fs::File,
) -> Result<(), MarketplaceError> {
    use std::io::{Seek, SeekFrom, Write};
    let expected = end - start + 1;
    file.set_len(start)
        .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|error| MarketplaceError::Network(format!("chunk at {start}: {error}")))?;
    if response.status() == reqwest::StatusCode::OK {
        return Err(MarketplaceError::InvalidIndex(
            "server ignored the range request".into(),
        ));
    }
    if !response.status().is_success() {
        return Err(MarketplaceError::HttpStatus(response.status().as_u16()));
    }
    let mut received = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| MarketplaceError::Network(format!("chunk at {start}: {error}")))?;
        received += chunk.len() as u64;
        if received > expected {
            return Err(MarketplaceError::InvalidIndex(
                "chunk exceeds the requested range".into(),
            ));
        }
        file.write_all(&chunk)
            .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
    }
    if received != expected {
        return Err(MarketplaceError::Network(format!(
            "chunk at {start} ended after {received} of {expected} bytes"
        )));
    }
    Ok(())
}

async fn fetch_bounded(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
) -> Result<Vec<u8>, MarketplaceError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| MarketplaceError::Network(e.to_string()))?;
    if response.status().is_redirection() {
        return Err(MarketplaceError::Redirect);
    }
    if !response.status().is_success() {
        return Err(MarketplaceError::HttpStatus(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(MarketplaceError::TooLarge);
    }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| MarketplaceError::Network(e.to_string()))?;
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(MarketplaceError::TooLarge);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, MarketplaceError> {
    let bytes = std::fs::read(path).map_err(|e| MarketplaceError::Cache(e.to_string()))?;
    if bytes.len() > limit {
        return Err(MarketplaceError::TooLarge);
    }
    Ok(bytes)
}

fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), MarketplaceError> {
    let parent = path
        .parent()
        .ok_or_else(|| MarketplaceError::Cache("cache path has no parent".into()))?;
    let temporary = tempfile::Builder::new()
        .prefix(".marketplace-")
        .tempfile_in(parent)
        .map_err(|e| MarketplaceError::Cache(e.to_string()))?;
    std::fs::write(temporary.path(), bytes).map_err(|e| MarketplaceError::Cache(e.to_string()))?;
    temporary
        .persist(path)
        .map_err(|e| MarketplaceError::Cache(e.error.to_string()))?;
    Ok(())
}
