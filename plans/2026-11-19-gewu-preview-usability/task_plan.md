# Task state

- Current task: 10
- Status: implemented and verified; release gate evidence recorded in plan.md
- Blockers: Manimble installed package is not restaged (source tree only)

## Log

- 2026-11-19: Confirmed the image error comes from the text-only `read_file`
  path; Files has an HTML-only preview action and no media MIME metadata.
- 2026-11-19: Confirmed globally disabled plugins are stored as `package: None`,
  which removes manifest metadata from `catalog()` and prevents the UI from
  offering a session-only enable action.
- 2026-11-19: Confirmed `Plugin selection · N` is rendered from
  `publish_context` data appended to the next prompt as untrusted view context.
- 2026-11-19: Confirmed the current Plugins panel intentionally renders tool
  schemas, package paths and controls in the same dense row; this is an
  information-architecture issue rather than a missing backend capability.
- 2026-11-19: Verified the additional developer report: existing HostAction
  lifecycle operations are broader than the five agent gateway tools; global
  enablement has separate installation-state and host-runtime stores; Manimble
  exact-text source editing is contradicted by its skill wording; scene/render
  are the only current source-text path and include large render payloads;
  random server ports are not emitted as a machine-readable endpoint contract;
  and instance public info has no focus field.
- 2026-11-19: Implemented all ten tasks. Two product bugs surfaced during
  verification: integration tests were writing `demo` into the real
  `~/.gewu/plugin-state.json` (fixed by making config-less hosts non-persistent
  and cleaning the entry), and `pkill -f` patterns killed the driving shell
  (test-harness issue only, no product change).
- 2026-11-19: Confirmed the new `source` tool returns class text with no
  `objects`/`frames` keys against the real Manimble engine, and a stale
  `expectedText` is refused with "read class Intro again".

## Decisions

- Fix media preview as a ticketed server route, not by reading binary data into
  the text protocol.
- Use explicit global-default/session-override state rather than overloading a
  boolean `enabled` field.
- Treat plugin view context as optional provenance shown on demand, not as a
  visible “selection” event on every user message.
- Treat agent-facing plugin operations as a task-shaped ACI: expose typed
  lifecycle/status/focus operations with bounded output and actionable errors,
  rather than requiring agents to use raw TCP or infer HostAction payloads.
