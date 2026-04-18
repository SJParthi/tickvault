# Claude Co-Work — How Claude Code sessions operate tickvault

> **Authority:** CLAUDE.md > `.claude/rules/project/observability-architecture.md` > this file.
> **Scope:** How Claude Code (on laptop, on EC2 via Lambda bridge, via
> `/loop`, or a fresh session) interacts with the tickvault stack
> **without human intervention**. Every tool path here is automated.

## The operator's directive (verbatim)

> "extreme comprehensive 100 percentage automation especially which
> should be always viewable readable debuggable searchable findable
> especially using AWS Claude code and Claude cowork okay I mean no
> manual inputs or manual intervention should never ever be expected."

This document proves every word is mechanically delivered.

## Viewable — every piece of state is a path

| What | Where | How Claude reads it |
|---|---|---|
| Live ERROR events | `data/logs/errors.jsonl.YYYY-MM-DD-HH` | `mcp__tickvault-logs__tail_errors(limit=N)` |
| Top signatures | `data/logs/errors.summary.md` | `mcp__tickvault-logs__summary_snapshot()` |
| Triage audit trail | `data/logs/auto-fix.log` | `mcp__tickvault-logs__triage_log_tail(limit=N)` |
| Firing alerts | Alertmanager :9093 | `mcp__tickvault-logs__list_active_alerts()` |
| Any metric value | Prometheus :9090 | `mcp__tickvault-logs__prometheus_query(query)` |
| Any signature history | errors.jsonl.* | `mcp__tickvault-logs__signature_history(sig)` |
| Novel-signature candidates | errors.jsonl.* | `mcp__tickvault-logs__list_novel_signatures(since_minutes)` |

No grepping, no tail-F'ing, no curl chains. Every state view is one
typed tool call.

## Readable — human AND machine, same format

Every structured artefact is simultaneously:
- Markdown-renderable (humans read in any editor/browser)
- JSON/JSONL-parseable (machines read in one call)

| Artefact | Machine format | Human format |
|---|---|---|
| errors.jsonl | JSON per line | `jq` or `make tail-errors` |
| errors.summary.md | Markdown table | Render in VS Code/Grafana/cat |
| triage-seen.jsonl | JSON per line | `jq` |
| auto-fix.log | Text with `key=value` fields | Grep/awk |
| validate-automation output | `[STATUS] name` per line | Read top-to-bottom |
| doctor output | Same `[STATUS]` lines | Read top-to-bottom |

## Debuggable — from alert to root cause in one chain

The canonical Claude flow when a Telegram alert fires:

```
Telegram: "🔴 OMS-GAP-03 CircuitBreakerOpen"
    │
    ├─> Claude: list_active_alerts()
    │       sees full alert context + runbook link
    │
    ├─> Claude: find_runbook_for_code("OMS-GAP-03")
    │       points at docs/runbooks/oms-risk.md
    │       gets 3 lines of context + preview
    │
    ├─> Claude reads runbook (1 tool call)
    │
    ├─> Claude: prometheus_query("tv_circuit_breaker_state")
    │       confirms state = 1 (OPEN)
    │
    ├─> Claude: signature_history("<hash>")
    │       gets all historical occurrences
    │
    └─> Claude posts analysis + fix PR
```

Zero shell commands. Zero path guessing. Zero context switching.

## Searchable — 3 search axes

### By ErrorCode

Every ERROR log carries a `code` field (Phase 1.2 migration) + a
stable `signature_hash` field (Phase 5). Claude searches by either:

```
mcp__tickvault-logs__tail_errors(limit=100, code="DH-904")
mcp__tickvault-logs__signature_history(signature="abcd1234efgh5678")
```

### By live metric

```
mcp__tickvault-logs__prometheus_query("rate(tv_ticks_processed_total[1m])")
mcp__tickvault-logs__prometheus_query("tv_circuit_breaker_state")
```

### By novel pattern

```
mcp__tickvault-logs__list_novel_signatures(since_minutes=60)
```

## Findable — Claude auto-discovers the runbook

`find_runbook_for_code` scans 2 directories:
- `docs/runbooks/*.md` — operator runbooks
- `.claude/rules/**/*.md` — enforcement rules

Returns every match + first-seen line + 5-line preview. Claude never
has to ask "where is the runbook for X?" — the MCP answers in one
call.

Cross-validated by `runbook_cross_link_guard` — every path referenced
in any runbook must resolve on disk OR be allowlisted with a TODO.
So when Claude navigates from a runbook to a linked script or rule
file, the link always works.

## The five operator-morning commands

Every operator interaction reduces to ONE of these. Each is
typed-input-less (pass no args):

```bash
make doctor              # 7-section total health (exit 0 = all green)
make validate-automation # 30-guard test suite (exit 0 = no regression)
make errors-summary      # cat data/logs/errors.summary.md
make tail-errors         # live tail of errors.jsonl.* with jq pretty-print
make triage-dry-run      # simulate .claude/hooks/error-triage.sh
```

An operator who remembers these 5 commands — and nothing else — can
run tickvault zero-touch.

## Claude Code session types

### Type 1 — Local dev session (laptop)

Already auto-configured via `.mcp.json` at repo root. The
`tickvault-logs` MCP server starts automatically when Claude Code
opens the workspace. All 8 tools immediately available.

### Type 2 — EC2 `claude-triage` tmux session (Phase 8.2)

Long-running Claude CLI inside a pre-created tmux pane on the
production EC2:

```bash
# Once, at EC2 bootstrap:
tmux new-session -d -s claude-triage 'claude code'
```

CloudWatch alarms → SNS → `claude-triage` Lambda → SSM RunCommand →
`tmux send-keys -t claude-triage "triage: <prompt>"` → Claude picks
up the prompt, runs the triage flow.

### Type 3 — `/loop` scheduled runbook

```bash
/loop 5m .claude/triage/claude-loop-prompt.md
```

Claude Code polls `errors.summary.md` every 5 minutes, applies
`.claude/triage/error-rules.yaml`, and takes one of four actions
(silence / auto_fix / auto_restart / escalate). Zero operator input.

### Type 4 — Parallel co-work session (this project's norm)

Two Claude sessions working on the same repo simultaneously. Each
takes ownership of a disjoint file set; the primary session sends a
bootstrap prompt that lists the forbidden files. Verified byte-level
merge-cleanness after both sessions finish (see PR 276 and PR 277).

## No-manual-intervention invariants

| Invariant | Enforcement mechanism |
|---|---|
| Every `error!` with a tracked code has a `code =` field | `error_code_tag_guard` — meta-test fails build |
| Every triage rule has a real script path + runbook path | `triage_rules_guard` — meta-test fails build |
| Every runbook reference resolves OR is allowlisted with TODO | `runbook_cross_link_guard` — meta-test fails build |
| Every Prometheus alert required by a runbook exists | `zero_tick_loss_alert_guard` + `resilience_sla_alert_guard` |
| Every dashboard panel required by morning-ops exists | `operator_health_dashboard_guard` |
| Every MCP tool name the Claude loop prompt depends on exists | `tickvault_logs_mcp_guard` |
| Every AWS Lambda scaffold stays count-gated off until EC2 | `claude_triage_lambda_guard` |
| Every Loki/Alloy service stays opt-in | `loki_alloy_profile_guard` |
| Every ErrorCode variant appears in a rule file AND vice-versa | `error_code_rule_file_crossref` |
| Every flush/drain/persist failure emits ERROR (not WARN) | `error_level_meta_guard` — 28 phrases pinned |

If any of these regresses, `make validate-automation` fails and the
PR cannot merge. Operator never sees silent drift.

## What's explicitly NOT automated (and why)

1. **Manually editing Dhan's SSM credentials** — rotation requires
   the Dhan web UI. This is a Dhan-side workflow, not ours.
2. **Provisioning the EC2 instance** — operator runs
   `make aws-apply` once. Everything post-provisioning auto-wires.
3. **Flipping `dry_run = false` to go live** — deliberate 2-person
   review gate per oms-risk.md runbook. Safety over automation.
4. **Accepting a draft PR from Claude's auto-triage** — operator
   reviews before merge. Claude can't merge its own PRs.

All four are bounded human gates, not ongoing intervention.

## How to verify 100% automation is live

```bash
# One command, fully covers:
make doctor

# If all [PASS]: zero-touch is working.
# If [FAIL]: the section header names the subsystem, find the
# matching runbook in docs/runbooks/README.md.
```
