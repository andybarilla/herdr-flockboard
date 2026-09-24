#!/usr/bin/env bash
# scripts/pane-dashboard.sh — the dashboard pane's entrypoint.
# herdr resolves a pane's relative command against the pane cwd, so the pane
# must launch from the plugin root.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/flockboard"
exec "$bin" tui
