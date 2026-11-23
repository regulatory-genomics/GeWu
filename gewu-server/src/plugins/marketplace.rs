//! Signed marketplace snapshot client.

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use gewu_protocol::release::{
    Artifact, MARKETPLACE_SCHEMA, MARKETPLACE_V2_SCHEMA, MARKETPLACE_V3_SCHEMA, MarketplaceIndex,
    RELEASE_V2_SCHEMA, RELEASE_V3_SCHEMA, Release, ReleaseSelectionError, Target,
};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::Value;
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};
use time::format_description::well_known::Rfc3339;

use super::{
    artifacts::ArtifactStore,
    diagnostics::Attempt,
    lifecycle::{InstallManager, RuntimeSelection},
    state::GenerationState,
};

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
    ArtifactDigest,
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
            Self::ArtifactDigest => f.write_str("artifact SHA-256 does not match its metadata"),
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
    offline: bool,
}

impl MarketplaceClient {
    /// GUI and CLI use the same verified download and fallback transaction.
    pub async fn install_candidates(
        &self,
        manager: &InstallManager,
        release: &Release,
        selections: Vec<RuntimeSelection<'_>>,
        workspace: &Path,
        deadline: Duration,
        report: &mut Attempt,
    ) -> Result<GenerationState> {
        if release.schema_version == "gewu.release/v3"
            && let Some(RuntimeSelection::Bundled(runtime)) = selections.first()
            && let Some(budget) = runtime.installed_bytes_upper_bound
        {
            let estimate = self.install_bytes(manager, release, runtime, budget)?;
            #[cfg(unix)]
            anyhow::ensure!(
                available_bytes(manager.state.root())? >= estimate,
                "GeWu needs {estimate} available bytes to install {}",
                release.plugin
            );
        }
        report.stage("package_download");
        let package = self
            .download_artifact(&release.package, &manager.artifacts, Some(&mut *report))
            .await?;
        let mut failures = Vec::new();
        for selection in selections {
            let (kind, id, sha) = match &selection {
                RuntimeSelection::Bundled(r) => {
                    ("bundled", r.id.as_str(), Some(r.artifact.sha256.as_str()))
                }
                RuntimeSelection::External { provider, .. } => ("external", provider.id(), None),
                RuntimeSelection::Managed { provider } => ("managed", provider.id(), None),
            };
            report.provider(kind, id, sha);
            let result = async {
                let runtime_archive = if let RuntimeSelection::Bundled(runtime) = &selection {
                    report.stage("runtime_download");
                    Some(
                        self.download_artifact(
                            &runtime.artifact,
                            &manager.artifacts,
                            Some(&mut *report),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                manager
                    .install_reported(
                        release,
                        &package,
                        runtime_archive.as_deref(),
                        selection,
                        workspace,
                        deadline,
                        Some(report),
                    )
                    .await
            }
            .await;
            report.provider_result(&result);
            match result {
                Ok(installed) => return Ok(installed),
                Err(error) => {
                    // If publication of state has started, don't try another provider
                    // against a generation that may already be committed.
                    if report.is_committing() {
                        return Err(error);
                    }
                    failures.push(format!("{kind}/{id}: {error:#}"));
                }
            }
        }
        anyhow::bail!(
            "all runtime providers failed for {}: {}",
            release.release_id,
            failures.join("; ")
        )
    }

    pub fn new(repository: MarketplaceRepository, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            repository,
            cache_dir: cache_dir.into(),
            max_bytes: DEFAULT_MAX_BYTES,
            offline: false,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    fn install_bytes(
        &self,
        manager: &InstallManager,
        release: &Release,
        runtime: &gewu_protocol::release::RuntimeRef,
        budget: u64,
    ) -> Result<u64> {
        let missing = |a: &Artifact| -> Result<u64> {
            Ok(
                if fs::metadata(manager.artifacts.blob_path(&a.sha256)?)
                    .is_ok_and(|m| m.len() == a.bytes)
                {
                    0
                } else {
                    a.bytes
                },
            )
        };
        Ok(budget
            .saturating_add(missing(&release.package)?)
            .saturating_add(missing(&runtime.artifact)?)
            .saturating_add(release.package.bytes.saturating_mul(4)))
    }

    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
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
            "gewu.marketplace/v3" => MARKETPLACE_V3_SCHEMA,
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
        fn invalid(error: impl fmt::Display) -> MarketplaceError {
            MarketplaceError::InvalidIndex(error.to_string())
        }
        let registry = jsonschema::Registry::new()
            .add(
                "urn:gewu:release:v2",
                serde_json::from_str::<Value>(RELEASE_V2_SCHEMA).expect("embedded schema"),
            )
            .map_err(invalid)?
            .add(
                "urn:gewu:release:v3",
                serde_json::from_str::<Value>(RELEASE_V3_SCHEMA).expect("embedded schema"),
            )
            .map_err(invalid)?
            .add(
                "urn:gewu:marketplace:v2",
                serde_json::from_str::<Value>(MARKETPLACE_V2_SCHEMA).expect("embedded schema"),
            )
            .map_err(invalid)?
            .prepare()
            .map_err(invalid)?;
        let validator = jsonschema::options()
            .with_registry(&registry)
            .build(&schema)
            .map_err(invalid)?;
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
        for release in &index.releases {
            if release.schema_version == "gewu.release/v3" {
                for (i, runtime) in release.runtimes.iter().enumerate() {
                    if runtime.installed_bytes_upper_bound < runtime.unpacked_bytes
                        || release.runtimes[..i]
                            .iter()
                            .any(|r| r.target == runtime.target && r.profile == runtime.profile)
                    {
                        return Err(MarketplaceError::InvalidIndex(
                            "invalid runtime size or duplicate target/profile".into(),
                        ));
                    }
                }
            }
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
        let _lock =
            super::state::lock_file(&self.cache_dir.join("snapshot.lock")).map_err(cache_error)?;
        if let Ok(old) = self.load_cached()
            && old.schema_version == "gewu.marketplace/v3"
            && (index.schema_version != old.schema_version || index.generated_at < old.generated_at)
        {
            return Err(MarketplaceError::InvalidIndex(
                "marketplace snapshot would go backwards".into(),
            ));
        }
        let pair = (
            std::str::from_utf8(index_bytes).map_err(cache_error)?,
            std::str::from_utf8(signature_bytes).map_err(cache_error)?,
        );
        atomic_bytes(
            &self.cache_dir.join("snapshot.json"),
            &serde_json::to_vec(&pair).map_err(cache_error)?,
        )?;
        Ok(index)
    }

    pub fn load_cached(&self) -> Result<MarketplaceIndex, MarketplaceError> {
        let snapshot = self.cache_dir.join("snapshot.json");
        if snapshot.exists() {
            let (index, signature): (String, String) =
                serde_json::from_slice(&read_bounded(&snapshot, self.max_bytes.saturating_mul(4))?)
                    .map_err(cache_error)?;
            if index.len() > self.max_bytes || signature.len() > self.max_bytes {
                return Err(MarketplaceError::TooLarge);
            }
            return Self::verify(index.as_bytes(), signature.as_bytes(), &self.repository);
        }
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
        // The pointer only names an immutable, same-origin digest. The content
        // and its detached signature are still independently verified.
        let base = reqwest::Url::parse(&self.repository.index_url)
            .map_err(|e| MarketplaceError::InvalidIndex(e.to_string()))?;
        if base.path().ends_with("/index.v1.json") {
            let pointer = base.join("catalog/current.v3.json").map_err(cache_error)?;
            match fetch_bounded(&client, pointer.as_str(), 4096).await {
                Ok(bytes) => {
                    #[derive(serde::Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Pointer {
                        sha256: String,
                    }
                    let pointer: Pointer = serde_json::from_slice(&bytes).map_err(cache_error)?;
                    super::artifacts::validate_digest(&pointer.sha256).map_err(cache_error)?;
                    let artifact = base
                        .join(&format!("catalog/snapshots/{}/index.json", pointer.sha256))
                        .map_err(cache_error)?;
                    let index = fetch_bounded(&client, artifact.as_str(), self.max_bytes).await?;
                    if super::artifacts::sha256_bytes(&index) != pointer.sha256 {
                        return Err(MarketplaceError::InvalidSignature);
                    }
                    let signature =
                        fetch_bounded(&client, &format!("{artifact}.sig"), self.max_bytes).await?;
                    return self.cache(&index, &signature);
                }
                Err(MarketplaceError::HttpStatus(404)) => {}
                Err(error) => return Err(error),
            }
        }
        let index = fetch_bounded(&client, &self.repository.index_url, self.max_bytes).await?;
        let signature =
            fetch_bounded(&client, &self.repository.signature_url, self.max_bytes).await?;
        self.cache(&index, &signature)
    }

    pub async fn download_artifact(
        &self,
        artifact: &Artifact,
        store: &ArtifactStore,
        report: Option<&mut Attempt>,
    ) -> Result<PathBuf, MarketplaceError> {
        let blob = store.blob_path(&artifact.sha256).map_err(cache_error)?;
        let partial = store.root().join("partial").join(&artifact.sha256);
        let _lock = super::state::lock_file_async(partial.with_extension("lock"))
            .await
            .map_err(cache_error)?;
        if let Ok(metadata) = fs::metadata(&blob)
            && metadata.len() == artifact.bytes
            && super::artifacts::sha256_file(&blob).map_err(cache_error)? == artifact.sha256
        {
            return Ok(blob);
        }
        if self.offline {
            return Err(MarketplaceError::Cache(format!(
                "offline: missing verified artifact {}",
                artifact.sha256
            )));
        }
        if blob.exists() {
            fs::remove_file(&blob).map_err(cache_error)?;
        }
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
            return self
                .download_ranged(&client, &url, artifact, store, &partial, report)
                .await;
        }
        let bytes = fetch_bounded(&client, &url, limit).await?;
        if bytes.len() as u64 != artifact.bytes {
            return Err(MarketplaceError::InvalidIndex(
                "artifact size does not match its metadata".into(),
            ));
        }
        if super::artifacts::sha256_bytes(&bytes) != artifact.sha256 {
            return Err(MarketplaceError::ArtifactDigest);
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
        partial: &Path,
        mut report: Option<&mut Attempt>,
    ) -> Result<PathBuf, MarketplaceError> {
        let receipt = partial.with_extension("json");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(partial)
            .map_err(cache_error)?;
        let mut offset = fs::read(&receipt)
            .ok()
            .and_then(|data| serde_json::from_slice::<(String, u64, u64)>(&data).ok())
            .filter(|(origin, size, offset)| {
                origin == url
                    && *size == artifact.bytes
                    && (*offset == *size || *offset % DOWNLOAD_CHUNK == 0)
            })
            .map_or(0, |(_, _, offset)| offset);
        if offset > artifact.bytes || offset > file.metadata().map_err(cache_error)?.len() {
            offset = 0;
        }
        file.set_len(offset).map_err(cache_error)?;
        while offset < artifact.bytes {
            let end = (offset + DOWNLOAD_CHUNK - 1).min(artifact.bytes - 1);
            let mut last_error = None;
            for attempt in 0..DOWNLOAD_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
                }
                match fetch_chunk(
                    client,
                    url,
                    end,
                    artifact.bytes,
                    &partial.with_extension("json"),
                    &mut file,
                    &mut offset,
                )
                .await
                {
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
            file.sync_all().map_err(cache_error)?;
            atomic_bytes(
                &receipt,
                &serde_json::to_vec(&(url, artifact.bytes, offset)).map_err(cache_error)?,
            )?;
            if let Some(report) = report.as_deref_mut() {
                report.progress(offset, artifact.bytes);
            }
        }
        drop(file);
        if super::artifacts::sha256_file(partial).map_err(cache_error)? != artifact.sha256 {
            let _ = fs::remove_file(partial);
            let _ = fs::remove_file(&receipt);
            return Err(MarketplaceError::ArtifactDigest);
        }
        let blob = store.blob_path(&artifact.sha256).map_err(cache_error)?;
        fs::create_dir_all(blob.parent().unwrap()).map_err(cache_error)?;
        fs::rename(partial, &blob).map_err(cache_error)?;
        let _ = fs::remove_file(&receipt);
        Ok(blob)
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

#[cfg(unix)]
fn available_bytes(root: &Path) -> std::io::Result<u64> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let directory = root
        .ancestors()
        .find(|path| path.is_dir())
        .unwrap_or(Path::new("."));
    let path = CString::new(directory.as_os_str().as_bytes())?;
    let mut info = MaybeUninit::<libc::statvfs>::uninit();
    // A successful statvfs call initializes the full output struct.
    if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(info
        .f_bavail
        .saturating_mul(info.f_frsize as libc::fsblkcnt_t) as u64)
}

fn cache_error(error: impl fmt::Display) -> MarketplaceError {
    MarketplaceError::Cache(error.to_string())
}

#[allow(clippy::too_many_arguments)]
pub fn select_for_install<'a>(
    index: &'a MarketplaceIndex,
    plugin: &str,
    version: Option<&str>,
    channel: &str,
    target: &Target,
    host_version: &str,
    policy: &str,
    app_origin: bool,
    profile: &str,
) -> Result<&'a Release, ReleaseSelectionError> {
    index.select_matching(plugin, version, channel, &now_rfc3339(), |release| {
        install_blocker(release, target, host_version, policy, app_origin, profile)
            .map_or(Ok(()), |blocker| Err(blocker.selection_error()))
    })
}

#[derive(Debug, serde::Serialize)]
pub struct InstallBlocker {
    pub code: &'static str,
    pub message: String,
}

impl InstallBlocker {
    fn selection_error(&self) -> ReleaseSelectionError {
        match self.code {
            "revoked" => ReleaseSelectionError::Revoked,
            "plugin_api" => ReleaseSelectionError::IncompatibleApi,
            "host_version" | "host_feature" | "app_origin" => {
                ReleaseSelectionError::IncompatibleHost
            }
            _ => ReleaseSelectionError::RuntimeUnavailable,
        }
    }
}

pub fn install_blocker(
    release: &Release,
    target: &Target,
    host_version: &str,
    policy: &str,
    app_origin: bool,
    profile: &str,
) -> Option<InstallBlocker> {
    let blocked = |code, message| Some(InstallBlocker { code, message });
    if release.revocation.is_some() {
        return blocked("revoked", "This release has been revoked.".into());
    }
    if release.gewu.api != "gewu.plugin/v1" {
        return blocked(
            "plugin_api",
            format!("This release requires plugin API {}.", release.gewu.api),
        );
    }
    if release.check_host(host_version).is_err() {
        return blocked(
            "host_version",
            format!(
                "GeWu {host_version} does not meet this release's Host requirement {}. Source installation cannot bypass Host compatibility.",
                release.gewu.host
            ),
        );
    }
    for feature in &release.required_features {
        match feature.as_str() {
            "runtime-v2" | "offline-pixi-pack" => {}
            "webapp-origin" if app_origin => {}
            "webapp-origin" => {
                return blocked(
                    "app_origin",
                    "The GeWu GUI needs an app origin configured by the host administrator.".into(),
                );
            }
            _ => {
                return blocked(
                    "host_feature",
                    format!(
                        "This Host does not support required feature {feature}. Source installation cannot add Host features."
                    ),
                );
            }
        }
    }
    if profile == "core"
        && policy != "bundled"
        && release
            .runtime_plan
            .as_ref()
            .is_some_and(|plan| plan.select(policy, target).is_some())
    {
        return None;
    }
    let target_name = super::runtime::target_label(target);
    let runtimes: Vec<_> = release
        .runtimes
        .iter()
        .filter(|r| &r.target == target)
        .collect();
    if !matches!(policy, "auto" | "bundled") || runtimes.is_empty() {
        return blocked(
            "runtime_target",
            format!(
                "No {policy} runtime is published for {target_name}. Source installation may be possible; check the guide's platform support."
            ),
        );
    }
    let matching: Vec<_> = runtimes
        .iter()
        .filter(|r| r.profile.as_deref().unwrap_or("core") == profile)
        .collect();
    if matching.is_empty() {
        return blocked(
            "runtime_profile",
            format!("No {profile} runtime is published for {target_name}."),
        );
    }
    let system = super::runtime::system_version();
    if matching
        .iter()
        .any(|r| super::runtime::profile_compatible(r, system.as_deref(), profile))
    {
        return None;
    }
    blocked(
        "runtime_requirements",
        format!(
            "The {profile} runtime for {target_name} requires a newer system library/OS than {}. Requirements: {}.",
            system.as_deref().unwrap_or("unknown"),
            serde_json::to_string(&matching.iter().map(|r| &r.requirements).collect::<Vec<_>>())
                .unwrap_or_default()
        ),
    )
}

/// A recovery-only candidate never authorizes installation of an incompatible release.
#[allow(clippy::too_many_arguments)]
pub fn select_for_install_reported<'a>(
    index: &'a MarketplaceIndex,
    plugin: &str,
    version: Option<&str>,
    channel: &str,
    target: &Target,
    host_version: &str,
    policy: &str,
    app_origin: bool,
    profile: &str,
    report: Option<&mut Attempt>,
) -> Result<&'a Release> {
    match select_for_install(
        index,
        plugin,
        version,
        channel,
        target,
        host_version,
        policy,
        app_origin,
        profile,
    ) {
        Ok(release) => {
            if let Some(report) = report {
                report.release(release, target);
            }
            Ok(release)
        }
        Err(error) => {
            let candidate = index
                .select_release_version(plugin, version, channel, &now_rfc3339())
                .ok();
            let Some(release) = candidate else {
                return Err(error.into());
            };
            let blocker =
                install_blocker(release, target, host_version, policy, app_origin, profile);
            let guide = release.provenance.build_inputs.iter().find_map(|entry| {
                entry
                    .strip_prefix("installation-guide=")
                    .and_then(super::diagnostics::pinned_source_url)
            });
            if let Some(report) = report {
                report.release(release, target);
                report.compatibility(blocker.as_ref());
            }
            let message = blocker
                .map(|b| b.message)
                .unwrap_or_else(|| error.to_string());
            match guide {
                Some(url) => anyhow::bail!("{message}\nSource installation guide (manual): {url}"),
                None => anyhow::bail!(
                    "{message}\nThis version has not supplied a source installation guide."
                ),
            }
        }
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
    end: u64,
    total: u64,
    receipt: &Path,
    file: &mut File,
    offset: &mut u64,
) -> Result<(), MarketplaceError> {
    use std::io::{Seek, SeekFrom, Write};
    let start = *offset;
    file.set_len(start)
        .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| MarketplaceError::Cache(error.to_string()))?;
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| MarketplaceError::Network(format!("chunk at {start}: {error}")))?;
    let (start, expected) = match response.status() {
        reqwest::StatusCode::OK => {
            // The server ignored Range. Persist the restart before reading the
            // body so a short response cannot leave a later retry at the old
            // offset.
            *offset = 0;
            atomic_bytes(
                receipt,
                &serde_json::to_vec(&(url, total, 0u64)).map_err(cache_error)?,
            )?;
            file.set_len(0).map_err(cache_error)?;
            file.seek(SeekFrom::Start(0)).map_err(cache_error)?;
            (0, total)
        }
        reqwest::StatusCode::PARTIAL_CONTENT => {
            let expected_header = format!("bytes {start}-{end}/{total}");
            if response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                != Some(&expected_header)
            {
                return Err(MarketplaceError::Network(format!(
                    "unexpected Content-Range for {expected_header}"
                )));
            }
            (start, end - start + 1)
        }
        status => return Err(MarketplaceError::HttpStatus(status.as_u16())),
    };
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
    *offset = start + received;
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
