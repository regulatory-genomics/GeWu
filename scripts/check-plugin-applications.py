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
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    base = args.manifest.absolute().parent
    entries = json.loads(args.manifest.read_text())
    if sorted(item['id'] for item in entries) != ['manimble', 'paper', 'tachyon']:
        parser.error('require one entry each for paper, tachyon and manimble')
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
