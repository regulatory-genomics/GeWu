# Task state

Current task: 1
Status: done

## Log

- 2026-11-14: Existing implementation requires `workspaces` in config. User requested runtime selection without hard-coded roots.
- 2026-11-14: Chosen server-side authenticated directory picker so remote 8889 sessions select directories on the GeWu host.
- 2026-11-14: Added server browse/add protocol operations, runtime session root persistence, GUI browser modal, and always-visible add-directory action.
- 2026-11-14: Full workspace tests passed after the runtime picker changes.
- 2026-11-14: Added a regression test for the user-visible `pwd` symptom; full workspace tests pass after fixing primary agent construction to retain the selected root.
