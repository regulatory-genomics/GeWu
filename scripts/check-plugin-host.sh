#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == --applications ]]; then
  exec "${PYTHON:-python3}" scripts/check-plugin-applications.py "${2:?expected application test manifest}"
fi
if [[ "${1:-}" == --help || "$#" == 0 ]]; then
  cat <<'EOF'
Usage: scripts/check-plugin-host.sh --contracts [--browser]
       scripts/check-plugin-host.sh --applications <plugin-test-manifest.json>

Contracts use Cargo and Python 3.11+ with jsonschema. Browser tests additionally
use Node.js, npm ci under gewu-gui/tests/browser, and Playwright Chromium.
See docs/plugins/authoring.md and scripts/check-plugin-applications.py --help.
EOF
  exit 0
fi
if [[ "$1" != --contracts || ( "$#" -gt 1 && "$2" != --browser ) || "$#" -gt 2 ]]; then
  echo 'Invalid arguments; use --help' >&2
  exit 2
fi
cargo test --workspace --locked
PYTHONDONTWRITEBYTECODE=1 "${PYTHON:-python3}" -m unittest discover -s sdk/python -p 'test_*.py'
PYTHONDONTWRITEBYTECODE=1 "${PYTHON:-python3}" -m unittest discover -s scripts/tests -p 'test_plugin_*.py'
cargo build -p gewu --locked
if [[ "${2:-}" == --browser ]]; then
  node --test gewu-gui/tests/panels.test.mjs
  npm --prefix gewu-gui/tests/browser test
fi
