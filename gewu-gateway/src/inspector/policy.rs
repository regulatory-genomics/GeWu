//! Exposure policy for inspector snapshots.
//!
//! Controls how much detail the inspector reveals based on request context.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Security level controlling snapshot detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExposureLevel {
    /// Names, descriptions, status, topology edges, redacted paths.
    #[default]
    PublicSummary,
    /// Source paths, workflow node details, tool schemas, error details.
    LocalDebug,
    /// Full prompts, full logs, full config. Never enabled by default.
    RawInternal,
}

impl ExposureLevel {
    pub fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("local_debug") => Self::LocalDebug,
            Some("raw_internal") => Self::RawInternal,
            _ => Self::PublicSummary,
        }
    }

    /// Determine effective level: requested level is capped by what the
    /// caller is allowed based on origin.
    pub fn effective(requested: Self, remote_addr: Option<IpAddr>, _headers: &HeaderMap) -> Self {
        let max_allowed = if is_local(remote_addr) {
            Self::LocalDebug
        } else {
            Self::PublicSummary
        };
        cap(requested, max_allowed)
    }

    pub fn allows_source_paths(self) -> bool {
        matches!(self, Self::LocalDebug | Self::RawInternal)
    }

    pub fn allows_full_prompts(self) -> bool {
        matches!(self, Self::RawInternal)
    }

    pub fn allows_full_output(self) -> bool {
        matches!(self, Self::LocalDebug | Self::RawInternal)
    }

    pub fn allows_error_details(self) -> bool {
        matches!(self, Self::LocalDebug | Self::RawInternal)
    }
}

fn is_local(addr: Option<IpAddr>) -> bool {
    match addr {
        Some(IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private(),
        Some(IpAddr::V6(v6)) => v6.is_loopback(),
        None => true, // unix socket or unknown → treat as local
    }
}

fn cap(requested: ExposureLevel, max: ExposureLevel) -> ExposureLevel {
    let rank = |l: ExposureLevel| match l {
        ExposureLevel::PublicSummary => 0,
        ExposureLevel::LocalDebug => 1,
        ExposureLevel::RawInternal => 2,
    };
    if rank(requested) <= rank(max) {
        requested
    } else {
        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn from_query_defaults_to_public() {
        assert_eq!(
            ExposureLevel::from_query(None),
            ExposureLevel::PublicSummary
        );
        assert_eq!(
            ExposureLevel::from_query(Some("garbage")),
            ExposureLevel::PublicSummary
        );
    }

    #[test]
    fn from_query_parses_levels() {
        assert_eq!(
            ExposureLevel::from_query(Some("local_debug")),
            ExposureLevel::LocalDebug
        );
        assert_eq!(
            ExposureLevel::from_query(Some("raw_internal")),
            ExposureLevel::RawInternal
        );
    }

    #[test]
    fn effective_caps_remote_to_public() {
        let remote = Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        let headers = HeaderMap::new();
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::LocalDebug, remote, &headers),
            ExposureLevel::PublicSummary
        );
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::RawInternal, remote, &headers),
            ExposureLevel::PublicSummary
        );
    }

    #[test]
    fn effective_allows_local_debug_for_loopback() {
        let local = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let headers = HeaderMap::new();
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::LocalDebug, local, &headers),
            ExposureLevel::LocalDebug
        );
    }

    #[test]
    fn effective_allows_local_debug_for_private() {
        let private = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        let headers = HeaderMap::new();
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::LocalDebug, private, &headers),
            ExposureLevel::LocalDebug
        );
    }

    #[test]
    fn effective_caps_raw_internal_for_local() {
        let local = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let headers = HeaderMap::new();
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::RawInternal, local, &headers),
            ExposureLevel::LocalDebug
        );
    }

    #[test]
    fn effective_treats_none_as_local() {
        let headers = HeaderMap::new();
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::LocalDebug, None, &headers),
            ExposureLevel::LocalDebug
        );
    }

    #[test]
    fn ipv6_loopback_is_local() {
        let v6 = Some(IpAddr::V6(Ipv6Addr::LOCALHOST));
        let headers = HeaderMap::new();
        assert_eq!(
            ExposureLevel::effective(ExposureLevel::LocalDebug, v6, &headers),
            ExposureLevel::LocalDebug
        );
    }

    #[test]
    fn permission_helpers() {
        assert!(!ExposureLevel::PublicSummary.allows_source_paths());
        assert!(ExposureLevel::LocalDebug.allows_source_paths());
        assert!(ExposureLevel::RawInternal.allows_source_paths());

        assert!(!ExposureLevel::PublicSummary.allows_full_prompts());
        assert!(!ExposureLevel::LocalDebug.allows_full_prompts());
        assert!(ExposureLevel::RawInternal.allows_full_prompts());
    }
}
