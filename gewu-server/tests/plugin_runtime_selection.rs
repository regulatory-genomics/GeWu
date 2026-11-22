use gewu_protocol::release::{MarketplaceIndex, ReleaseSelectionError};
use gewu_server::plugins::{marketplace::select_for_install, runtime::current_target};
use serde_json::json;

fn fixture() -> MarketplaceIndex {
    let target = current_target().unwrap();
    let artifact = json!({"url":"https://example.invalid/a", "sha256":"a".repeat(64), "bytes":1});
    let requirements = if target.os == "macos" {
        json!({"minimumOsVersion":"14.0"})
    } else {
        json!({"minimumGlibc":"2.28"})
    };
    serde_json::from_value(json!({
        "schemaVersion":"gewu.marketplace/v3","repository":"test","keyId":"test",
        "generatedAt":"2026-11-20T00:00:00Z","expiresAt":"2099-01-01T00:00:00Z","releases":[{
            "schemaVersion":"gewu.release/v3","releaseId":"paper-2.0.0","plugin":"paper","version":"2.0.0",
            "channel":"beta","gewu":{"api":"gewu.plugin/v1","host":">=0.1.8"},
            "requiredFeatures":["runtime-v2","webapp-origin"],"package":artifact,"runtimes":[{
                "id":"paper-pdf","version":"2.0.0","target":target,"artifact":artifact,"manifest":artifact,
                "profile":"pdf","runtimeSchema":"gewu.runtime/v2","requirements":requirements,"unpackedBytes":10,"installedBytesUpperBound":20
            }],"capabilities":[],"permissions":[],"provenance":{"builder":"test","source":"fixture",
            "sourceCommit":"aaaaaaaa","buildInputs":[],"createdAt":"2026-11-20T00:00:00Z"}
        }]
    })).unwrap()
}

#[test]
fn preflight_rejects_wrong_host_and_selects_profile_before_download() {
    let target = current_target().unwrap();
    let index = fixture();
    let select = |version, origin, profile| {
        select_for_install(
            &index, "paper", None, "beta", &target, version, "auto", origin, profile,
        )
    };
    assert_eq!(
        select("0.1.7", true, "pdf").unwrap_err(),
        ReleaseSelectionError::IncompatibleHost
    );
    assert_eq!(
        select("0.1.8", false, "pdf").unwrap_err(),
        ReleaseSelectionError::IncompatibleHost
    );
    assert_eq!(
        select("0.1.8", true, "core").unwrap_err(),
        ReleaseSelectionError::RuntimeUnavailable
    );
    assert_eq!(select("0.1.8", true, "pdf").unwrap().version, "2.0.0");
    let mut newer_platform = index.clone();
    let req = newer_platform.releases[0].runtimes[0]
        .requirements
        .as_mut()
        .unwrap();
    if target.os == "macos" {
        req.minimum_os_version = Some("99.0".into());
    } else {
        req.minimum_glibc = Some("99.0".into());
    }
    assert_eq!(
        select_for_install(
            &newer_platform,
            "paper",
            None,
            "beta",
            &target,
            "0.1.8",
            "auto",
            true,
            "pdf"
        )
        .unwrap_err(),
        ReleaseSelectionError::RuntimeUnavailable
    );
}

#[test]
fn minimum_017_keeps_version_feature_target_and_system_checks() {
    use gewu_server::plugins::marketplace::install_blocker;
    let mut index = fixture();
    let release = &mut index.releases[0];
    let target = current_target().unwrap();
    release.gewu.host = ">=0.1.7".into();
    let code = |release: &gewu_protocol::release::Release, host, origin, profile| {
        install_blocker(release, &target, host, "auto", origin, profile).map(|b| b.code)
    };
    assert_eq!(code(release, "0.1.7", true, "pdf"), None);
    assert_eq!(code(release, "0.1.6", true, "pdf"), Some("host_version"));
    assert_eq!(code(release, "0.1.7", false, "pdf"), Some("app_origin"));
    assert_eq!(
        code(release, "0.1.7", true, "core"),
        Some("runtime_profile")
    );
    release.gewu.host = ">=0.1.9".into();
    assert_eq!(code(release, "0.1.7", true, "pdf"), Some("host_version"));
    release.gewu.host = ">=0.1.7".into();
    release.required_features.push("future-feature".into());
    assert_eq!(code(release, "0.1.7", true, "pdf"), Some("host_feature"));
    release.required_features.pop();
    release.runtimes[0].target.arch = if target.arch == "aarch64" {
        "x86_64"
    } else {
        "aarch64"
    }
    .into();
    assert_eq!(code(release, "0.1.7", true, "pdf"), Some("runtime_target"));
    release.runtimes[0].target = target.clone();
    let req = release.runtimes[0].requirements.as_mut().unwrap();
    req.minimum_glibc = Some("99.0".into());
    req.minimum_os_version = Some("99.0".into());
    assert_eq!(
        code(release, "0.1.7", true, "pdf"),
        Some("runtime_requirements")
    );
}

#[test]
fn recovery_metadata_is_exact_trusted_and_does_not_replace_compatible_selection() {
    use gewu_server::plugins::{
        diagnostics::{self, Attempt},
        marketplace::select_for_install_reported,
    };
    let target = current_target().unwrap();
    let mut index = fixture();
    let guide = format!(
        "https://github.com/example/paper/blob/{}/INSTALL.md",
        "a".repeat(40)
    );
    index.releases[0]
        .provenance
        .build_inputs
        .push(format!("installation-guide={guide}"));
    let root = tempfile::tempdir().unwrap();
    let mut report = Attempt::start(
        root.path(),
        None,
        json!({"plugin":"paper"}),
        None,
        root.path(),
    )
    .unwrap();
    let id = report.id().to_string();
    let error = select_for_install_reported(
        &index,
        "paper",
        Some("2.0.0"),
        "beta",
        &target,
        "0.1.7",
        "auto",
        true,
        "pdf",
        Some(&mut report),
    )
    .unwrap_err();
    assert!(error.to_string().contains(&guide));
    let view = diagnostics::read(root.path(), &id, None).unwrap();
    assert_eq!(
        view["report"]["release"]["sources"]["installation-guide"],
        guide
    );
    assert_eq!(view["report"]["compatibility"]["code"], "host_version");
    assert!(!root.path().join("plugins").exists());
    let mut older = index.releases[0].clone();
    older.version = "1.0.0".into();
    older.release_id = "paper-1.0.0".into();
    older.gewu.host = ">=0.1.7".into();
    index.releases.push(older);
    assert_eq!(
        select_for_install_reported(
            &index, "paper", None, "beta", &target, "0.1.7", "auto", true, "pdf", None
        )
        .unwrap()
        .version,
        "1.0.0"
    );
    for invalid in ["expired", "api", "missing"] {
        let mut bad = index.clone();
        let version = if invalid == "missing" {
            "9.0.0"
        } else {
            "2.0.0"
        };
        if invalid == "expired" {
            bad.expires_at = "2000-01-01T00:00:00Z".into();
        }
        if invalid == "api" {
            bad.releases[0].gewu.api = "gewu.plugin/v99".into();
        }
        let error = select_for_install_reported(
            &bad,
            "paper",
            Some(version),
            "beta",
            &target,
            "0.1.7",
            "auto",
            true,
            "pdf",
            None,
        )
        .unwrap_err();
        assert!(!error.to_string().contains(&guide), "{invalid}: {error}");
    }
}
