#![cfg(target_os = "linux")]
use gewu_tools::process::{ProcessScope, read_frame};
use std::{process::Stdio, time::Duration};
use tokio::{io::BufReader, process::Command};

#[tokio::test]
async fn cleans_owned_groups_and_rejects_unrelated_identity() {
    let mut command = Command::new("python3");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/process_tree.py"
        ))
        .stdout(Stdio::piped());
    let mut scope = ProcessScope::spawn(command).unwrap();
    let mut stdout = BufReader::new(scope.child_mut().stdout.take().unwrap());
    let pid: u32 = String::from_utf8(read_frame(&mut stdout, 128).await.unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        scope
            .register(std::process::id(), std::process::id())
            .is_err()
    );
    scope.register(pid, pid).unwrap();
    scope.stop(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn bounded_frame_reader_does_not_buffer_a_flood() {
    let mut reader = BufReader::new(&b"abc\ndefgh"[..]);
    assert_eq!(read_frame(&mut reader, 3).await.unwrap().unwrap(), b"abc");
    assert!(read_frame(&mut reader, 3).await.is_err());
}
