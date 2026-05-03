# Harsh architecture review (2026-05-03)

Branch under review: `feature/subagent` (HEAD `a557ca4`).
Scope: GeWu workspace at `/home/gilberthan/disk1/projects/GeWu` plus related local crates (GeWu-genome, FateWeaver, GeWu-inspector-dashboard, gewu-dashboard, gewu-memory, gewu-message, tachyon-browser, tachyon-canvas).
Focus areas: architecture and crate boundaries, recent subagent / session-persistence commits, error handling and concurrency, tests and frontend monolith, DB schema and storage.

> Reviewer mandate from the user: brutal, no-mercy. The review below intentionally avoids softening language. File and line citations are kept exact so the team can jump to evidence.

## One-paragraph verdict

This is an ambitious multi-crate AI agent framework, but its persistence durability claim is fictional, the concurrency model has silent data-loss paths, the auth scheme is at a 2010 level, there is no CI, no SQL migrations directory, `gewu-cli` has 0 tests across 6 source files, and `gewu-canvas` (which advertises itself as kind-agnostic) ships genome-specific tools. The recent subagent and session-persistence work promises cross-restart resume, but because `Memory::add_message` returns `()` and the runtime calls it after `mark_injected`, the promise is unrecoverable on the failure path.

## Severity legend

- 🔴 CRITICAL: ship-blocker. Data loss, security, or unrecoverable state.
- 🟠 HIGH: architectural debt that compounds; will cost more to fix later.
- 🟡 MEDIUM: real but tolerable problems.
- ⚪ NIT: small fixes worth a single PR.

---

## 🔴 CRITICAL findings

### C1. `Memory::add_message` swallows persistence errors. "Durable session" is a lie on the failure path

- `gewu-core/src/traits.rs:132`: `async fn add_message(&mut self, message: Message);` returns nothing. This is a contract-level defect.
- `gewu-storage/src/lib.rs:184-196`: the SQLite implementation logs `error!` then `return;` on failure. If logs are filtered or shipped late, the caller learns nothing.
- Fatal interaction in `gewu-runtime/src/background_tasks.rs:98-105`:

  ```rust
  if !store.mark_injected(&group.group_id).await? { return Ok(false); }   // line 98
  let message = build_completion_message(&group, &members);              // line 102
  let mut memory = SqliteMemory::new(self.pool.clone(), ...).await?;
  memory.add_message(message).await;                                      // line 105 (no Result)
  ```

  - `mark_injected` already flips the group to "injected".
  - `add_message` SQL failure is swallowed.
  - Startup `reconcile_uninjected_groups` (state.rs:232-242) skips the group because `injected_at IS NOT NULL`.
  - Net: the parent session never receives the `[background_tasks.completed]` system message. The agent waits forever.

- Fix: change `Memory::add_message` (and `clear`, `replace`) to return `Result<()>`. Wrap `maybe_inject` in a transaction so `mark_injected` rolls back if the message insert fails.

### C2. No SQL migrations directory. Schema is scattered across five stores

- `find . -name 'migrations' -type d` matches only `.gewu/workflows`, not a real migrations folder.
- Schema lives inside each store's constructor, run on every startup:
  - `gewu-storage/src/lib.rs:103-123` (`memory_messages`)
  - `gewu-storage/src/session_store.rs:24-46` (`sessions`)
  - `gewu-storage/src/async_task_group_store.rs:31-79` (`async_task_groups`, `async_task_group_members`)
  - plus `task_store.rs`, `execution_trace_store.rs`, `project_store.rs`
- No version table, no ordering, no rollback. `async_task_group_store.rs:49-55` even contains an `ALTER TABLE ADD COLUMN` followed by `.ok()`, which silently runs on every startup and ignores the duplicate-column error. That is an admission of "we have no migration story".
- Consequence: any column change drifts across deployments. Cross-restart resume cannot survive two releases in production.
- Fix: introduce `migrations/NNNN_name.sql`, run via `sqlx::migrate!()` in `AppState::new`, delete the inlined `CREATE TABLE` blocks.

### C3. SSE disconnect agent continuation — now addressed (downgraded to MEDIUM)

- **Design intent**: browser closes, gateway stays up, agent keeps running. This is intentional so long-running agent tasks complete even if the user navigates away.
- **Original gaps** (now fixed):
  - No `AbortHandle` — user could not cancel a running turn. **Fixed**: `AbortHandle` stored in `AppState`, `POST /api/sessions/{id}/cancel` endpoint added.
  - `active_turns` only in memory — process restart lost visibility. **Fixed**: `active_turn_snapshots` table persists turn state; on startup, stale snapshots produce `[session.interrupted_by_restart]` system messages.
  - `Memory::add_message` swallowed errors — fixed in Phase 1 (C1).
- Remaining risk: no automatic timeout on orphaned turns. A future phase could add a configurable grace period.

### C4. Auth — now cookie-based with argon2 hashing (RESOLVED)

- URL token auth has been completely removed. Auth is now cookie-based:
  - `AuthMode::None` (default on loopback): no auth required.
  - `AuthMode::Password { argon2_hash }`: enabled via `--password` flag. Login page at `/login`, session cookie `gewu_session` (HttpOnly, SameSite=Strict).
  - Non-loopback bind without `--password` refuses to start unless `--allow-insecure-bind` is passed.
- Token comparison uses `subtle::ConstantTimeEq`.
- `grep -rn '?token' gewu-web/` returns nothing.
- Security headers added: CSP, X-Frame-Options, X-Content-Type-Options, Referrer-Policy.

### C5. SQLite connection has zero tuning. Foreign keys are silently disabled

- `gewu-web/src/state.rs:624-631`:

  ```rust
  let db_url = format!("sqlite://{}?mode=rwc", db_path);
  let pool = SqlitePoolOptions::new().connect(&db_url).await?;
  ```

- A repository-wide grep for `PRAGMA|journal_mode|busy_timeout|foreign_keys` returns zero matches. So:
  - Foreign keys default OFF. The `FOREIGN KEY(group_id) REFERENCES async_task_groups(group_id)` declared at `async_task_group_store.rs:64` is decorative.
  - WAL is not enabled. Concurrent SSE streams plus background writes will race for the file lock.
  - `busy_timeout` defaults to 0. Contention raises immediate `SQLITE_BUSY` errors instead of waiting.
  - `synchronous` defaults to `FULL`, which is slower than `NORMAL` and unnecessary for an agent log.
  - `max_connections` defaults to 10, which is small for multi-session plus background tasks.
- Fix: set `PRAGMA journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON`, `synchronous=NORMAL`, and tune `max_connections` per workload.

### C6. `gewu-cli` ships zero tests across six source files

- Per-crate test density measurement:

  ```
  gewu-cli           files=6   files_w_tests=0   test_fns=0
  ```

- No unit tests on `config.rs`, `registry.rs`, `mcp_loader.rs`, `tools.rs`, or `lib.rs`. The user-facing CLI entry point has no regression coverage.
- Fix: at minimum add tests for config precedence (CLI args > env > file > default), agent listing, and error paths.

### C7. No CI configuration anywhere in the repo

- `.github/workflows/`, `.gitlab-ci.yml`, `.circleci/` all absent.
- `.git/hooks/` only contains the 14 default `*.sample` files. No `pre-commit` or `pre-push`.
- Multi-developer or autonomous-agent workflows currently rely on trust. A single bad commit lands without `cargo fmt`, `clippy`, or `test` verification.
- Fix: add `.github/workflows/ci.yml` running `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and `cargo check -p <each crate>`.

---

## 🟠 HIGH findings

### H1. Layering inversion: `gewu-core` depends on `sqlx`

- `gewu-core/Cargo.toml:16`: `sqlx = { version = "0.8", features = ["sqlite", "runtime-tokio-native-tls"] }`.
- The "core" crate should expose `trait Memory`, `trait LlmProvider`, etc. and remain storage-agnostic. Right now switching backends is impossible without refactoring core.
- `gewu-storage` then redeclares `sqlx` and depends on `gewu-core`. `gewu-web/Cargo.toml` also pulls `sqlx` directly instead of going through `gewu-storage`. Three independent declarations of the same engine.

### H2. Domain bleed: `gewu-canvas` ships genome-specific tools

- `gewu-canvas/Cargo.toml` description: `"Generic agent-controllable canvas runtime ... kind-agnostic surface; genome canvas lives in tachyon-canvas"`.
- Reality:
  - `gewu-canvas/src/genome_tools.rs` (genome client baked into the generic crate).
  - `gewu-canvas/src/genome_tools.rs:41` reads `TACHYON_BROWSER_URL` env in `Default::default()`.
  - `gewu-web/src/state.rs:2`: `use gewu_canvas::{canvas_tools, genome_tools, GenomeToolsConfig};`.
  - Recent commit `e878ef6 Normalize genome tool assay inputs` adds 40 more lines to `gewu-canvas/src/genome_tools.rs`.
- README says genome plugin should be in `GeWu-genome` (separate crate). The code does the opposite.
- Fix: move `genome_tools.rs` to `GeWu-genome`. Make `gewu-web` ignorant of genome.

### H3. Triple-wrapped `db_pool: Arc<Mutex<Option<SqlitePool>>>`

- `gewu-web/src/state.rs:116`.
- `SqlitePool` is already `Clone + Send + Sync` with internal pooling. Wrapping it in `Arc<Mutex<...>>` serializes every DB access on a tokio mutex (state.rs:169, 285, 499) and adds a useless `Option`.
- `Option<...>` is always `Some` after `AppState::new` (state.rs:200-205), so the `None` branch is dead.
- Fix: `pub db_pool: SqlitePool;` direct. Cloning is cheap.

### H4. `broadcast::channel(256)` silently drops events on lag

- `gewu-runtime/src/background_tasks.rs:34-36`:

  ```rust
  BG_EVENTS.get_or_init(|| broadcast::channel(256).0)
  ```

- `tokio::sync::broadcast` drops the oldest message and returns `RecvError::Lagged` when receivers fall behind.
- Frontend `app.js:736` `addEventListener('background_group.completed', ...)` does not handle lag. If the user switches tabs for a few minutes, group completion events disappear. The "auto-resume by group_id" hook depends on luck.
- Fix: hybrid `mpsc` plus DB polling fallback. On reconnect, refetch any uninjected group completions for the active session.

### H5. `expected_member_count` defaults to 0, race lets the wave settle before all children register

- Schema `gewu-storage/src/async_task_group_store.rs:33-43`: `expected_member_count INTEGER NOT NULL DEFAULT 0`.
- Coordinator `gewu-runtime/src/background_tasks.rs:92`:

  ```rust
  if group.expected_member_count > 0
      && members.len() < group.expected_member_count as usize
  {
      return Ok(false);
  }
  ```

- When `expected = 0`, the coordinator interprets it as "no expectation". Race sequence:
  1. Parent registers task A.
  2. A finishes immediately and settles.
  3. Coordinator sees one settled member, `expected = 0`, injects, marks the group injected.
  4. Parent then registers task B. The group is already "completed", so B's settlement finds no uninjected group and is silently dropped.
- Commit `3c80d51 Make async task groups wait for full parallel waves` mitigates this, but only if the parent always writes `expected_member_count = N` before the first child can settle. That ordering is not enforced in code.
- Fix: enforce "register expected count before spawning any child" in a single transaction, or treat `expected = 0` as "do not inject yet".

### H6. Session backfill rewrites all historical agents to `general`

- `gewu-storage/src/session_store.rs:50-66`:

  ```sql
  INSERT OR IGNORE INTO sessions (session_id, agent_type, created_at, updated_at)
  SELECT m.session_id, 'general', MIN(m.created_at), MAX(m.created_at)
  FROM memory_messages m
  LEFT JOIN sessions s ON s.session_id = m.session_id
  WHERE s.session_id IS NULL
  GROUP BY m.session_id
  ```

- Pre-existing sessions might have been `coder`, `genome_orchestrator`, `fateweaver_orchestrator`, etc. After the first restart following commit `0c67419`, every legacy session becomes `general`.
- `state.rs:458` then rebuilds the agent from `record.agent_type`. The user's session is silently downgraded to a generic agent.
- Fix: backfill should infer agent type from message metadata or set `agent_type = NULL` and force a UI selection on next open.

### H7. Multiple `tokio::spawn` calls drop their `JoinHandle`

Concrete sites:

- `gewu-web/src/routes/chat.rs:122` (see C3)
- `gewu-runtime/src/tools.rs:282` (async child task dispatcher)
- `gewu-tools/src/mcp/client.rs:71`, `client.rs:147` (MCP long-lived background tasks)
- `gewu-gateway/src/scheduler/worker.rs:9`, `worker.rs:25` (scheduler workers)

None of these have abort plumbing. There is no way to implement "cancel everything for session X" without a process restart.

### H8. Hardcoded model `gpt-5.3-codex` appears five times

- `gewu-web/src/state.rs:71, 154, 270, 484` all do `unwrap_or_else(|| "gpt-5.3-codex".to_string())`.
- This is not a public model name. It looks like a `cliproxyapi` route alias. Anyone forking this code gets `model_not_found` immediately.
- Fix: a single `DEFAULT_MODEL` constant, or `Err("model not configured")` instead of silent fallback.

### H9. Frontend attribute-context XSS: `escapeHtml` does not escape quotes

- `gewu-web/static/app.js:221-225`:

  ```js
  function escapeHtml(text) {
      const div = document.createElement('div');
      div.textContent = text;
      return div.innerHTML;
  }
  ```

- The `textContent → innerHTML` round-trip encodes `<>&` but leaves `"` and `'` untouched.
- This function is used inside attribute contexts in many places (`title="${escapeHtml(...)}"`, `data-session-id="${escapeHtml(run.session_id || '')}"`), for example `app.js:291, 342, 437, 438, 565, 1088, 1102`.
- If any rendered field contains `"` (LLM output, tool output, agent type, summary text), the attribute breaks and arbitrary attributes such as `onload=` can be injected.
- The markdown renderer is fine: `app.js:227-230` uses `marked.parse` followed by `DOMPurify.sanitize`.
- Fix: extend `escapeHtml` with `.replace(/"/g, '&quot;').replace(/'/g, '&#39;')`, or use `setAttribute` instead of template literals.

### H10. `temperature` is hardcoded to `None` in the OpenAI path

- `gewu-providers/src/llm.rs:502`: `temperature: None,`.
- `max_tokens` was made configurable in commit `984b366`, but `temperature` was not. Users cannot pin determinism (0.0) or comply with reasoning models that require 1.0.

### H11. `agent.rs` hardcodes `context_limit: 128_000`

- `gewu-core/src/agent.rs:393`. Same anti-pattern as the original `max_tokens=8192`. Anthropic 200K, OpenAI 1M, local small models 32K all share the same default.

### H12. `gewu-rag` pins `embed_anything` to a git revision

- `gewu-rag/Cargo.toml`:

  ```toml
  embed_anything = { git = "https://github.com/StarlightSearch/EmbedAnything", rev = "273bf9da..." }
  ```

- No release tag, no fork, no vendor directory. If the upstream force-pushes, the rag crate stops building.

### H13. Workspace edition mismatch

- Most crates are `edition = "2024"`. `gewu-web/Cargo.toml` is `edition = "2021"`. Some lints behave differently across editions, and `gewu-web` directly depends on `gewu-canvas` (2024). Trivial to fix, embarrassing to leave.

---

## 🟡 MEDIUM findings

### M1. `gewu-graph`, `gewu-rag`, `gewu-canvas`, `gewu-gateway` purposes are unclear

- `gewu-graph` has 14 source files and 97 tests but is referenced only by `gewu-runtime` and `gewu-cli`. Its actual responsibility inside runtime is undocumented.
- `gewu-gateway` is its own binary with `scheduler/`, `inspector/`, `catalog/`, and routes that overlap with `gewu-web` (both have `routes/runs.rs`). Whether it is a superset, sibling, or successor of `gewu-web` is not declared anywhere.
- Fix: every crate `Cargo.toml` description should commit to one external promise. Crates that cannot articulate their promise should merge or die.

### M2. `panic!` in production code paths

- `gewu-graph/src/graph/state_graph.rs:302, 305, 749`: runtime panics on illegal graph construction. These should be `Result`.
- `gewu-runtime/src/providers/mcp.rs:71`: `unimplemented!("MCP tool discovery via ToolProvider not yet wired")`. Any caller that hits this path crashes the process.

### M3. `println!` in library code

- `gewu-cli/src/lib.rs:143, 157, 169, 309, 311, 315, 386, 459, 516, 551, 561` print to stdout from inside a library, which is unfriendly for embedders. Convert to `tracing` or a callback.
- `gewu-rag/src/main.rs:267-311` and `gewu-web/src/lib.rs:258-261` are entry-point banners and acceptable.

### M4. No security headers

- `gewu-web/src/fallback.rs` only handles `index.html`. The rest is served by `tower_http::services::ServeDir`. No `Content-Security-Policy`, `X-Frame-Options`, `Referrer-Policy`, `Strict-Transport-Security` set anywhere.
- A localhost UI exposed via tokenized URL would benefit from CSP as a backstop for any future rendering bug.

### M5. All storage tests use `sqlite::memory:`

- `gewu-storage` has 6 files and 11 tests, all on in-memory connections. In-memory SQLite has no file lock, no WAL, no fsync, so atomicity claims tested in memory do not transfer to disk behavior.
- No fault injection ("disk full"), no multi-pool concurrency tests.

### M6. `mark_injected` and `add_message` are not in the same transaction

- See C1. Even after `add_message` returns `Result`, the UPDATE in `mark_injected` and the INSERT in `add_message` should share a single `BEGIN ... COMMIT`.

### M7. Sidecar lacks graceful shutdown and PID tracking

- `gewu-web/src/sidecar.rs:64-69`: `Drop` calls `child.start_kill()` directly (SIGKILL), with no SIGTERM grace period.
- No PID file. A previous GeWu process that crashed may have left a sidecar listening on the configured port. The `health_ok` probe at line 95-98 partially compensates, but a port-bound but 5xx health endpoint would deadlock startup.

### M8. `state.rs:539 get_session_messages` orders by autoincrement id

- Schema is `id INTEGER PRIMARY KEY AUTOINCREMENT` (`gewu-storage/src/lib.rs:107`). Ordering by `id` works today, but switching to ULID or UUID later silently breaks ordering. Use `ORDER BY created_at, id` to be explicit.

### M9. `gewu-rag/src/main.rs:267-311` REPL is bare-bones

- No line editor (`rustyline`, `reedline`), no history, no completion, no Ctrl-C handling. Functional but rough for end users.

---

## ⚪ NIT findings

- `gewu-storage/src/lib.rs:308`, `gewu-providers/src/replay.rs:241, 249`: `_ => panic!()` inside tests is acceptable but noisy. `assert!(matches!(...))` is clearer.
- `gewu-web/src/auth.rs:14`: `rand::thread_rng()` is fine, but `rand::rngs::OsRng` is more explicit about CSPRNG intent.
- `gewu-web/src/state.rs:615-622`: `get_default_db_path` has a side effect (creates the directory). Rename to `ensure_default_db_path`.
- `gewu-storage/src/session_store.rs:111-114, 148-152`: row deserialization uses `unwrap_or_default()`. Errors should propagate via `?`.
- `gewu-web/static/app.js` is 1736 lines, single file, all globals. Not a bug; maintenance hazard.
- `gewu-providers/src/replay.rs` has 10 `unwrap`s worth auditing.
- `gewu-runtime/src/composer.rs` has 33 `unwrap`s, the highest non-test density in the workspace.

---

## What is actually done well

Listed briefly because the user asked for harshness, not validation.

- `gewu-web/static/app.js:227-230`: `marked.parse` followed by `DOMPurify.sanitize` is the right pattern for rendering markdown content (the attribute-context bug in H9 is a separate issue).
- `gewu-storage/src/async_task_group_store.rs:179-188`: `mark_injected` uses `WHERE injected_at IS NULL` for atomic CAS, with idempotency unit tests at `mark_injected_is_idempotent`.
- `gewu-storage/src/lib.rs:222-273`: `replace` uses a SQLx transaction, even if the surrounding documentation overstates its atomicity.
- `gewu-web/src/sidecar.rs` has unit tests covering camelCase/snake_case alias parsing.
- `gewu-runtime` is the best-tested crate (19 of 34 files, 63 tests).

---

## Per-crate test density (factual, no commentary)

```
gewu-core          files=8   files_w_tests=2   test_fns=9
gewu-providers     files=5   files_w_tests=3   test_fns=19
gewu-storage       files=6   files_w_tests=6   test_fns=11
gewu-tools         files=16  files_w_tests=8   test_fns=24
gewu-runtime       files=34  files_w_tests=19  test_fns=63
gewu-cli           files=6   files_w_tests=0   test_fns=0
gewu-web           files=14  files_w_tests=5   test_fns=12
gewu-gateway       files=26  files_w_tests=2   test_fns=16
gewu-graph         files=14  files_w_tests=4   test_fns=97
gewu-rag           files=19  files_w_tests=5   test_fns=17
gewu-canvas        files=5   files_w_tests=4   test_fns=20
```

`gewu-cli` (the user-facing entry point) is the worst by absolute test count.

---

## Recommended fix order

1. Change `Memory::add_message`, `clear`, `replace` to return `Result<()>`. Propagate the error in every caller. Wrap `maybe_inject` in a transaction (addresses C1 and M6).
2. Set SQLite PRAGMAs at pool creation: `journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON`, `synchronous=NORMAL` (addresses C5; partially addresses C2 by making FK declarations real).
3. Move auth token to `Authorization: Bearer ...` header. Use `subtle::ConstantTimeEq` for comparison. Stop printing the tokenized URL in the startup banner (addresses C4).
4. Wire a `CancellationToken` into `chat.rs:122`, store `AbortHandle`s in `AppState`, expose `POST /api/sessions/{id}/cancel`, and honor SSE disconnect with a grace period before abort (addresses C3 and H7).
5. Add `.github/workflows/ci.yml` running `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `cargo check -p <each crate>` (addresses C7).
6. Introduce `migrations/NNNN_*.sql`, run via `sqlx::migrate!()` in `AppState::new`, delete the inlined `CREATE TABLE` blocks in each store (addresses C2).
7. Move `gewu-canvas/src/genome_tools.rs` to `GeWu-genome` and stop importing `genome_tools` from `gewu-web`. Make `gewu-canvas` actually kind-agnostic (addresses H2).
8. Patch `escapeHtml` in `gewu-web/static/app.js` to also escape quotes. Audit every attribute-context use site (addresses H9).

---

## Appendix: how the evidence was gathered

- `git log --oneline -15` and `git show --stat <hash>` for the recent commits `a557ca4`, `3c80d51`, `048b00a`, `adb7dd7`, `0c67419`, `e878ef6`.
- `cat <crate>/Cargo.toml` for every workspace member.
- `find <crate>/src -name '*.rs'` to enumerate per-crate file structure.
- `Grep` for `panic!`, `todo!`, `unimplemented!`, `unwrap()`, `expect(`, `unbounded_channel`, `tokio::spawn`, `eprintln!`, `println!`, `dbg!`, `max_tokens`, `temperature`, `top_p`, `PRAGMA`, `journal_mode`, `busy_timeout`, `foreign_keys`, `innerHTML`, `DOMPurify`, `escapeHtml`, `EventSource`.
- `Read` of `gewu-web/src/routes/chat.rs`, `gewu-runtime/src/background_tasks.rs`, `gewu-storage/src/async_task_group_store.rs`, `gewu-storage/src/session_store.rs`, `gewu-storage/src/lib.rs`, `gewu-web/src/state.rs`, `gewu-web/src/auth.rs`, `gewu-web/src/sidecar.rs`, `gewu-web/src/fallback.rs`, `gewu-canvas/src/genome_tools.rs`, plus selected ranges of `gewu-providers/src/llm.rs` and `gewu-core/src/agent.rs`.
- `gewu-web/static/app.js` and `app.css` line counts and inline pattern audit.
- Per-crate test density via `find ... -name '*.rs' | wc -l` and `grep -rn '#\[test\]\|#\[tokio::test\]' | wc -l`.

No production data was modified. No code was rewritten. This is observation only.
