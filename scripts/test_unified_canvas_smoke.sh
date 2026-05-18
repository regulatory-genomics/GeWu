#!/usr/bin/env bash
set -euo pipefail

BASE=${BASE:-http://127.0.0.1:8080}
WORKSPACE=${WORKSPACE:-default}
SESSION=${SESSION:-default}
TMPDIR=${TMPDIR:-/tmp}

curl -fsS "$BASE/api/workspaces" >"$TMPDIR/gewu_workspaces.json"
curl -fsS "$BASE/api/workspaces/$WORKSPACE/canvas/graph?session_id=$SESSION" >"$TMPDIR/gewu_graph.json"

curl -fsS -X POST "$BASE/api/workspaces/$WORKSPACE/workflow/events" \
  -H 'Content-Type: application/json' \
  -d '{"id":"e_smoke_art","timestamp":"2026-05-18T00:00:00Z","session_id":"default","actor":"agent","kind":"artifact_produced","payload":{"artifact_id":"smoke_table","summary":"Smoke table","format":"tsv","canvas_kind":"data_table","preview":{"kind":"table","columns":["sample"],"rows":[["SM30-1"]],"total_rows":1}}}' >"$TMPDIR/gewu_event1.json"

curl -fsS -X POST "$BASE/api/workspaces/$WORKSPACE/workflow/events" \
  -H 'Content-Type: application/json' \
  -d '{"id":"e_smoke_surface","timestamp":"2026-05-18T00:00:01Z","session_id":"default","actor":"user","kind":"artifact_surfaced","payload":{"artifact_id":"smoke_table","canvas_node_id":"node_smoke_table","position":{"x":10,"y":20}}}' >"$TMPDIR/gewu_event2.json"

node_count=$(curl -fsS "$BASE/api/workspaces/$WORKSPACE/canvas/graph?session_id=$SESSION" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["nodes"]))')
if [ "$node_count" -lt 1 ]; then
  echo "Expected at least one graph node, got $node_count" >&2
  exit 1
fi

payload_ok=$(curl -fsS "$BASE/api/workspaces/$WORKSPACE/canvas/graph?session_id=$SESSION" | python3 -c 'import json,sys; g=json.load(sys.stdin); print(any(n.get("data",{}).get("payload",{}).get("columns") == ["sample"] for n in g["nodes"]))')
if [ "$payload_ok" != "True" ]; then
  echo "Expected projected node payload preview" >&2
  exit 1
fi

echo "UNIFIED CANVAS SMOKE PASS"
