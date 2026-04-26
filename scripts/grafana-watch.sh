#!/usr/bin/env bash
# Watch deploy/docker/grafana/provisioning/ and auto-reload Grafana on change.
#
# Usage:
#   make grafana-watch
#
# Runs in the foreground — leave it open in a terminal while editing alert
# rules or dashboard JSON. Each save triggers `make grafana-reload`, so
# changes appear in Grafana within ~10 seconds without manual restart.
#
# Why a watcher script instead of a sidecar container:
#   - Zero docker-compose changes (no new container, no RAM budget impact)
#   - Zero AWS-budget impact (script only runs when the operator launches it)
#   - Works identically on macOS (fswatch) and Linux (inotifywait)
#   - Reversible by deleting this file
#
# Stop with Ctrl+C.

set -euo pipefail

WATCH_DIR="deploy/docker/grafana/provisioning"

if [ ! -d "$WATCH_DIR" ]; then
    echo "Error: directory not found: $WATCH_DIR"
    echo "Run from the repo root."
    exit 1
fi

echo "Watching $WATCH_DIR for changes — Ctrl+C to stop."
echo ""

reload_now() {
    echo "[$(date +%H:%M:%S)] change detected — reloading Grafana..."
    if make grafana-reload >/dev/null 2>&1; then
        echo "[$(date +%H:%M:%S)] reload OK"
    else
        echo "[$(date +%H:%M:%S)] reload FAILED — is Docker running?"
    fi
    echo ""
}

if command -v fswatch >/dev/null 2>&1; then
    # macOS path (fswatch ships via Homebrew: brew install fswatch)
    fswatch -o "$WATCH_DIR" | while read -r _; do reload_now; done
elif command -v inotifywait >/dev/null 2>&1; then
    # Linux path (apt install inotify-tools)
    inotifywait -m -r -q -e modify,create,delete,move "$WATCH_DIR" | \
        while read -r _; do reload_now; done
else
    cat <<EOF
Error: no file watcher installed.

  macOS:  brew install fswatch
  Linux:  sudo apt install inotify-tools

Or just run \`make grafana-reload\` manually after each edit.
EOF
    exit 1
fi
