use gewu_canvas::invocation_poller::InvocationPoller;

#[tokio::test]
async fn poll_returns_empty_when_no_pending() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/api/workflow/invocations/pending")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    let poller = InvocationPoller::new(&server.url());
    let result = poller.poll().await.unwrap();
    assert!(result.is_empty());
    mock.assert_async().await;
}

#[tokio::test]
async fn poll_returns_pending_invocation() {
    let mut server = mockito::Server::new_async().await;
    let body = r###"[{
        "invocation_id": "inv_001",
        "agent_id": "canvas-analyst",
        "briefing": "## Trigger\nUser prompt: run QC on SM30\n## Selection\n- ucf_SM30_1 (data)\n",
        "selected_nodes": ["ucf_SM30_1", "ucf_SM30_2"]
    }]"###;
    let mock = server
        .mock("GET", "/api/workflow/invocations/pending")
        .with_status(200)
        .with_body(body)
        .create_async()
        .await;

    let poller = InvocationPoller::new(&server.url());
    let result = poller.poll().await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].invocation_id, "inv_001");
    assert_eq!(result[0].agent_id, "canvas-analyst");
    assert!(result[0].briefing.contains("run QC on SM30"));
    assert_eq!(result[0].selected_nodes, vec!["ucf_SM30_1", "ucf_SM30_2"]);
    mock.assert_async().await;
}
