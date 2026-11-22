#![cfg(unix)]
use gewu_protocol::plugin::*;
use gewu_server::plugins::stdio::{Adapter, Limits};
use std::path::Path;
use tokio::process::Command;

#[tokio::test]
async fn cancel_ack_does_not_replace_terminal_and_adapter_exits() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-adapter");
    let mut command = Command::new("python3");
    command.arg(directory.join("adapter.py"));
    command.env(
        "PYTHONPATH",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk/python"),
    );
    let activation = Activation {
        api_version: ApiVersion::V1,
        instance_id: "i".into(),
        generation: 1,
        workspace_id: "w".into(),
        workspace_root: directory.display().to_string(),
        session_id: None,
        binding_resource: None,
        settings: Object::new(),
    };
    let (adapter, _) = Adapter::start(command, activation, Limits::default(), None)
        .await
        .unwrap();
    let (progress, mut events) = tokio::sync::mpsc::channel(2);
    let call = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            adapter
                .request(
                    Operation::Call {
                        tool: "wait".into(),
                        args: Object::new(),
                        context: Box::new(CallContext {
                            instance_id: "i".into(),
                            generation: 1,
                            workspace_id: "w".into(),
                            call_id: "c".into(),
                            session_id: None,
                            run_id: None,
                            binding_resource: None,
                            resource: None,
                            view_id: None,
                            view_epoch: None,
                        }),
                    },
                    Some(progress),
                )
                .await
                .unwrap()
        })
    };
    assert!(matches!(events.recv().await, Some(Event::Progress { .. })));
    let ack = adapter
        .request(
            Operation::Cancel {
                call_id: "c".into(),
            },
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        ack,
        Response::Cancelled {
            state: CancelState::Requested
        }
    ));
    assert!(
        matches!(call.await.unwrap(), Response::Called(ToolOutput::Failure {error, ..}) if error.code == "CANCELLED")
    );
    adapter.shutdown("test").await.unwrap();
}
