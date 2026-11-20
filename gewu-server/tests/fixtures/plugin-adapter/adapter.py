#!/usr/bin/env python3
from gewu_plugin import serve


def activate(params):
    service = params["settings"].get("service")
    return {"tools": [
        {"name": name, "description": name.capitalize() + " in the fixture", "group": "workflow" if name == "workflow_echo" else "workspace", "inputSchema": {"type": "object", "properties": {"value": {"type": "integer"}}},
         "outputSchema": {"type": "object", "properties": {"value": {"type": "integer"}}}, "exposeToAgent": True, "effect": "read", "target": "view" if name == "view_echo" else "backend"}
        for name in ("echo", "wait", "make_file", "bad_output", "workflow_echo", "view_echo")
    ], "serviceEndpoints": [{"id":"native","baseUrl":service,"protocol":"http", "headers":{"x-demo-key":"native-secret"},"routes":[{"pathPrefix":"/native","methods":["GET","POST"]}]}] if service else []}


def call(tool, args, context, control):
    if tool == "view_echo" and args.get("value") == 13:
        return {"ok": True, "data": {"value": 13}, "resources": [], "changes": [
            {"uri": "workspace:///doc", "beforeRevision": "old", "afterRevision": "new"}
        ]}
    if tool == "bad_output":
        return {"ok": True, "data": {"value": "wrong type"}, "resources": []}
    if tool == "wait":
        control.progress("Waiting")
        if control.cancelled.wait(5):
            return {"ok": False, "error": {"code": "CANCELLED", "message": "Stopped", "retryable": False}, "resources": []}
        return {"ok": False, "error": {"code": "TIMED_OUT", "message": "No cancel", "retryable": False}, "resources": []}
    if tool == "make_file":
        from pathlib import Path
        Path("result.txt").write_text("0123456789", encoding="utf-8")
        return {"ok": True, "data": {}, "resources": [{"uri":"workspace:///result.txt","type":"text","mimeType":"text/plain"}]}
    return {"ok": True, "data": args, "resources": []}


if __name__ == "__main__":
    serve(activate=activate, call=call)
