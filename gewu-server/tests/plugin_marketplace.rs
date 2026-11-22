use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::stream;
use gewu_protocol::release::Artifact;
use gewu_server::plugins::artifacts::{ArtifactStore, sha256_bytes};
use gewu_server::plugins::diagnostics::{self, Attempt};
use gewu_server::plugins::marketplace::{MarketplaceClient, MarketplaceRepository};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn repository() -> (MarketplaceRepository, Vec<u8>, Vec<u8>) {
    let seed = [7u8; 32];
    let keypair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let index = br#"{"schemaVersion":"gewu.marketplace/v1","repository":"official","generatedAt":"2026-11-19T00:00:00Z","expiresAt":"2026-12-19T00:00:00Z","keyId":"official-v1","releases":[]}"#;
    let signature = BASE64.encode(keypair.sign(index).as_ref()).into_bytes();
    let repository = MarketplaceRepository {
        id: "official".into(),
        index_url: "http://127.0.0.1:9/index.v1.json".into(),
        signature_url: "http://127.0.0.1:9/index.v1.json.sig".into(),
        public_key: keypair.public_key().as_ref().to_vec(),
        key_id: "official-v1".into(),
    };
    (repository, index.to_vec(), signature)
}

#[test]
fn valid_signed_cache_is_available_offline_and_tampering_is_rejected() {
    let (repository, index, signature) = repository();
    let temp = tempfile::tempdir().unwrap();
    let client = MarketplaceClient::new(repository, temp.path());
    assert_eq!(
        client.cache(&index, &signature).unwrap().repository,
        "official"
    );
    assert_eq!(client.load_cached().unwrap().releases.len(), 0);
    let mut tampered = index.clone();
    tampered[10] ^= 1;
    assert!(client.cache(&tampered, &signature).is_err());
}

#[test]
fn oversized_cache_and_wrong_key_are_blocked() {
    let (mut repository, index, signature) = repository();
    repository.public_key[0] ^= 1;
    let client = MarketplaceClient::new(repository, tempfile::tempdir().unwrap().path())
        .with_max_bytes(index.len() - 1);
    assert!(client.cache(&index, &signature).is_err());
    let _ = sha256_bytes(&index);
}

#[test]
fn a_valid_signature_does_not_bypass_marketplace_schema_validation() {
    let (repository, _, _) = repository();
    let invalid = br#"{"schemaVersion":"gewu.marketplace/v1","repository":"official","generatedAt":"2026-11-19T00:00:00Z","expiresAt":"","keyId":"official-v1","releases":[]}"#;
    let keypair = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
    let signature = BASE64.encode(keypair.sign(invalid).as_ref()).into_bytes();
    let client = MarketplaceClient::new(repository, tempfile::tempdir().unwrap().path());
    assert!(client.cache(invalid, &signature).is_err());
}

#[test]
fn artifact_origin_rewrite_changes_only_scheme_host_and_port() {
    let rewritten = gewu_server::plugins::marketplace::rewrite_origin(
        "http://172.16.211.53:3000/gilbert/gewu-marketplace/releases/download/a/b.zip",
        "http://127.0.0.1:19080",
    )
    .unwrap();
    assert_eq!(
        rewritten,
        "http://127.0.0.1:19080/gilbert/gewu-marketplace/releases/download/a/b.zip"
    );
}

#[derive(Clone)]
struct RangeState {
    bytes: Arc<Vec<u8>>,
    first_request: Arc<AtomicUsize>,
    fail_tail: Arc<std::sync::atomic::AtomicBool>,
}

async fn ranged_artifact(State(state): State<RangeState>, headers: HeaderMap) -> impl IntoResponse {
    let Some(range) = headers.get("range").and_then(|value| value.to_str().ok()) else {
        return (
            StatusCode::OK,
            [("content-range", String::new())],
            state.bytes.as_ref().clone(),
        );
    };
    let (start, end) = range
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let start: usize = start.parse().unwrap();
    if start != 0 && state.fail_tail.load(Ordering::SeqCst) {
        return (
            StatusCode::FORBIDDEN,
            [("content-range", String::new())],
            vec![],
        );
    }
    let requested_end: usize = end.parse().unwrap();
    let end = requested_end.min(state.bytes.len() - 1);
    let mut body_end = end;
    if start == 0 && state.first_request.fetch_add(1, Ordering::SeqCst) == 0 {
        body_end = start + 127;
    }
    let body = state.bytes[start..=body_end].to_vec();
    let headers = [(
        "content-range",
        format!("bytes {start}-{body_end}/{}", state.bytes.len()),
    )];
    (StatusCode::PARTIAL_CONTENT, headers, body)
}

#[tokio::test]
async fn large_artifacts_retry_short_ranges_and_verify_the_complete_digest() {
    let bytes: Vec<u8> = (0..(32 * 1024 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect();
    let state = RangeState {
        bytes: Arc::new(bytes.clone()),
        first_request: Arc::new(AtomicUsize::new(0)),
        fail_tail: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/artifact", get(ranged_artifact))
                .with_state(state),
        )
        .into_future(),
    );
    let temp = tempfile::tempdir().unwrap();
    let repository = MarketplaceRepository {
        id: "official".into(),
        index_url: "http://127.0.0.1:9/index".into(),
        signature_url: "http://127.0.0.1:9/signature".into(),
        public_key: vec![0; 32],
        key_id: "official-v1".into(),
    };
    let client = MarketplaceClient::new(repository, temp.path());
    let store = ArtifactStore::new(temp.path().join("store"));
    let artifact = Artifact {
        url: format!("http://{address}/artifact"),
        sha256: sha256_bytes(&bytes),
        bytes: bytes.len() as u64,
    };
    let mut report = Attempt::start(
        temp.path(),
        None,
        serde_json::json!({"plugin":"demo"}),
        None,
        temp.path(),
    )
    .unwrap();
    let attempt = report.id().to_string();
    report.stage("runtime_download");
    let path = client
        .download_artifact(&artifact, &store, Some(&mut report))
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert!(store.root().join("sha256").exists());
    let persisted = diagnostics::read(temp.path(), &attempt, None).unwrap();
    assert_eq!(
        persisted["report"]["progress"]["totalBytes"],
        bytes.len() as u64
    );
    assert_eq!(
        persisted["report"]["progress"]["completedBytes"],
        bytes.len() as u64
    );
    server.abort();
    let cached = client
        .download_artifact(&artifact, &store, None)
        .await
        .unwrap();
    assert_eq!(std::fs::read(cached).unwrap(), bytes);
}

#[tokio::test]
async fn a_new_client_resumes_after_the_last_complete_range() {
    let bytes = vec![42; 32 * 1024 * 1024 + 1];
    let state = RangeState {
        bytes: Arc::new(bytes.clone()),
        first_request: Arc::new(AtomicUsize::new(1)),
        fail_tail: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let artifact = Artifact {
        url: format!("http://{}/artifact", listener.local_addr().unwrap()),
        sha256: sha256_bytes(&bytes),
        bytes: bytes.len() as u64,
    };
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/artifact", get(ranged_artifact))
                .with_state(state.clone()),
        )
        .into_future(),
    );
    let temp = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(temp.path().join("store"));
    let client = || MarketplaceClient::new(repository().0, temp.path());
    assert!(
        client()
            .download_artifact(&artifact, &store, None)
            .await
            .is_err()
    );
    state.fail_tail.store(false, Ordering::SeqCst);
    let path = client()
        .download_artifact(&artifact, &store, None)
        .await
        .unwrap();
    assert_eq!(state.first_request.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    server.abort();
}

#[derive(Clone)]
struct FallbackState {
    bytes: Arc<Vec<u8>>,
    starts: Arc<Mutex<Vec<usize>>>,
    trick: Arc<AtomicUsize>,
}

async fn ignore_one_range(State(state): State<FallbackState>, headers: HeaderMap) -> Response {
    let range = headers
        .get("range")
        .and_then(|value| value.to_str().ok())
        .unwrap();
    let (start, end) = range
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let start: usize = start.parse().unwrap();
    state.starts.lock().unwrap().push(start);
    if start > 0 && state.trick.fetch_add(1, Ordering::SeqCst) == 0 {
        // A proxy that ignores Range must not poison a later resume: this
        // complete but short 200 body is shorter than the advertised artifact.
        return (StatusCode::OK, Body::from(vec![state.bytes[0]])).into_response();
    }
    let end = end.parse::<usize>().unwrap().min(state.bytes.len() - 1);
    let body = state.bytes[start..=end].to_vec();
    (
        StatusCode::PARTIAL_CONTENT,
        [(
            "content-range",
            format!("bytes {start}-{end}/{}", state.bytes.len()),
        )],
        Body::from_stream(stream::once(async move { Ok::<_, std::io::Error>(body) })),
    )
        .into_response()
}

#[tokio::test]
async fn a_range_ignored_by_the_server_restarts_cleanly_after_a_short_body() {
    let bytes: Vec<u8> = (0..(32 * 1024 * 1024 + 1))
        .map(|index| (index % 251) as u8)
        .collect();
    let state = FallbackState {
        bytes: Arc::new(bytes.clone()),
        starts: Default::default(),
        trick: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/artifact", get(ignore_one_range))
                .with_state(state.clone()),
        )
        .into_future(),
    );
    let temp = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(temp.path().join("store"));
    let client = MarketplaceClient::new(repository().0, temp.path());
    let artifact = Artifact {
        url: format!("http://{address}/artifact"),
        sha256: sha256_bytes(&bytes),
        bytes: bytes.len() as u64,
    };
    let path = client
        .download_artifact(&artifact, &store, None)
        .await
        .expect("a truncated 200 response must restart from zero");
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let starts = state.starts.lock().unwrap().clone();
    assert_eq!(starts[0], 0);
    assert!(starts.iter().skip(1).any(|start| *start == 0), "{starts:?}");
    server.abort();
}
