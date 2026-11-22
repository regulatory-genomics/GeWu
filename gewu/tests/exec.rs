use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::{io::AsyncWriteExt, net::TcpListener, process::Command};
use tokio_tungstenite::{accept_async, tungstenite::Message};

async fn run(script: &str, json_output: bool) -> std::process::Output {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gewu"));
    cmd.env_remove("GEWU_SERVE_TOKEN")
        .args(["exec", "--server", &url]);
    if json_output {
        cmd.arg("--json");
    }
    let mut child = cmd
        .arg("summarize")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all("中文".as_bytes())
        .await
        .unwrap();
    let mut ws = accept_async(listener.accept().await.unwrap().0)
        .await
        .unwrap();
    let ask: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(ask["prompt"], "summarize\n\n<stdin>\n中文\n</stdin>");
    let ready =
        json!({"id":ask["id"], "type":"ready","request_id":"r","session_id":"s","agent":"build"});
    ws.send(Message::text(ready.to_string())).await.unwrap();
    let result = match script {
        "ok" => {
            json!({"type":"result","id":null,"request_id":"r","session_id":"s","response":"done"})
        }
        "question" => {
            json!({"type":"event","id":null,"request_id":"r","event":{"type":"question_asked","request_id":"q","questions":[{"header":"Choice","question":"which file?"}]}})
        }
        _ => json!({"type":"error","error":"failed"}),
    };
    ws.send(Message::text(result.to_string())).await.unwrap();
    if script == "question" {
        let req: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(req["op"], "interrupt");
        ws.send(Message::text(
            json!({"type":"interrupted","id":req["id"],"request_id":"r"}).to_string(),
        ))
        .await
        .unwrap();
    }
    child.wait_with_output().await.unwrap()
}

#[tokio::test]
async fn exec_pipe_and_plain_stdout() {
    let output = run("ok", false).await;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"done\n");
}

#[tokio::test]
async fn json_has_terminal_record_for_success_failure_and_question() {
    for (script, code, kind) in [
        ("ok", 0, "result"),
        ("error", 1, "error"),
        ("question", 3, "error"),
    ] {
        let result = run(script, true).await;
        assert_eq!(result.status.code(), Some(code));
        let records: Vec<Value> = std::str::from_utf8(&result.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[0]["type"], "started");
        assert_eq!(records.last().unwrap()["type"], kind);
        assert!(records.iter().all(|r| r["session_id"] == "s"));
    }
}
