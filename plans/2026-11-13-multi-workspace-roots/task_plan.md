# Task state

Current task: 5
Status: done

## Log

- 2026-11-13: Confirmed current GeWu has one `Bootstrap.working_directory`, one `ProfileInputs.working_directory`, one-root `ToolContext` fallback via tool structs, one-root server file APIs, and no workspace-selection protocol.
- 2026-11-13: Chosen design: named configured roots, session-scoped active root, root-relative paths, additive factory propagation, SQLite persistence.
- 2026-11-13: RED: added config/root-resolution and SQLite session-workspace tests; they failed because the APIs did not exist.
- 2026-11-13: GREEN: implemented `Config.workspaces`, canonical root resolution, `session_workspaces`, and dynamic `ToolContext` root selection. Targeted tests pass.
- 2026-11-13: Threaded selected roots through primary agents, task/workflow child agents, server file APIs, and the wire/client protocol. Added idle-only `switch_workspace` with SQLite persistence.
- 2026-11-13: Added GUI root selector, per-tab workspace state, root-aware file cache/client operations, and documentation/config examples.
- 2026-11-13: Full test run was started in the background; a concurrent `cargo check` later timed out on Cargo's build-directory lock, so a fresh post-change verification is still required.
- 2026-11-14: Root cause of the excessive test duration was two overlapping GeWu full builds plus unrelated Tachyon Rust builds. Terminated only the two GeWu process groups and reran with `CARGO_BUILD_JOBS=4`.
- 2026-11-14: Fresh `cargo test --workspace --locked` passed all workspace tests; three GUI message-order regressions were fixed and `cargo test -p gewu-gui --locked` then passed 104 tests.
