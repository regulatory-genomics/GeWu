use gewu_protocol::release::*;
use jsonschema::validator_for;
use serde_json::Value;

fn valid() -> Value {
    serde_json::from_str(include_str!("fixtures/release-v1/valid.json")).unwrap()
}

#[test]
fn release_runtime_and_marketplace_fixtures_match_their_schemas() {
    let release: Value = serde_json::from_str(include_str!("fixtures/release-v1/valid.json")).unwrap();
    let runtime: Value = serde_json::from_str(include_str!("fixtures/release-v1/runtime.json")).unwrap();
    let marketplace: Value = serde_json::from_str(include_str!("fixtures/release-v1/marketplace.json")).unwrap();
    assert!(validator_for(&serde_json::from_str(RELEASE_SCHEMA).unwrap()).unwrap().is_valid(&release));
    assert!(validator_for(&serde_json::from_str(RUNTIME_SCHEMA).unwrap()).unwrap().is_valid(&runtime));
    assert!(validator_for(&serde_json::from_str(MARKETPLACE_SCHEMA).unwrap()).unwrap().is_valid(&marketplace));
    serde_json::from_value::<Release>(release).unwrap();
    serde_json::from_value::<RuntimeManifest>(runtime).unwrap();
    serde_json::from_value::<SignedMarketplaceIndex>(marketplace).unwrap();
}

#[test]
fn invalid_release_fixtures_are_rejected_deterministically() {
    let schema = validator_for(&serde_json::from_str(RELEASE_SCHEMA).unwrap()).unwrap();
    for name in ["stale", "tampered", "incompatible", "revoked", "missing-runtime"] {
        let contents = std::fs::read_to_string(format!(
            "{}/tests/fixtures/release-v1/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let value: Value = serde_json::from_str(&contents).unwrap();
        assert!(!schema.is_valid(&value), "fixture {name} unexpectedly matches schema");
    }
}

#[test]
fn provider_release_and_plan_match_schemas() {
    let release: Value =
        serde_json::from_str(include_str!("fixtures/release-v2/valid.json")).unwrap();
    let plan: Value =
        serde_json::from_str(include_str!("fixtures/release-v2/runtime-plan.json")).unwrap();
    assert!(
        validator_for(&serde_json::from_str(RELEASE_V2_SCHEMA).unwrap())
            .unwrap()
            .is_valid(&release)
    );
    assert!(
        validator_for(&serde_json::from_str(RUNTIME_PLAN_SCHEMA).unwrap())
            .unwrap()
            .is_valid(&plan)
    );
    let release: Release = serde_json::from_value(release).unwrap();
    assert!(release.runtimes.is_empty());
    assert_eq!(release.runtime_plan.as_ref().unwrap().providers.len(), 2);
    let plan: RuntimePlan = serde_json::from_value(plan).unwrap();
    let linux = Target { os: "linux".into(), arch: "x86_64".into(), libc: Some("glibc".into()) };
    let macos = Target { os: "macos".into(), arch: "aarch64".into(), libc: None };
    assert_eq!(plan.select("auto", &linux).unwrap().kind(), "external");
    assert_eq!(plan.select("managed", &linux).unwrap().kind(), "managed");
    assert_eq!(plan.select("bundled", &linux).unwrap().kind(), "bundled");
    assert!(plan.select("managed", &macos).is_none());
}

#[test]
fn provider_release_selects_without_a_bundled_runtime() {
    let release: Release =
        serde_json::from_str(include_str!("fixtures/release-v2/valid.json")).unwrap();
    let index = MarketplaceIndex {
        schema_version: "gewu.marketplace/v2".into(),
        repository: "official".into(),
        generated_at: "2026-11-19T00:00:00Z".into(),
        expires_at: "2026-12-19T00:00:00Z".into(),
        key_id: "official-v1".into(),
        releases: vec![release],
    };
    let target = Target { os: "linux".into(), arch: "x86_64".into(), libc: Some("glibc".into()) };
    let release = index
        .select_release_version("manimble", None, "stable", "2026-11-19T00:00:00Z")
        .unwrap();
    assert_eq!(release.runtime_plan.as_ref().unwrap().providers.len(), 2);
    assert_eq!(
        index.select_version("manimble", None, "stable", &target, "2026-11-19T00:00:00Z").err(),
        Some(ReleaseSelectionError::RuntimeUnavailable)
    );
}

#[test]
fn selection_reports_expiry_revocation_and_runtime_mismatch() {
    let release: Release = serde_json::from_value(valid()).unwrap();
    let index = MarketplaceIndex {
        schema_version: "gewu.marketplace/v1".into(),
        repository: "official".into(),
        generated_at: "2026-11-19T00:00:00Z".into(),
        expires_at: "2026-12-19T00:00:00Z".into(),
        key_id: "official-v1".into(),
        releases: vec![release],
    };
    let target = Target { os: "linux".into(), arch: "x86_64".into(), libc: Some("glibc-2.28".into()) };
    assert!(matches!(index.select("paper", "stable", &target, "2026-11-19T00:00:00Z"), Ok(_)));
    assert_eq!(index.select("paper", "stable", &target, "2027-01-01T00:00:00Z"), Err(ReleaseSelectionError::Expired));
    let wrong = Target { os: "macos".into(), arch: "aarch64".into(), libc: None };
    assert_eq!(index.select("paper", "stable", &wrong, "2026-11-19T00:00:00Z"), Err(ReleaseSelectionError::RuntimeUnavailable));
}
