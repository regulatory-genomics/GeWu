#!/usr/bin/env python3
"""Run real application gate: JSON array of {id,package,settings,workspace,scenario,guiSpec} objects.

Paths can be absolute or relative to the manifest. A non-zero command or missing
package/runtime fails the gate; application-specific prerequisites belong in the
adapter settings and test spec. The manifest can supply realSmokeArgv (no shell).
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--require-plugins", help="Require exactly these comma-separated IDs")
    parser.add_argument("--validate-only", action="store_true", help="Check the set of plugin IDs without running tests")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    base = args.manifest.absolute().parent
    try:
        entries = json.loads(args.manifest.read_text())
    except (ValueError, OSError) as exc:
        parser.error(str(exc))
    if not isinstance(entries, list) or not entries or any(
        not isinstance(item, dict) or not isinstance(item.get('id'), str)
        or not re.fullmatch(r'[a-z][a-z0-9.-]{0,127}', item['id']) for item in entries
    ):
        parser.error('manifest must contain a nonempty array of entries with valid plugin IDs')
    ids = {item['id'] for item in entries}
    if len(ids) != len(entries):
        parser.error('duplicate plugin IDs')
    if args.require_plugins is not None and ids != set(args.require_plugins.split(',')):
        parser.error(f'require exactly: {args.require_plugins}')
    if args.validate_only:
        return 0
    failed = False
    for item in entries:
        try:
            package, settings, workspace, scenario, spec = [(base / item[k]).resolve(strict=True) for k in ('package','settings','workspace','scenario','guiSpec')]
            smoke = item['realSmokeArgv']
            if not isinstance(smoke, list) or not smoke or not all(isinstance(arg, str) for arg in smoke):
                raise ValueError('realSmokeArgv must be a nonempty argv array')
            subprocess.run(smoke, check=True, cwd=workspace)
            subprocess.run([sys.executable, 'scripts/plugin-conformance.py','--package', package,'--settings',settings,'--workspace',workspace,'--scenario',scenario,'--schema-root','gewu-protocol/schema'],check=True,cwd=root)
            subprocess.run(['npm','--prefix','gewu-gui/tests/browser','test','--','application-plugin.spec.mjs'], check=True, cwd=root,
                           env={**os.environ,'GEWU_APP_SPEC':str(spec),'GEWU_APP_PLUGIN':str(package)})
            print(f"PASS: {item['id']}", flush=True)
        except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as exc:
            print(f"FAILED/BLOCKED: {item['id']}: {type(exc).__name__}", file=sys.stderr, flush=True)
            failed = True
    return int(failed)


if __name__ == '__main__':
    sys.exit(main())
