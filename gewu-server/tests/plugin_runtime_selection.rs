use gewu_protocol::release::{MarketplaceIndex, ReleaseSelectionError};
use gewu_server::plugins::{marketplace::select_for_install, runtime::current_target};
use serde_json::json;

#[test]
fn preflight_rejects_wrong_host_and_selects_profile_before_download() {
    let Ok(target) = current_target() else {
        return;
    };
    let artifact = json!({"url":"https://example.invalid/a", "sha256":"a".repeat(64), "bytes":1});
    let requirements = if target.os == "macos" {
        json!({"minimumOsVersion":"14.0"})
    } else {
        json!({"minimumGlibc":"2.28"})
    };
    let index: MarketplaceIndex = serde_json::from_value(json!({
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
    })).unwrap();
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
