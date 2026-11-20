use gewu_core::{traits::Tool, types::ToolContext};
use gewu_lab::{Lab, LabProjectTool, ProjectTaskBoardTool};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn routes_messages_and_keeps_board_edits_project_scoped() {
    use gewu_lab::AgentRunner;
    let lab = Arc::new(Mutex::new(Lab::new()));
    let tool = LabProjectTool::new(lab.clone());
    let ctx = ToolContext::new("lab", "message");
    let first_id = {
        let result = tool
            .execute(json!({"action":"list_projects"}), &ctx)
            .await
            .unwrap();
        let projects: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(projects["projects"].as_array().unwrap().len(), 0);
        let result = tool
            .execute(
                json!({"action":"create_project","name":"Alpha","goal":"Explore"}),
                &ctx,
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        output["id"].as_str().unwrap().to_owned()
    };
    tool.execute(
        json!({"action":"create_project","name":"Alpha","goal":"Second"}),
        &ctx,
    )
    .await
    .unwrap();
    let listing = tool
        .execute(json!({"action":"list_projects"}), &ctx)
        .await
        .unwrap();
    let projects: serde_json::Value = serde_json::from_str(&listing.output).unwrap();
    assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
    assert!(projects.to_string().contains("wiki"));
    tool.execute(json!({"action":"create_project","name":"","goal":""}), &ctx)
        .await
        .unwrap_err();
    // Without a runner, ask_project_agent falls back to the queue.
    tool.execute(
        json!({"action":"ask_project_agent","id":first_id,"message":"first"}),
        &ctx,
    )
    .await
    .unwrap();
    tool.execute(
        json!({"action":"ask_project_agent","id":"missing","message":"x"}),
        &ctx,
    )
    .await
    .unwrap_err();
    {
        let mut lab = lab.lock().unwrap();
        let second_id = "Alpha-1";
        assert_eq!(lab.take_project_message(second_id).unwrap(), None);
        assert_eq!(
            lab.take_project_message(&first_id).unwrap().as_deref(),
            Some("first")
        );
    }
    // With a runner, ask_project_agent waits for the reply.
    let runner: AgentRunner =
        Arc::new(|id, message| Box::pin(async move { Ok(format!("{id} heard: {message}")) }));
    let sync_tool = LabProjectTool::new(lab.clone()).with_project_runner(runner);
    let replied = sync_tool
        .execute(
            json!({"action":"ask_project_agent","id":first_id,"message":"status?"}),
            &ctx,
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&replied.output).unwrap();
    assert_eq!(output["status"], "replied");
    assert_eq!(output["reply"], format!("{first_id} heard: status?"));
    let board = ProjectTaskBoardTool::new(lab.clone(), &first_id);
    for (id, dependencies) in [("first", vec![]), ("second", vec!["first"])] {
        board.execute(json!({"action":"add","task_id":id,"name":id,"deliverable":"notes","dependencies":dependencies}), &ctx).await.unwrap();
    }
    let created = board
        .execute(
            json!({"action":"create_task","name":"Write","depends_on":["second"]}),
            &ctx,
        )
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&created.output).unwrap();
    assert_eq!(output["task_id"], "task-3");
    board
        .execute(json!({"action":"create_task","name":"  "}), &ctx)
        .await
        .unwrap_err();
    assert!(board.execute(json!({"action":"revise","task_id":"first","name":"changed","deliverable":"notes","dependencies":["second"]}), &ctx).await.is_err());
    assert!(
        board
            .execute(json!({"action":"list_task"}), &ctx)
            .await
            .is_ok()
    );
    let lab = lab.lock().unwrap();
    assert!(lab.project("b").is_none());
    assert_eq!(
        lab.project(&first_id).unwrap().task("first").unwrap().name,
        "first"
    );
    assert_eq!(
        lab.project(&first_id).unwrap().task("task-3").unwrap().name,
        "Write"
    );
    let beta = lab.project(&first_id).unwrap();
    assert_eq!(beta.name(), "Alpha");
    assert_eq!(beta.goal(), "Explore");
}
