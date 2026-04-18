# tickvault-logs MCP Server

Phase 7.2 of `.claude/plans/active-plan.md` — exposes the zero-touch
error observability pipeline to Claude Code as structured MCP tools.

## Why

The observability chain already writes:
- `data/logs/errors.jsonl.YYYY-MM-DD-HH` — structured JSONL, ERROR-only, hourly rotated
- `data/logs/errors.summary.md` — human + Claude-readable signature summary
- `data/logs/auto-fix.log` — audit trail of triage-hook actions
- `.claude/state/triage-seen.jsonl` — edge-trigger dedup state

Without this MCP, Claude has to `Bash tail` + `jq`-parse the raw files.
With it, Claude gets typed tools that return aggregated, filtered,
deduplicated views in one call.

## Tools exposed

| Tool | Purpose |
|---|---|
| `tail_errors` | Last N ERROR events, optionally filtered by `code` |
| `list_novel_signatures` | Signatures first seen in the last N minutes |
| `summary_snapshot` | Current `errors.summary.md` contents (markdown) |
| `triage_log_tail` | Last N lines of `auto-fix.log` |
| `signature_history` | All occurrences of a specific signature hash |

Every tool returns JSON. No free-text scraping.

## Running

```bash
# One-shot smoke test (no MCP stdio loop; prints tool list + demo)
python3 scripts/mcp-servers/tickvault-logs/server.py --self-test

# MCP stdio mode (what Claude spawns)
python3 scripts/mcp-servers/tickvault-logs/server.py
```

## Config

The MCP server reads `data/logs/` by default; override via
`TICKVAULT_LOGS_DIR` env var.

Registered via `.mcp.json` at the workspace root.

## Dep-free

Uses only Python stdlib (json, pathlib, sys, os, datetime). No pip install.
