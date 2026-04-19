Run the full tickvault automation + observability health check and report
a single PASS/FAIL table. This is the one command an operator types to
answer "is everything OK?" — works in any Claude Code session (fresh or
existing, Mac local or AWS remote) with zero manual setup.

## What to run (in this order — each gets its own row in the final table)

1. **Runtime attach**
   Source the session env so the MCP log tools resolve:

   ```
   [ -f .claude/.session-env ] && . .claude/.session-env
   echo "profile=${TICKVAULT_MCP_PROFILE:-?}  prom=${TICKVAULT_PROM_STATUS:-?}  qdb=${TICKVAULT_QDB_STATUS:-?}  graf=${TICKVAULT_GRAF_STATUS:-?}  api=${TICKVAULT_API_STATUS:-?}"
   ```

2. **Static guards — `scripts/validate-automation.sh`**
   31 mechanical checks (compile gates, observability guards, file-level
   invariants, source-code invariants). Must be 31/31 PASS.

3. **Full-system health — `scripts/doctor.sh`**
   7 sections: source invariants, Docker stack, service endpoints,
   zero-touch artefacts, live error signal, env vars, AWS readiness.
   On a Mac with `make docker-up` done, every section must be PASS
   (AWS readiness legitimately SKIPs when terraform isn't initialized).

4. **100% audit — `scripts/100pct-audit.sh`**
   41 P-category dimensions (mechanically provable), 4 R (runtime),
   3 L (layered asymptotic), 3 I (impossible-absolute). Report only
   P + R counts; L + I are advisory per `100pct-audit-tracker.md`.

5. **Live ERROR tail — MCP `tail_errors`**
   Call `mcp__tickvault-logs__tail_errors` with `limit: 10`. Report the
   count, plus the top code breakdown (e.g. `I-P1-11×2, WS-GAP-03×1`).
   `count=0` is the healthy baseline.

6. **Live summary — MCP `summary_snapshot`**
   Call `mcp__tickvault-logs__summary_snapshot`. Report whether it
   exists and whether `Zero ERROR-level events` is in the body.

7. **Novel signatures in the last hour — MCP `list_novel_signatures`**
   Call with `since_minutes: 60`. Any novel signature = operator
   attention needed. Report count + top 3 (if any).

8. **Prometheus pulse — MCP `prometheus_query`**
   Run these 5 queries, report value (skip with "N/A — Prometheus
   OFFLINE" if `TICKVAULT_PROM_STATUS != REACHABLE`):

   - `sum(tv_ticks_dropped_total)` — must be 0
   - `sum(tv_depth_sequence_holes_total)` — operator reviews rate
   - `tv_instrument_registry_cross_segment_collisions` — known collisions only (e.g. 2)
   - `tv_websocket_connections_active` — 0 off-hours, 5 during 9:00-15:30 IST
   - `sum(tv_questdb_spill_bytes)` — 0 during steady state

9. **QuestDB pulse — MCP `questdb_sql`**
   Run these 3 queries, report row count / first value (skip with "N/A —
   QuestDB OFFLINE" if `TICKVAULT_QDB_STATUS != REACHABLE`):

   - `SELECT count() FROM ticks WHERE ts > dateadd('h', -1, now())` — hour's tick count
   - `SELECT count() FROM historical_candles` — total candles
   - `SELECT count() FROM derivative_contracts WHERE status='active'` — active contracts

10. **Active alerts — MCP `list_active_alerts`**
    Any firing alert = red row in the table.

## Output format

Print **exactly** this markdown table at the end (no filler prose above/below):

```
| # | Check | Status | Detail |
|---|-------|--------|--------|
| 1 | Runtime attach | PASS | profile=local prom=REACHABLE qdb=REACHABLE graf=REACHABLE api=OFFLINE |
| 2 | Static guards (validate-automation) | PASS | 31/31 |
| 3 | Doctor full-system health | PASS | 6/7 sections green (aws SKIP) |
| 4 | 100% audit | PASS | P=41/41 R=4/4 |
| 5 | ERROR tail (last 10) | PASS | count=0 |
| 6 | Summary snapshot | PASS | zero ERROR events in window |
| 7 | Novel signatures (60min) | PASS | count=0 |
| 8 | Prometheus — ticks_dropped | PASS | 0 |
| 8 | Prometheus — depth_seq_holes | PASS | 0 |
| 8 | Prometheus — id_collisions | PASS | 2 (expected: NIFTY id=13, BANKNIFTY id=25) |
| 8 | Prometheus — ws_active | INFO | 0 (off-hours expected) |
| 8 | Prometheus — qdb_spill_bytes | PASS | 0 |
| 9 | QuestDB — ticks last 1h | INFO | 0 (off-hours expected) |
| 9 | QuestDB — historical candles | PASS | 220500 |
| 9 | QuestDB — active contracts | PASS | 105846 |
| 10 | Active alerts | PASS | none firing |
```

## Rules

- If anything is `FAIL`, name the runbook file from `docs/runbooks/` that
  applies. If no runbook exists, say so — do not invent one.
- `INFO` is the right status for "expected 0 off-hours" — don't flag it
  as `FAIL`.
- If `TICKVAULT_QDB_STATUS=OFFLINE`, mark row 9 as `SKIP — Docker not up;
  run 'make docker-up' to enable`.
- If `TICKVAULT_PROM_STATUS=OFFLINE`, same for row 8.
- Never hallucinate values. If an MCP tool returns nothing, report `N/A`.
- Do NOT fix anything unless I say so. Just report. If I ask for fixes,
  apply them one at a time with a ratchet test each.

## Performance budget

Total runtime ≤ 60 seconds on a warm system. If a single check exceeds
10 seconds, print its elapsed time next to the status so I can see
which one is the bottleneck.
