#!/bin/bash
# block-env-files.sh — Blocks creation/editing of .env files
# Enforces: "NO .env FILES. NO SECRETS ON DISK." principle

INPUT=$(cat)
TOOL_NAME=$(echo "$INPUT" | jq -r '.hook_event_name // empty')
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

# Only check file-writing tools
if [ -z "$FILE_PATH" ]; then
  exit 0
fi

# Block any .env file
if echo "$FILE_PATH" | grep -qE '\.env($|\.)'; then
  echo "BLOCKED: .env files are banned. Use AWS SSM Parameter Store for secrets." >&2
  exit 2
fi

exit 0
