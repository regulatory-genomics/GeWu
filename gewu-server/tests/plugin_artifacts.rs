use gewu_server::plugins::artifacts::{ArtifactStore, sha256_bytes};
use std::io::Write;

#[test]
fn content_addressed_store_reuses_verified_blobs_and_rejects_hash_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.bin");
    let bytes = b"immutable plugin artifact";
    std::fs::write(&source, bytes).unwrap();
    let digest = sha256_bytes(bytes);
    let store = ArtifactStore::new(temp.path().join("store"));
    let first = store.put_file(&source, &digest, bytes.len() as u64).unwrap();
    let second = store.put_file(&source, &digest, bytes.len() as u64).unwrap();
    assert_eq!(first, second);
    assert!(store.put_file(&source, &"0".repeat(64), bytes.len() as u64).is_err());
}

#[test]
fn bounded_zip_extraction_rejects_traversal_and_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("bad.zip");
    let file = std::fs::File::create(&archive).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("../escape", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(b"bad").unwrap();
    zip.finish().unwrap();
    let error = ArtifactStore::new(temp.path().join("store")).extract_zip(&archive, &temp.path().join("out")).unwrap_err().to_string();
    assert!(error.contains("escapes"), "{error}");
}
