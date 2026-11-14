# Task state

Current task: 3
Status: done

## Log

- 2026-11-14: Existing GUI tabs were confirmed; the missing behavior is choosing a workspace before the first session request.
- 2026-11-14: Chosen design is a server-side workspace chooser followed by a creation-time workspace field, preserving the existing tab/session model.
- 2026-11-14: Added optional `workspace` to the first-session request path; persisted workspace metadata is authoritative for resumed sessions.
- 2026-11-14: Protocol, server, workspace-state, and GUI chooser tests pass; moving to browser event wiring and full verification.
- 2026-11-14: Added the chooser event flow, workspace labels on tabs, and pre-session file-tree handling.
- 2026-11-14: `cargo test --workspace --locked` and `cargo build -p gewu --release --locked` passed; release service restarted on port 8889.
- 2026-11-14: Desktop browser automation was unavailable in this session; HTTP and served asset checks passed.
