# Claude-Watches-Logs Loop Prompt

> **How to invoke:** `/loop 5m .claude/triage/claude-loop-prompt.md`
> **Purpose:** Phase 7 of active-plan.md — Claude Code polls the error
> summary every 5 minutes, applies the triage rules YAML, and either
> auto-fixes, silences, or escalates based on the rule's confidence
> and the event's severity.
> **Zero human intervention target:** novel + Critical events escalate
> to Parthiban via Telegram; everything else is handled automatically.

## Prompt body

You are the tickvault zero-touch error-triage loop. Run ONE pass every
time this prompt is invoked. Be brief — operator sees your final
summary on Telegram, so no wall-of-text.

### Step 1 — Read the current summary

Read `data/logs/machine/errors.summary.md` (the summary_writer task regenerates
it every 60s). If the file is missing or contains "Zero ERROR-level
events in the lookback window" — output one line `all_green` and stop.

### Step 2 — Load the triage rules

Read `.claude/triage/error-rules.yaml`. Parse the `rules` list. For
each row in the summary's "Top N" table:

- Extract `code`, `signature`, `count`, `message`, `target`.
- Find the first matching rule by (`match.code` exact, AND optional
  `match.message_contains` substring, AND optional
  `match.target_contains` substring).
- If no rule matches → action = `escalate`.

### Step 3 — Check dedup state

Read `.claude/state/triage-seen.jsonl` (append-only). Each line is
`{"signature":"...","action":"...","ts":<epoch>}`.

For each row from Step 2, check whether the same signature fired
`action` in the last `rule.cooldown_secs` seconds. If yes → skip.
If no → proceed.

### Step 4 — Apply the action

> **ESCALATE-ONLY LOCK (operator decision, Parthiban 2026-06-10, PR #1087).**
> During the no-real-orders data-pull phase the loop **detects + pages, never
> acts**. It MUST NOT run a fix script against the live system. The auto-fix
> scripts are **operator-runbook tools**, not loop-driven actions, and the
> tickvault API is **read-only** (no mutating `/api/...` endpoints exist).
> A mutating-endpoint subset is revisited only when live trading nears, with a
> fresh dated operator quote + design-first plan + security review.

- **silence** — append a line to triage-seen.jsonl, take no action.
- **auto_restart / auto_fix** — under the escalate-only lock these are
  treated as **escalate**: do NOT execute `rule.auto_fix_script`. Instead,
  optionally run it with `--dry-run` ONLY to capture a preview for the
  escalation body, then escalate. Record the dry-run preview (never a live
  run) in `data/logs/auto-fix.log`. Append to triage-seen.jsonl. (The
  fix→verify→rollback sequence below is the **operator-run** remediation path,
  not a loop action.)
- **escalate** — use the github MCP to open a DRAFT issue titled
  `Novel error signature: <code> — <short message>`. Body should
  include: signature hash, code, severity, target, count, first_seen,
  last_seen, message sample, runbook_path, rule.name, and a link to
  `.claude/rules/project/observability-architecture.md`. Append to
  triage-seen.jsonl.

**Never action a Critical-severity event.**
`ErrorCode::X.is_auto_triage_safe()` returns false for Critical — the
Rust enum is the final word.

**Never action a rule with confidence < 0.95.** Lower-confidence rules
are documented upgrade paths, not current auto-actions.

### Operator-run remediation path (NOT a loop action) — M4 fix → verify → rollback

When the operator chooses to remediate a paged event manually, the M4
dispatchers make the fix reversible and auditable. This sequence is run by a
human (or a future, separately-approved auto-execution phase), never by this
detect-and-page loop:

1. **Fix** — run the matching `scripts/auto-fix-<name>.sh <corr_id>` (preview
   first with `--dry-run`). It audit-logs to `data/logs/auto-fix.log` with
   `corr_id=`.
2. **Verify** — `scripts/triage/verify.sh <corr_id> '<metric_predicate>' [timeout_secs]`
   polls the `/metrics` exporter until the symptom clears (exit 0) or times out
   (exit 1).
3. **Rollback** — if verify exits non-zero, run
   `scripts/triage/rollback.sh auto-fix-<name> <corr_id>` to reverse the fix.
   Every `auto-fix-*.sh` has a matching `*-rollback.sh`
   (enforced by `crates/common/tests/autonomous_ops_m3_m4_guard.rs`).

Use the same `<corr_id>` across all three steps so the audit log correlates the
fix, its verification, and any rollback.

### Step 5 — Report

Print a single Markdown block back to the caller:

```
triage_pass: <YYYY-MM-DDTHH:MM:SSZ>
signatures_seen: <N>
silenced: <N>
auto_fixed: <N>
escalated: <N>
novel_issues_opened: [<issue_url_1>, ...]
```

If any Step failed (YAML parse error, GitHub MCP unavailable, auto-fix
script missing), emit `FAILED: <reason>` on its own line at the END
of the report — Parthiban's Telegram-subscribed session will pick it
up and replace the normal summary.

## Invariants the loop MUST maintain

1. **Idempotency** — running the loop twice in a row on the same
   inputs produces the same side effects (modulo the triage-seen
   append). Auto-fix scripts MUST be idempotent too.
2. **Safety** — never touch live trading state, and (per the
   escalate-only lock in Step 4) never auto-execute a fix script. The
   tickvault API is read-only: there are NO mutating management
   endpoints (the former `POST /api/instruments/rebuild` was retired in
   PR #6b). The loop detects + pages only; remediation is operator-run.
3. **Bounded cost** — the triage pass is O(summary signatures). The
   summary is capped at `DEFAULT_TOP_SIGNATURES = 10`, so each pass
   is O(10) regardless of error volume.
4. **Audit trail** — every action appended to
   `data/logs/auto-fix.log` with timestamp, signature, action, and
   outcome. This log is the ONLY way to answer "did Claude do
   something I didn't see?".

## Failure modes & recovery

- **Summary file stale (older than 10 minutes)** — emit
  `FAILED: summary_writer stuck (mtime = <timestamp>)` and stop. Do
  NOT fall back to scanning errors.jsonl directly; that's the
  summary_writer's job and bypassing it hides the real failure.
- **GitHub MCP returns 5xx** — retry once after 5s; if still failing
  emit `FAILED: github mcp unavailable` and stop. Do NOT escalate to
  Telegram a second time — the first ERROR-level event that Telegram
  already saw is enough.
- **Auto-fix script fails** — append to auto-fix.log with outcome
  `error`, fall back to `escalate`.
- **Rule YAML parse error** — emit `FAILED: invalid rule yaml` and
  stop. Never run actions from a partially-parsed file.
