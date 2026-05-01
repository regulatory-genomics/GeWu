---
name: daily-memory-rollup
description: Summarize one local day of memory events with source-event provenance.
capabilities:
  - memory_sql_query
  - memory_summary_write
specialists:
  - daily-memory-rollup-agent
---

# Daily Memory Rollup

Use this skill when asked to summarize a local day of GeWu/OpenCode memory events and persist the result.

## Workflow

1. Fetch raw evidence with `memory_sql_query`.
2. Group rows by `session_id`, preserving chronological order.
3. Reduce noise: summarize large tool outputs, but keep user requests, decisions, files, validations, commits, and blockers.
4. Produce a Markdown daily summary with these sections:
   - Overview
   - Sessions / themes
   - Work completed
   - Decisions made
   - Files or artifacts touched
   - Validation / tests
   - Follow-ups
   - Evidence
5. Persist with `memory_summary_write` using a stable `idempotency_key`.

When this skill is used inside a GeWu workflow node whose prompt asks for a JSON object, do not call `memory_summary_write` directly. Return the JSON payload through `report_result` so the workflow can persist it in the next node.

## Evidence rules

- Every claim in the summary should be grounded in raw event rows.
- Pass all event IDs used for the summary in `derived_from`.
- If the query returns too much data, first summarize by session, then summarize the day.
- Do not write a summary if no source events were found.

## Current OpenCode import query pattern

For imported OpenCode rows that still use the older live schema, filter via `metadata_json`:

```sql
SELECT event_id, session_id, event_kind, content, created_at,
       json_extract(metadata_json, '$.opencode.session_title') AS session_title,
       json_extract(metadata_json, '$.opencode.directory') AS workspace_root
FROM memory_events
WHERE json_extract(metadata_json, '$.host') = 'opencode'
  AND json_extract(metadata_json, '$.import.local_date') = ?
  AND json_extract(metadata_json, '$.import.timezone') = ?
  AND json_extract(metadata_json, '$.opencode.directory') = ?
ORDER BY created_at
LIMIT 2000
```
