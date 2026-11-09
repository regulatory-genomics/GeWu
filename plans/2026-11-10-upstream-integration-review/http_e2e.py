#!/usr/bin/env python3
"""Process-level GeWu/Tachyon smoke test; fixture outputs are pipeline_validation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--datasets", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    token = secrets.token_hex(24)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(path, body=None, auth=True):
        headers = {"content-type": "application/json"}
        if auth:
            headers["authorization"] = f"Bearer {token}"
        return opener.open(urllib.request.Request(base + path,
            data=None if body is None else json.dumps(body).encode(), headers=headers), timeout=30)

    def get(path, body=None):
        try:
            with request(path, body) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise AssertionError(f"{path}: HTTP {error.code}: {error.read().decode()}") from error

    def call(tool, arguments):
        return {"type": "tool_call", "tool": tool, "arguments": arguments}

    collection = "gewu-upgrade-validation"
    param = {"id": "comparison", "kind": "parameter",
             "values": {"case_group": "fibroblast", "reference_group": "microglia"}}
    de = {"id": "de", "kind": "operator", "operator": "pseudobulk-de@1",
          "parameters": {"mode": "fixture", "analysis_purpose": "pipeline_validation",
             "case_group": {"ref": {"node_id": "comparison", "port": "case_group"}},
             "reference_group": {"ref": {"node_id": "comparison", "port": "reference_group"}},
             "rows": [{"gene_id": "FIXTURE_GENE", "baseMean": 100.0,
                       "log2FoldChange": 2.0, "pvalue": 0.0001, "padj": 0.001}]},
          "outputs": {"table": {"schema": "tachyon.de.result.v1", "mime": "text/tab-separated-values"},
                      "plot": {"schema": "tachyon.plotly.v1", "mime": "application/json"}}}
    steps = [call("node.capabilities", {}),
             call("node.upsert", {"collection_id": collection, "node": param}),
             call("node.upsert", {"collection_id": collection, "node": de}),
             call("node.run", {"collection_id": collection, "node_id": "de"}),
             {"type": "text", "text": "pipeline_validation; inference_eligible=false"}]
    script = work / "script.json"
    script.write_text(json.dumps(steps))
    env = dict(os.environ)
    for name in list(env):
        if name.startswith("TACHYON_"):
            del env[name]
    env.update({"TACHYON_CONTROL_TOKEN": token, "TACHYON_AGENT_API_KEY": "fixture-key",
                "TACHYON_AGENT_MODEL": "scripted-validation", "TACHYON_AGENT_SCRIPT": str(script),
                "TACHYON_AGENT_STATE_DIR": str(work / "state"),
                "TACHYON_WORKFLOW_DIR": str(work / "workflows"),
                "TACHYON_RESULT_DIR": str(work / "results"),
                "TACHYON_ANALYSIS_WORK_DIR": str(work / "analysis")})
    binary = args.binary.resolve()
    report = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "dataset_root": str(args.datasets.resolve()), "analysis_purpose": "pipeline_validation",
              "inference_eligible": False, "checks": []}
    with (work / "server.log").open("w") as log:
        process = subprocess.Popen([str(binary), "--host", "127.0.0.1", "--port", str(port),
                                    "--datasets", str(args.datasets.resolve())],
                                   cwd=work, env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 180
            while True:
                assert process.poll() is None, "server exited; inspect server.log"
                try:
                    health = get("/api/health")
                    break
                except (urllib.error.URLError, TimeoutError):
                    assert time.monotonic() < deadline, "startup deadline"
                    time.sleep(0.25)
            assert health["status"] == "ok"
            report["checks"].append("health")
            try:
                request("/api/agent/sessions", {"mode": "reference"}, auth=False)
                raise AssertionError("unauthenticated agent mutation was accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 401, error.code
            report["checks"].append("authentication")
            session = get("/api/agent/sessions", {"mode": "reference", "session": "e2e",
                                                   "workspace_epoch": "validation"})
            assert session["session"] == "e2e", session
            with request("/api/agent/sessions/e2e/runs", {"message": "Run the synthetic pipeline validation"}) as response:
                raw = response.read().decode()
            (work / "run.sse").write_text(raw)
            events = [json.loads(line[5:]) for line in raw.splitlines() if line.startswith("data:")]
            assert events and events[-1]["type"] == "finish", events
            assert not any(event["type"] == "error" for event in events), events
            starts = [e["call_id"] for e in events if e["type"] == "tool_start"]
            finishes = [e["call_id"] for e in events if e["type"] == "tool_finish"]
            assert len(starts) == 4 and starts == finishes, (starts, finishes)
            assert any(e["type"] == "usage" for e in events)
            report["checks"].extend(["SSE tool correlation", "SSE usage", "terminal finish"])
            state = get(f"/api/node-collections/{collection}")
            (work / "collection.json").write_text(json.dumps(state, indent=2))
            run = state["runs"]["de"]
            assert run["status"] == "succeeded", run
            hashes = {}
            for port_name, artifact in run["outputs"].items():
                with request("/api/artifacts/" + artifact["artifact_id"]) as response:
                    payload = response.read()
                assert payload, port_name
                hashes[port_name] = hashlib.sha256(payload).hexdigest()
            assert {"table", "plot"} <= hashes.keys(), hashes
            report["artifacts"] = hashes
            report["checks"].append("published DE table and plot")
            history = get("/api/agent/history/e2e")
            assert "FIXTURE_GENE" in json.dumps(history), "tool output missing from persisted history"
            report["checks"].append("persisted tool history")
            report["ok"] = True
            (work / "report.json").write_text(json.dumps(report, indent=2))
            print(json.dumps(report, indent=2))
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
