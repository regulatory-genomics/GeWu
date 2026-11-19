use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gewu_server::plugins::artifacts::sha256_bytes;
use gewu_server::plugins::marketplace::{MarketplaceClient, MarketplaceRepository};
use ring::signature::{Ed25519KeyPair, KeyPair};

fn repository() -> (MarketplaceRepository, Vec<u8>, Vec<u8>) {
    let seed = [7u8; 32];
    let keypair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
    let index = br#"{"schemaVersion":"gewu.marketplace/v1","repository":"official","generatedAt":"2026-11-19T00:00:00Z","expiresAt":"2026-12-19T00:00:00Z","keyId":"official-v1","releases":[]}"#;
    let signature = BASE64.encode(keypair.sign(index).as_ref()).into_bytes();
    let repository = MarketplaceRepository {
        id: "official".into(), index_url: "http://127.0.0.1:9/index.v1.json".into(), signature_url: "http://127.0.0.1:9/index.v1.json.sig".into(),
        public_key: keypair.public_key().as_ref().to_vec(), key_id: "official-v1".into(),
    };
    (repository, index.to_vec(), signature)
}

#[test]
fn valid_signed_cache_is_available_offline_and_tampering_is_rejected() {
    let (repository, index, signature) = repository();
    let temp = tempfile::tempdir().unwrap();
    let client = MarketplaceClient::new(repository, temp.path());
    assert_eq!(client.cache(&index, &signature).unwrap().repository, "official");
    assert_eq!(client.load_cached().unwrap().releases.len(), 0);
    let mut tampered = index.clone();
    tampered[10] ^= 1;
    assert!(client.cache(&tampered, &signature).is_err());
}

#[test]
fn oversized_cache_and_wrong_key_are_blocked() {
    let (mut repository, index, signature) = repository();
    repository.public_key[0] ^= 1;
    let client = MarketplaceClient::new(repository, tempfile::tempdir().unwrap().path()).with_max_bytes(index.len() - 1);
    assert!(client.cache(&index, &signature).is_err());
    let _ = sha256_bytes(&index);
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
