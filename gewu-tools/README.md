# gewu-tools

`gewu-tools` provides a collection of utility tools for the GeWu agent framework. It also includes support for loading MCP (Model Context Protocol) servers as tools.

## Tools

### Available Tools

- **`ApplyPatchTool`** (`apply_patch`): Applies validated multi-file add, update, delete, and move operations.
- **`ReadFileTool`**, **`WriteFileTool`**, and **`EditFileTool`**: Read and modify workspace files.
- **`GlobTool`**, **`GrepTool`**, and **`ListTool`**: Search and inspect the workspace.
- **`BashTool`**: Executes shell commands in a controlled working directory.
- **`WebFetchTool`** and **`WebSearchTool`**: Fetch and search external information.
- **`TodoWriteTool`**: Maintains the agent task list (echoes the full list back).
- **`TaskTool`**: Launches sub-agents (build/explore/general) for complex tasks; resumable via `task_id`.
- **`QuestionTool`** (`question`): Asks the user structured multiple-choice questions mid-run. It needs a `QuestionBroker` and an interactive front-end (the server GUI wires one); CLI runs and subagents do not get the tool.

### MCP (Model Context Protocol)

`gewu-tools` supports loading external tools via MCP. MCP is an open standard that enables AI models to interact with external data and tools.

## Usage

Tools implement `gewu_core::traits::Tool` and can be supplied through `AgentBuilder::with_tools`. Filesystem tools receive the working directory they are allowed to access:

```rust
use gewu_tools::ApplyPatchTool;

let tool = ApplyPatchTool {
    working_directory: std::env::current_dir()?,
};
```
