#!/usr/bin/env bash
# scripts/open-dashboard.sh — open the dashboard pane as a split.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"

# herdr resolves the pane's relative command against --cwd, so the pane has to
# launch from the plugin root.
exec "$herdr_bin" plugin pane open \
  --plugin andybarilla.flockboard \
  --entrypoint dashboard \
  --placement split \
  --direction right \
  --focus \
  --cwd "$script_dir/.."
