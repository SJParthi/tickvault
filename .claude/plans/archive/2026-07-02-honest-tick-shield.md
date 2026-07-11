# Implementation Plan: Honest tick-conservation shield (kills the "No tick lost" false-OK)

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — audit action #1: the operator demanded no-false-OK
signals after the "No tick lost" shield showed WORKING ✅ through a mid-market crash-loop
where upstream ticks were genuinely lost. Per `audit-findings-2026-04-17` Rule 11 (no
false-OK) + `operator-charter-forever.md` §F (honest envelope).
**Branch:** `claude/charming-newton-qy6j0i`
**Changed area:** deploy-only (`deploy/aws/lambda/operator-control/handler.py` +
`test_handler.py`). No `crates/` code — the Rust conservation ledger, WS-GAP-06 gap
detector, `tv_ticks_lost_total` counters, and both audit tables are READ as ground truth,
never modified.
**Guarantee matrices:** cross-referenced per
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices apply;
deploy-only rows are N/A'd honestly in the PR body).

---

## Design

The operator-portal "No tick lost" shield was a **liveness heuristic**: `max_ticks_per_second > 1
→ WORKING ✅`. That is a false-OK — a box can stream 500 ticks/sec while upstream ticks are
lost in crash-loop disconnect windows. The shield becomes an honest **3-source composite**,
classified server-side by a pure function (`_classify_tick_conservation`) and rendered
verbatim by the UI:

1. **Conservation audit** (source a): the latest `tick_conservation_audit` rows (last ~2
   days, per feed — the 15:40 IST daily audit per
   `.claude/rules/project/tick-conservation-audit-error-codes.md`). Latest trading day only;
   `delivery_residual>0 / outcome_residual>0 / outcome='leak'` → RED `RESIDUAL: N ❌ (date)`;
   `partial_coverage / outcome='partial'` → AMBER `partial coverage ⚠ — mid-day restart`.
2. **Today's socket-down evidence** (source b): count of today's `ws_event_audit` rows with
   `event_kind='disconnected'` (the app's own in-market 09:15–15:30 IST disconnect class —
   off-hours drops are stamped `disconnected_off_hours`). Any drop → the shield can NEVER
   show green today: AMBER `N disconnect(s) today ⚠ — upstream ticks in those windows are
   unverifiable` (the honest capture-at-receipt envelope).
3. **Live liveness** (source c): the old ticks/sec rate survives ONLY as a decorating label
   when a+b are clean and the market is open: `BALANCED ✅ · capturing (X ticks/sec)`.

Shield renamed **"No tick lost" → "Tick conservation"**. Classification precedence
(ratcheted by tests): **residual > partial > disconnects > balanced > no-audit/idle/unreachable.**
The tooltip (title attribute) states the envelope: *"Proves every tick that REACHED the box
is stored. Ticks Dhan sent during a disconnect window never arrived and are outside this
proof."* Both new QuestDB reads run inside the existing `_VIEW_COMMANDS` SSM batch (fixed,
URL-encoded queries — no user input, no new round-trip). The #1330 stopped-box banner +
greyed shields behavior is preserved (title updated).

## Edge Cases

- **Fresh box, `tick_conservation_audit` table absent:** curl -f fails → empty CONSERVE →
  `no audit yet ⚠` (market open) or `idle (market closed)` — never green.
- **Table exists, zero rows:** /exp returns header only; `tail -n +2` yields empty → same as
  absent (honest).
- **Today's 15:40 audit not yet run:** the 2-day query window returns yesterday's rows; the
  label always names the audit date so a yesterday-verdict is never mistaken for today.
- **Multiple feeds (dhan + groww):** latest trading day, latest row per feed; worst verdict
  wins across feeds.
- **Yesterday residual + today balanced:** only the LATEST trading day's rows count
  (pinned by `test_only_latest_trading_day_rows_count`).
- **IST midnight / day boundary:** queries reuse the project's IST-stored-ts conventions —
  `ts IN today()` (same as every other view query) for disconnects, and a 2-day
  `dateadd('d',-2,now())` window for the audit so the UTC-vs-IST-label 5.5h skew can never
  drop the latest audit row.
- **Malformed CSV fragments:** skipped by the pure parser (`_parse_conserve_rows`), never a
  crash, never a fabricated row.
- **Balanced audit + disconnects today (the original incident):** AMBER disconnects state —
  the exact false-OK this fix kills.

## Failure Modes

- **QuestDB down / box SSM offline:** both fields empty → `unreachable` (AMBER), consistent
  with the other shields; the box-stopped case keeps the calm #1330 banner + greyed shields.
- **One query fails, the other answers:** empty CONSERVE + numeric WS_DISC → `no_audit`/
  `disconnects` as applicable; numeric-less WS_DISC alone never fakes a zero.
- **`ws_event_audit` absent:** WS_DISC empty → disconnect evidence unknown → if the audit rows
  exist the verdict still comes from them; if both absent → `unreachable`. No green without
  positive evidence.
- **Classifier bug risk (precedence inversion):** pinned by three explicit precedence tests.
- **SQL injection:** none — both queries are FIXED strings baked into `_VIEW_COMMANDS`; no
  user input reaches them (the read-only `_is_safe_sql` gate on the `sql` action is untouched).

## Test Plan

`python3 -m unittest discover deploy/aws/lambda/operator-control` — all green (127 tests;
+19 new in `TickConservationShield`):
- one test per shield state: balanced (open + closed), residual, partial, disconnects, idle,
  no_audit, unreachable;
- precedence ratchets: residual > partial > disconnects > balanced (3 tests);
- false-OK-is-dead ratchet: a 9999-tick/sec rate alone must NOT produce green BALANCED;
- latest-trading-day-only selection; envelope sentence on every verdict;
- parser happy/malformed/empty; `_parse_view` plumbing incl. empty stdout;
- `_VIEW_COMMANDS` URL-encoding ratchets (`%3E` for `>`, `%3D%27disconnected%27`, header skip);
- HTML ratchet: "Tick conservation" present, "No tick lost" + "WORKING ✅" gone, UI renders
  `j.tick_conservation` + `tc.envelope`.

## Rollback

Single squash-merged PR touching only the Lambda handler + its tests: `git revert <merge-sha>`
restores the previous shield wholesale. No schema change, no Rust change, no config change —
the two new SELECTs are read-only and vanish with the revert. Redeploy = the normal Lambda
deploy of `handler.py`.

## Observability

- The shield itself IS the observability surface: server-side verdict object
  (`state/label/cls/good/audit_date/disconnects_today/envelope`) is returned in the `view`
  JSON, so any API consumer (not just the browser) gets the honest classification.
- Underlying signals remain the ratcheted Rust chain (read-only here): TICK-CONSERVE-01
  (15:40 audit), AUDIT-WS-01 (ws_event_audit writes), WS-GAP-06 gap detector,
  `tv_ticks_lost_total`, the in-loop 60s conservation ledger.
- Cost: 2 extra fixed QuestDB reads inside the EXISTING single SSM `view` invocation (the
  8s poller already runs it) — no extra SSM round-trip, no extra Lambda call; the audit
  query is bounded (`LIMIT 8`, 2-day window), the disconnect count hits today's partition.

---

## Plan Items

- [x] Rewrite shield to 3-source honest composite (audit + disconnects + liveness label)
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_state_balanced_market_open_decorated_with_liveness, test_state_residual_is_red_and_counts_ticks, test_state_partial_is_amber, test_state_disconnects_today_blocks_green_despite_balanced_audit
- [x] Rename "No tick lost" → "Tick conservation"; keep #1330 stopped-banner behavior
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_html_renames_shield_and_drops_liveness_heuristic
- [x] Envelope tooltip on the shield
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_envelope_sentence_is_carried_on_every_verdict
- [x] Precedence + false-OK-dead ratchet tests, all states covered
  - Files: deploy/aws/lambda/operator-control/test_handler.py
  - Tests: test_precedence_residual_beats_partial_and_disconnects, test_precedence_partial_beats_disconnects, test_precedence_disconnects_beats_balanced, test_tick_rate_alone_never_produces_balanced
- [x] Archive stale portal-redesign plan (merged #1330)
  - Files: .claude/plans/archive/2026-07-02-portal-redesign.md

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Audit balanced, 0 drops, market open, 500 tps | GREEN "BALANCED ✅ · capturing (500 ticks/sec)" |
| 2 | Audit residual 42 | RED "RESIDUAL: 42 ❌ (date)" |
| 3 | Audit partial (mid-day restart) | AMBER "partial coverage ⚠" |
| 4 | Balanced yesterday + 2 in-market drops today (the incident) | AMBER "2 disconnect(s) today ⚠ — unverifiable" |
| 5 | Fresh box, no audit table, market open, high tps | AMBER "no audit yet ⚠" — never green |
| 6 | Market closed, no audit rows | "idle (market closed)" |
| 7 | QuestDB/box unreachable | "unreachable" (AMBER) |
