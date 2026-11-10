# Findings

## Verified by source inspection
- cfdec62 `ask_with` returns from the pre-dispatch loop on budget/cancel, after
  persisting Assistant tool_calls, leaving missing Tool replies and unclosed
  ToolStart events. The merge weakened the budget regression to expect zero
  executions without checking transcript validity.
- Cancellation/deadline checks are outside awaited compaction, provider setup,
  provider streaming and tool joins. A blocked request/tool can therefore exceed
  the declared wall-clock deadline indefinitely. Queued tasks do not check abort
  after acquiring the parallelism semaphore.
- GeWu uses a local untracked Cargo.lock; historical workspace verification was
  blocked by transitive GUI dependencies requiring a newer Rust version.
- The historical malformed-JSON test no longer expects a terminal AgentEvent::Error;
  upstream intentionally reports a recoverable ToolFinish error. Preserve that
  contract and assert the tool result actually exists.

## Open checks
- Server webFetch config was dropped during merge while retaining a local policy
  type; trace actual server registry configuration before restoring it.
- Tachyon sends empty reasoning_effort when unset; inspect request serialization.
- Review all workspace crate tests and HTTP/SSE behavior, not only compilation.

## Execution evidence
- Baseline attempt 1 could not write `/tmp/opencode` (permission denied); logs
  moved into this effort directory. Attempt 2 reproduced Rust 1.88 MSRV errors.
- Installed Rust 1.96.0 alongside stable (global default unchanged). Full
  workspace compilation exposed missing Bootstrap.web_fetch in two server test
  fixtures, then missing call_id in eight client test event literals.
- Lifecycle RED: 5 reproducible failures (budget, cancellation/history, stalled
  provider cancellation/deadline, abort-aware tool cleanup). GREEN: all 11
  lifecycle tests pass after shared-loop repair, including successful resumption.
- Server webFetch RED: existing config rejected by deny_unknown_fields. Restored
  config->bootstrap->profile->factory propagation and added actual tool execution
  checks for every profile and child agents (denied localhost before network I/O).
- Full GeWu workspace tests now pass on 1.96. A pre-existing glob fixture assumed
  identical creation mtimes while production sorts newest-first; explicitly equal
  mtimes now test the alphabetical tie-break, preserving the separate mtime test.
- Tachyon provider RED: unset reasoning_effort serialized as `""`, contradicting
  the documented default. Provider construction now calls the builder only when
  an effort is present; both omitted and explicit values are asserted on payloads.
- Tachyon in-place full tests blocked by concurrent canonical_selections changes.
  A tracked-source snapshot at sibling `tachyon-browser-gewu-review-20261110`
  contains HEAD 12390f0 plus only this effort's scoped files. It preserves path
  dependencies. Snapshot compilation exposed outdated Assistant fixtures in
  agent_host and agent_workflow_e2e; these are repaired in source and snapshot.

## Final verification
- GeWu `cargo test --workspace --locked`: 777 passed, 0 failed, 13 ignored.
- GeWu `cargo check --workspace --all-targets --locked`: passed. Two existing
  test-only unused import/mut warnings remain.
- Tachyon clean snapshot `cargo test --features agent --locked`: 656 passed,
  0 failed, 2 ignored. The live working tree remains under concurrent development.
- HTTP/SSE E2E `http_e2e.py`, successful evidence in `http-run-4/report.json`:
  authenticated mutation, correlated tool events, usage and finish events, node
  execution, DE table + plot publication, persisted history. Synthetic fixture
  outputs are pipeline_validation, inference_eligible=false. Dataset root was
  `/data3/litian/projects/tachyon_project/tachyon-datasets/_build/serve_root/datasets`.
- E2E attempt 1 corrected a test assumption: agent GET routes are intentionally
  public; mutation routes require auth. Attempts 2/3 got bare 404 despite an agent
  build because the shared target binary was overwritten. Rebuilding and copying
  immediately to a dedicated executable resolved it (attempt 4). Never validate
  or deploy a mutable shared Cargo target while other work is building it.
- Production port 8406 was not restarted by this review. Live-provider inference,
  visual UI interaction and ignored container tests were not run in this effort.

## Deployment correction (after review close-out)
+- Reported symptom reproduced: 8406 ran `./target/release/tachyon-browser` with
+  commit 6b0fd1e and `/api/agent/sessions` returned 404. This was a non-agent build.
+- Root cause: `deploy/serve.sh` silently fell back to the shared target/release
+  binary when the stable agent path was unavailable; the systemd unit template also
+  pointed at target/release.
+- Fix in tachyon-browser d6f926a: serve.sh refuses the fallback and exits nonzero if
+  the agent route is 404; the unit uses `_build/serve/tachyon-browser-agent` and the
+  full dataset root.
+- Rebuilt from clean worktree at 618c01b with an isolated target dir, verified the
+  binary (`commit=618c01bd3d8e`, agent route 200), replaced stable, and restarted.
+  Live 8406 now runs the stable agent binary; `/api/agent/sessions` and history are
+  200.
+