//! Release, packed-runtime, and marketplace metadata contracts.
//!
//! These types deliberately live beside the existing plugin wire contract.
//! They describe distribution and installation; they do not change the
//! `plugin.v1` adapter frames.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RELEASE_SCHEMA: &str = include_str!("../schema/release.v1.schema.json");
pub const RELEASE_V2_SCHEMA: &str = include_str!("../schema/release.v2.schema.json");
pub const RUNTIME_SCHEMA: &str = include_str!("../schema/runtime.v1.schema.json");
pub const RUNTIME_PLAN_SCHEMA: &str = include_str!("../schema/runtime-plan.v1.schema.json");
pub const MARKETPLACE_SCHEMA: &str = include_str!("../schema/marketplace.v1.schema.json");
pub const MARKETPLACE_V2_SCHEMA: &str = include_str!("../schema/marketplace.v2.schema.json");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub os: String,
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeWuCompatibility {
    pub api: String,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeRef {
    pub id: String,
    pub version: String,
    pub target: Target,
    pub artifact: Artifact,
    pub manifest: Artifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub builder: String,
    pub source: String,
    pub source_commit: String,
    pub build_inputs: Vec<String>,
    pub sbom: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Revocation {
    pub reason: String,
    pub revoked_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutableHint {
    pub env: Option<String>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbeSpec {
    pub args: Vec<String>,
    pub timeout_seconds: Option<u64>,
}

/// Acquisition strategy for one runtime on one target. `kind` is the
/// discriminator; the variants carry only typed, bounded data so an Agent can
/// choose a provider without executing catalog text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RuntimeProvider {
    External {
        id: String,
        target: Target,
        executable: ExecutableHint,
        probe: ProbeSpec,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        binaries: BTreeMap<String, String>,
    },
    Managed {
        id: String,
        target: Target,
        manager: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        channels: Vec<String>,
        packages: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pip: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pip_index: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pip_extra_index: Option<String>,
        probe: ProbeSpec,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        binaries: BTreeMap<String, String>,
    },
    Bundled {
        id: String,
        version: String,
        target: Target,
        artifact: Artifact,
        manifest: Artifact,
    },
}

impl RuntimeProvider {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::External { .. } => "external",
            Self::Managed { .. } => "managed",
            Self::Bundled { .. } => "bundled",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::External { id, .. } | Self::Managed { id, .. } | Self::Bundled { id, .. } => id,
        }
    }

    pub fn target(&self) -> &Target {
        match self {
            Self::External { target, .. } | Self::Managed { target, .. } | Self::Bundled { target, .. } => target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePlan {
    pub schema_version: String,
    pub policies: Vec<String>,
    pub providers: Vec<RuntimeProvider>,
}

impl RuntimePlan {
    /// First provider compatible with `target` and `policy`, using the plan's
    /// declared preference order when the policy is `auto`.
    pub fn select(&self, policy: &str, target: &Target) -> Option<&RuntimeProvider> {
        let kinds: Vec<&str> = match policy {
            "external" | "managed" | "bundled" => vec![policy],
            _ => self.policies.iter().map(String::as_str).collect(),
        };
        kinds.iter().find_map(|kind| {
            self.providers.iter().find(|provider| provider.kind() == *kind && provider.target() == target)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Release {
    pub schema_version: String,
    pub release_id: String,
    pub plugin: String,
    pub version: String,
    pub channel: String,
    pub gewu: GeWuCompatibility,
    pub package: Artifact,
    #[serde(default)]
    pub runtimes: Vec<RuntimeRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_plan: Option<RuntimePlan>,
    pub capabilities: Vec<String>,
    pub permissions: Vec<String>,
    pub provenance: Provenance,
    pub release_notes: Option<String>,
    pub revocation: Option<Revocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketplaceIndex {
    pub schema_version: String,
    pub repository: String,
    pub generated_at: String,
    pub expires_at: String,
    pub key_id: String,
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketplaceSignature {
    pub algorithm: String,
    pub key_id: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedMarketplaceIndex {
    pub index: MarketplaceIndex,
    pub signature: MarketplaceSignature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeManifest {
    pub schema_version: String,
    pub id: String,
    pub version: String,
    pub plugin: String,
    pub target: Target,
    pub launcher: String,
    pub probe: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseSelectionError {
    NotFound,
    Revoked,
    Expired,
    IncompatibleApi,
    RuntimeUnavailable,
}

impl std::fmt::Display for ReleaseSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotFound => "release is not present in the marketplace index",
            Self::Revoked => "release has been revoked",
            Self::Expired => "marketplace index has expired",
            Self::IncompatibleApi => "release requires an incompatible plugin API",
            Self::RuntimeUnavailable => "release has no compatible runtime",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ReleaseSelectionError {}

impl MarketplaceIndex {
    /// Select a release and runtime without consulting the filesystem or
    /// executing any plugin code. Timestamps are RFC3339 strings, so the
    /// canonical UTC form sorts lexicographically.
    pub fn select(
        &self,
        plugin: &str,
        channel: &str,
        target: &Target,
        now: &str,
    ) -> Result<(&Release, &RuntimeRef), ReleaseSelectionError> {
        if now >= self.expires_at.as_str() {
            return Err(ReleaseSelectionError::Expired);
        }
        self.select_version(plugin, None, channel, target, now)
    }

    pub fn select_version(
        &self,
        plugin: &str,
        version: Option<&str>,
        channel: &str,
        target: &Target,
        now: &str,
    ) -> Result<(&Release, &RuntimeRef), ReleaseSelectionError> {
        let release = self.select_release_version(plugin, version, channel, now)?;
        let runtime = release
            .runtimes
            .iter()
            .find(|runtime| runtime.target == *target)
            .ok_or(ReleaseSelectionError::RuntimeUnavailable)?;
        Ok((release, runtime))
    }

    /// Select a release without requiring a bundled runtime. Provider-based
    /// releases may carry only a runtime plan and no packed artifact.
    pub fn select_release_version(
        &self,
        plugin: &str,
        version: Option<&str>,
        channel: &str,
        now: &str,
    ) -> Result<&Release, ReleaseSelectionError> {
        if now >= self.expires_at.as_str() {
            return Err(ReleaseSelectionError::Expired);
        }
        let release = self
            .releases
            .iter()
            .find(|release| release.plugin == plugin && release.channel == channel && version.is_none_or(|version| release.version == version))
            .ok_or(ReleaseSelectionError::NotFound)?;
        if release.revocation.is_some() {
            return Err(ReleaseSelectionError::Revoked);
        }
        if release.gewu.api != "gewu.plugin/v1" {
            return Err(ReleaseSelectionError::IncompatibleApi);
        }
        if !matches!(release.schema_version.as_str(), "gewu.release/v1" | "gewu.release/v2") {
            return Err(ReleaseSelectionError::IncompatibleApi);
        }
        Ok(release)
    }
}
