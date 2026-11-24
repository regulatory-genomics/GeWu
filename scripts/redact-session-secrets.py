#!/usr/bin/env python3
"""Remove configured provider credentials from one GeWu session database.

The database is the operational record. This utility is deliberately scoped:
it rewrites only message and session-state values, keeps ids/timestamps, writes
a restricted backup first, and never prints a secret.

Usage:
  redact-session-secrets.py --database PATH --config PATH [--session ID] [--dry-run]
  redact-session-secrets.py --database PATH --secret-env GEWU_API_KEY [...]
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import sys
import time
from pathlib import Path

REDACTED = "[redacted]"
SENSITIVE_KEYS = {"api_key", "apikey", "authorization", "password", "secret", "token"}


def configured_secrets(config: Path, env_names: list[str]) -> list[str]:
    secrets: set[str] = set()
    if config:
        data = json.loads(config.read_text())
        for endpoint in (data.get("endpoints") or {}).values():
            if isinstance(endpoint, dict) and isinstance(endpoint.get("api_key"), str):
                secrets.add(endpoint["api_key"])
        if isinstance(data.get("api_key"), str):
            secrets.add(data["api_key"])
    for name in env_names:
        value = os.environ.get(name)
        if value:
            secrets.add(value)
    return sorted((s for s in secrets if len(s) >= 8 and s != REDACTED), key=len, reverse=True)


def redact_text(text: str, secrets: list[str]) -> str:
    for secret in secrets:
        text = text.replace(secret, REDACTED)
    return text


def redact_json(value, secrets: list[str]):
    if isinstance(value, str):
        return redact_text(value, secrets)
    if isinstance(value, list):
        return [redact_json(item, secrets) for item in value]
    if isinstance(value, dict):
        return {
            key: REDACTED if key.lower() in SENSITIVE_KEYS else redact_json(item, secrets)
            for key, item in value.items()
        }
    return value


def keys(tables: set[str]) -> tuple[str, str]:
    messages = "memory_messages" if "memory_messages" in tables else ""
    state = "session_state" if "session_state" in tables else ""
    if not messages or not state:
        raise SystemExit("database does not have the expected GeWu tables")
    return messages, state


def report_counts(db: sqlite3.Connection, secrets: list[str], session: str | None) -> dict:
    tables = {row[0] for row in db.execute("select name from sqlite_master where type='table'")}
    messages, state = keys(tables)
    where, args = (" where session_id=?", (session,)) if session else ("", ())
    message_hits = 0
    for (content,) in db.execute(f"select content from {messages}{where}", args):
        if any(secret in content for secret in secrets):
            message_hits += 1
    state_hits = 0
    for (value,) in db.execute(f"select value from {state}{where}", args):
        if any(secret in value for secret in secrets):
            state_hits += 1
    return {"messages": message_hits, "session_state": state_hits}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--secret-env", action="append", default=[])
    parser.add_argument("--session")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    secrets = configured_secrets(args.config, args.secret_env)
    if not secrets:
        raise SystemExit("no configured secret values found")

    source = sqlite3.connect(f"file:{args.database}?mode=ro", uri=True)
    tables = {row[0] for row in source.execute("select name from sqlite_master where type='table'")}
    messages, state = keys(tables)
    before = report_counts(source, secrets, args.session)
    if args.dry_run:
        print(json.dumps({"dry_run": True, "before": before, "secrets": len(secrets)}))
        return 0

    backup = args.database.with_name(
        f"{args.database.name}.quarantine-{time.strftime('%Y%m%d-%H%M%S')}"
    )
    with sqlite3.connect(backup) as target:
        source.backup(target)
    os.chmod(backup, 0o600)
    source.close()

    db = sqlite3.connect(args.database, timeout=30)
    db.execute("pragma busy_timeout=30000")
    where, where_args = (" where session_id=?", (args.session,)) if args.session else ("", ())
    changed_messages = 0
    changed_state = 0
    try:
        db.execute("begin immediate")
        message_rows = db.execute(
            f"select id, content from {messages}{where}", where_args
        ).fetchall()
        for row_id, content in message_rows:
            updated = redact_text(content, secrets)
            if updated == content:
                continue
            try:
                parsed = json.loads(updated)
            except json.JSONDecodeError:
                parsed = None
            if parsed is not None and any(secret in content for secret in secrets):
                updated = json.dumps(redact_json(parsed, secrets), ensure_ascii=False)
            db.execute(f"update {messages} set content=? where id=?", (updated, row_id))
            changed_messages += 1
        state_rows = db.execute(
            f"select session_id, key, value from {state}{where}", where_args
        ).fetchall()
        for session_id, key, value in state_rows:
            updated = redact_text(value, secrets)
            if updated == value:
                continue
            try:
                parsed = json.loads(updated)
            except json.JSONDecodeError:
                parsed = None
            if parsed is not None:
                updated = json.dumps(redact_json(parsed, secrets), ensure_ascii=False)
            db.execute(
                f"update {state} set value=? where session_id=? and key=?",
                (updated, session_id, key),
            )
            changed_state += 1
        db.commit()
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()

    check = sqlite3.connect(f"file:{args.database}?mode=ro", uri=True)
    after = report_counts(check, secrets, args.session)
    check.close()
    if after["messages"] or after["session_state"]:
        raise SystemExit(f"redaction incomplete: {after}")
    print(
        json.dumps(
            {
                "dry_run": False,
                "before": before,
                "after": after,
                "changed": {"messages": changed_messages, "session_state": changed_state},
                "backup": str(backup),
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
