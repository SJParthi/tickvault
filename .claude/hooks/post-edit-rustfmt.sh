#!/usr/bin/env bash
# post-edit-rustfmt.sh — Auto-format Rust files after Edit/Write tool use.
# Hook: PostToolUse (matcher: Edit|Write)
set -euo pipefail

INPUT="$(cat)"
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // .tool_input.filePath // ""' 2>/dev/null)

# Only format .rs files
if [[ "$FILE_PATH" == *.rs ]] && [[ -f "$FILE_PATH" ]]; then
    rustfmt --edition 2024 "$FILE_PATH" 2>/dev/null || true
fi
