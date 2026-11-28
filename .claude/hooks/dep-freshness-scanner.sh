#!/bin/bash
# Scan crates.io workspace pins. --json preserves the machine-readable report.
# --fail-on-minor returns 1 for minor/major drift. Incomplete scans return 2.
set -eu
tool_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "${CLAUDE_PROJECT_DIR:-$tool_root}"
exec cargo run --quiet --locked --manifest-path "$tool_root/scripts/dep-freshness/Cargo.toml" -- "$@"
