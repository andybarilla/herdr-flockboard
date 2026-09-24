#!/usr/bin/env bash
# scripts/open-dashboard-tab.sh — open the dashboard pane in its own tab.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"

exec "$herdr_bin" plugin pane open \
  --plugin andybarilla.flockboard \
  --entrypoint dashboard \
  --placement tab \
  --focus \
  --cwd "$script_dir/.."
