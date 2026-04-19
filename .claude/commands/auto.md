Fire the full automation chain with ONE command — no args, no thinking.
Delegates every step to existing automation so there's no new logic to
maintain: `/health` locally, `make dispatch-check` for 24/7 cloud, and
a final summary of any open PRs / firing alerts / novel error signatures.

This is the "did my box survive the day" / "is everything fine right now"
button.

## What to run (in order — report each row)

1. **Runtime attach.**
   ```
   [ -f .claude/.session-env ] && . .claude/.session-env
   echo "profile=${TICKVAULT_MCP_PROFILE:-?}  prom=${TICKVAULT_PROM_STATUS:-?}  qdb=${TICKVAULT_QDB_STATUS:-?}  api=${TICKVAULT_API_STATUS:-?}"
   ```

2. **Full local health check.**
   Run `/health` logic inline (do NOT re-invoke the slash command; you
   are already in a session). This prints the canonical 14-row PASS/FAIL
   table for static guards, doctor, audit, ERROR tail, summary snapshot,
   novel signatures, Prometheus/QuestDB pulse, alerts.

3. **Kick off cloud verification (fire-and-forget).**
   ```
   command -v gh >/dev/null 2>&1 \
     && gh workflow run claude-mobile-command.yml \
          -f instruction="run /health, list any firing alerts, list open PRs, report novel ERROR signatures in the last hour — single summary at the end" \
          -f allow_write=false \
     && gh run list --workflow=claude-mobile-command.yml --limit 1
   ```
   If `gh` is not installed, say so and skip — don't fail the whole
   command.

4. **Open-PR snapshot.**
   Call `mcp__github__list_pull_requests` (state=OPEN) and report
   count + titles. If any PR has failing CI or unresolved reviews,
   call them out.

5. **Novel signatures window.**
   Call `mcp__tickvault-logs__list_novel_signatures` (since=120 minutes).
   If zero → good. If non-zero → list codes + counts.

6. **Firing alerts.**
   Call `mcp__tickvault-logs__list_active_alerts`. Zero firing = good.
   Any firing → name them + severity.

## Output format

Single markdown block at the end. Terse. No wall of prose.

```
/auto — run <timestamp IST>
┌──────────────────────────────┬────────┬─────────────────────────────┐
│ Check                        │ Status │ Detail                      │
├──────────────────────────────┼────────┼─────────────────────────────┤
│ Session attach               │ PASS   │ profile=local               │
│ /health (14 rows)            │ PASS   │ 11 PASS / 3 SKIP (off-hours)│
│ Cloud verification fired     │ PASS   │ run #1234 queued            │
│ Open PRs                     │ INFO   │ #292 awaiting CI            │
│ Novel signatures (2h)        │ PASS   │ count=0                     │
│ Firing alerts                │ PASS   │ none                        │
└──────────────────────────────┴────────┴─────────────────────────────┘

Action items: <empty, or bullet list if any FAIL/INFO>
```

## Rules

- NEVER modify files. Read-only.
- NEVER push, commit, or merge anything.
- If any MCP tool is unreachable, report SKIP with reason ("OFFLINE; run `make docker-up`") — do NOT FAIL the whole command.
- Total runtime ≤ 45 seconds on a warm system. If any step exceeds
  10 s, print its elapsed time.
- If I say "fix it" after seeing the output, THEN act. Not before.

## Why `/auto` is different from `/health`

- `/health` = local static + live snapshot. No cloud fan-out.
- `/auto` = `/health` **plus** a cloud Co-work run (24/7 path) **plus** PR/alerts summary. One command = "check everywhere".
