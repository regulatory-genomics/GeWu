//! Tool execution utilities.
//!
//! This module provides utility functions for safe tool execution,
//! including panic recovery and error handling.

use crate::types::{ToolContext, ToolResult};
use anyhow::Result;
use std::error::Error;
use tokio::task::JoinHandle;

use super::extract_panic_message;

/// Executes a tool with panic recovery.
///
/// This function spawns the tool execution as a separate task and catches panics,
/// converting them into error return values rather than propagating as unhandled panics.
///
/// # Arguments
///
/// * `tool` - The tool to execute
/// * `args` - JSON arguments for the tool
/// * `ctx` - Execution context
///
/// # Returns
///
/// Returns the tool's result on success, or an error if the tool panicked or
/// the task was aborted.
///
/// # Design Notes
///
/// We spawn the tool as a separate task. Tokio automatically catches panics in
/// spawned tasks and stores them in the JoinHandle. When we await the handle,
/// we get a `JoinError` if the task panicked.
///
/// This avoids the nested `block_on` anti-pattern where blocking inside
/// `spawn_blocking` can cause deadlocks when the blocking thread pool is exhausted.
pub async fn execute_tool_with_panic_recovery(
    tool: std::sync::Arc<dyn crate::traits::Tool>,
    args: serde_json::Value,
    ctx: ToolContext,
) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
    let tool_name = tool.name().to_string();

    // Spawn the async tool execution as a separate task.
    // Tokio will catch any panics and store them in the JoinHandle.
    let handle: JoinHandle<Result<ToolResult, Box<dyn Error + Send + Sync>>> =
        tokio::spawn(async move { tool.execute(args, &ctx).await });

    // Await the result. Tokio's JoinHandle catches panics automatically.
    match handle.await {
        // Task completed successfully with a result
        Ok(Ok(res)) => Ok(res),
        // Tool returned an error (not a panic)
        Ok(Err(e)) => Err(e),
        // Task panicked (Tokio caught the panic and stored it in JoinError)
        Err(join_err) if join_err.is_panic() => {
            // Try to extract the panic payload
            match join_err.try_into_panic() {
                Ok(panic_payload) => {
                    let panic_msg = extract_panic_message(panic_payload);
                    Err(Box::new(std::io::Error::other(
                        format!(
                            "Tool '{}' panicked during execution: {}. This may indicate an issue \
                            with the tool implementation or invalid input. Consider adjusting the \
                            tool arguments or checking the tool's requirements.",
                            tool_name, panic_msg
                        ),
                    )) as Box<dyn Error + Send + Sync>)
                }
                // Could not retrieve panic payload (rare, e.g., if payload is !Send)
                Err(_) => Err(Box::new(std::io::Error::other(
                    format!(
                        "Tool '{}' panicked but could not retrieve panic message",
                        tool_name
                    ),
                )) as Box<dyn Error + Send + Sync>),
            }
        }
        // Task was cancelled (e.g., runtime shutdown)
        Err(_) => Err(Box::new(std::io::Error::other(
            format!("Tool '{}' task was cancelled", tool_name),
        )) as Box<dyn Error + Send + Sync>),
    }
}
