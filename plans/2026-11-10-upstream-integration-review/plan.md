---
status: done
---
# GeWu upstream integration review

## Goal and constraints
Review the complete GeWu workspace after merge cfdec62, repair demonstrated defects,
and verify the Tachyon consumer through deterministic HTTP/SSE end-to-end tests.
Work in place (user choice); preserve unrelated Tachyon working changes and the
pre-update backup/stash. Use bounded Cargo jobs and capture real command exit codes.
Keep upstream parallel tool execution, reasoning/replay and compaction behavior.

## Tasks and acceptance gates
1. Inventory workspace, callers and build requirements: `Cargo.toml`, all crate
   manifests, `.github/workflows/`, `gewu-core/src/agent/`, `gewu-server/src/`,
   `gewu-providers/src/`, Tachyon `src/agent/` and `deploy/serve.sh`.
   - [x] Full workspace build/test baseline recorded; toolchain limitations diagnosed.
2. Repair run lifecycle with tests in `gewu-core/tests/run_lifecycle.rs` and shared
   implementation in `gewu-core/src/agent/mod.rs` / `execution.rs` as required.
   - [x] Budget, cancellation and deadline tests prove terminal events, paired tool
     messages/events, bounded dispatch and successful subsequent turns.
   - [x] Core/runtime/tools pass as part of `cargo test --workspace --locked`.
3. Repair proven merge regressions in provider/server/client and Tachyon callers.
   - [x] Tests cover preserved webFetch configuration and unset reasoning effort.
   - [x] `cargo test --workspace --locked` and workspace check pass with documented
     reproducible dependencies, or exact external blockers are recorded.
4. Verify integration using Tachyon `tests/agent_*.rs`, deterministic scripted
   provider, isolated result/session roots and HTTP/SSE service on a test port.
   - [x] `cargo test --features agent --locked` passes or unrelated baseline
     failures are named with evidence; process-level E2E verifies real routing.
5. Inspect final diffs, commit only scoped files, and push GeWu/Tachyon to Gitea.
   - [x] Fresh verification, artifact hashes and remote commits are recorded;
     durable conclusions are recorded in `/data/litian/my-wiki`.

## Design decisions
Budgets count attempted calls up to the remaining per-run budget. Rejected calls
must be answered in history; every announced call must finish. Terminal events
come after history repair so a consumer stopping on the terminal event is safe.
Cancellation/deadlines must interrupt provider setup/streaming and waiting tools;
already-started cancellable processes receive their shared abort signal and are
given bounded cleanup before force-aborting asynchronous tasks.
