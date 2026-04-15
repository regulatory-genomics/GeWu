use anyhow::Result;
use gewu_core::types::TaskStatus;
use gewu_storage::TaskStore;
use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn test_blocked_task_resume() -> Result<()> {
    let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
    let store = TaskStore::new(pool.clone()).await?;

    let task_id = "resume-test-1";

    // Step 1: Create task
    let rec = store.create_task(task_id, "explore", "find files").await?;
    assert_eq!(rec.status, TaskStatus::Ready);
    assert_eq!(rec.attempt, 0);

    // Step 2: First attempt — transitions to Running then Blocked
    store.update_status(task_id, &TaskStatus::Running, None).await?;
    store.increment_attempt(task_id).await?;

    // Simulate agent error → blocked
    store
        .update_status(task_id, &TaskStatus::Blocked, Some("missing web.fetch capability"))
        .await?;

    let rec = store.get_task(task_id).await?.unwrap();
    assert_eq!(rec.status, TaskStatus::Blocked);
    assert_eq!(rec.attempt, 1);
    assert_eq!(rec.block_reason.as_deref(), Some("missing web.fetch capability"));

    // Step 3: Resume — same task_id, new attempt
    store.update_status(task_id, &TaskStatus::Running, None).await?;
    store.increment_attempt(task_id).await?;

    let rec = store.get_task(task_id).await?.unwrap();
    assert_eq!(rec.status, TaskStatus::Running);
    assert_eq!(rec.attempt, 2);

    // Step 4: Succeed on second attempt
    let result_json = r#"{"summary":"Found 42 relevant files"}"#;
    store.set_result(task_id, result_json).await?;
    store.update_status(task_id, &TaskStatus::Succeeded, None).await?;

    let rec = store.get_task(task_id).await?.unwrap();
    assert_eq!(rec.status, TaskStatus::Succeeded);
    assert_eq!(rec.attempt, 2);
    assert_eq!(rec.result_json.as_deref(), Some(result_json));
    // block_reason cleared on status update
    assert_eq!(rec.block_reason, None);

    Ok(())
}

#[tokio::test]
async fn test_task_status_transitions() -> Result<()> {
    let pool = SqlitePoolOptions::new().connect("sqlite::memory:").await?;
    let store = TaskStore::new(pool).await?;

    store.create_task("t2", "build", "compile project").await?;

    // Ready -> Running -> Succeeded (happy path)
    store.update_status("t2", &TaskStatus::Running, None).await?;
    store.increment_attempt("t2").await?;
    store.update_status("t2", &TaskStatus::Succeeded, None).await?;

    let rec = store.get_task("t2").await?.unwrap();
    assert_eq!(rec.status, TaskStatus::Succeeded);
    assert_eq!(rec.attempt, 1);

    Ok(())
}
