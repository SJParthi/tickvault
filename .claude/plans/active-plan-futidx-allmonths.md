# Implementation Plan: §36.7 FUTIDX all-months expansion (both feeds)

**Status:** APPROVED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — verbatim directive 2026-07-10, relayed via the
coordinator session: "instead of only one current month futures contracts just take all
the futures of these indices — I mean take all available applicable months futures."

Rule-file amendments landed FIRST (commit `docs(rules): §36.7 FUTIDX all-months`) per the
§15/§36 re-approval protocol; this plan governs the code commits that follow.

## Design

Expand the §36 FUTIDX grant from nearest-expiry-only to ALL available monthly expiries
`>= today` for the SAME 4 underlyings (NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO; SENSEX =
BSE_FNO), BOTH feeds. Design decisions (each locked in the §36.7 rule edit):

| # | Decision | Choice |
|---|---|---|
| D1 | Selector shape | New shared plural `select_index_future_expiries(...) -> Vec<NaiveDate>` owns the `>= today` boundary rule ONCE; the singular `select_index_future_expiry` DELEGATES (`.into_iter().next()`) and is retained for nearest-month identity |
| D2 | Degrade unit | Per-(underlying, expiry) for flood/ambiguity — a bad MONTH drops only that month; whole-underlying only for NoFutRows / AllExpiriesPast / BadExpiryFormat / BadNativeToken / MonthlySerialFlood. `IndexFutureMiss` gains `expiry: Option<NaiveDate>` (`None` = whole underlying). FUTIDX-01 counter labels stay `{feed, underlying}` (static, 2×4) — the month goes in the `error!` payload only |
| D3 | Month-count bound | NO hardcoded expected count. `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING: usize = 6`; >6 distinct future expiries = corrupt master → whole-underlying fail-closed miss `MonthlySerialFlood`. `MAX_INDEX_FUTURE_TARGETS = 4 × 6 = 24` (envelope debug/proptest bound, not an expected count) |
| D4 | Snapshot format | `PLAN_SNAPSHOT_FORMAT_CURRENT` 2 → 3 (forces one deterministic cold build on deploy day). R4 fail-closed arm re-keyed from per-canonical to per-(canonical, expiry_date) — distinct expiries per canonical are LEGAL; two DISTINCT SIDs for the same (canonical, expiry) stay fail-closed |
| D5 | Parity semantics | Per-canonical expiry-SET compare. A one-sided strict FAR-SUFFIX (every one-sided month > the shorter feed's max) = depth mismatch → `info!` + `tv_index_futures_parity_depth_mismatch_total{underlying}`, NO page. Any other divergence (nearest differs, a HOLE, or an underlying entirely one-sided) = FUTIDX-02 High naming the month(s). Fixes the verified `.find` first-per-canonical comparator bug — comparator + multi-month selector land in the SAME commit |
| D6 | Gauge | `tv_index_futures_selected{feed}` keeps its name; semantics become selected CONTRACT count (0..~12, envelope ≤24). /metrics-local only — not in the CloudWatch agent metric_selectors, no terraform consumer |
| D7 | Alarm gate for quiet far months | ALL future SIDs stay SEEDED in the tick-gap detector (WS-GAP-06 per-SID black-hole probe unchanged). A plan-derived exclusion set of NON-NEAREST-month IndexFuture SIDs is applied to the two alarm-facing counts only: the SLO tick_freshness silent filter (INDIA-VIX precedent) and the `tv_tick_gap_instruments_silent` gauge. Threshold 40 and the 0.95 SLO boundary unchanged |
| D8 | Rollback | Pure `git revert` of the code commits; a v3 snapshot under reverted v2 code degrades to ONE cold build via the v2 R4 arm. No feature flag; `dry_run = true` stands |
| D9 | Expiry day | `>= today` keeps the expiring month through T-0 alongside later months; it falls out next morning. NO rollover code |
| D10 | Commit order | Rule files first; source-literal guard pins change in the SAME commit as the source they pin; every intermediate commit green |

Crates touched: crates/core (`index_futures.rs`, `feed/groww/instruments.rs`,
`instrument_snapshot.rs`, `subscription_planner.rs`, `daily_universe.rs`,
`daily_universe_orchestrator.rs`), crates/app (`main.rs`, `groww_activation.rs`),
crates/storage (`tests/daily_universe_scope_guard.rs`), crates/common
(`error_code.rs` doc-only), `deploy/aws/terraform/app-alarms.tf` (comment-only).

## Edge Cases

- T-0: the expiring month is kept through its final session ALONGSIDE all later months;
  T+1 the expired month drops out of `>= today`.
- All expiries past → whole-underlying `AllExpiriesPast` miss (`expiry: None`).
- Duplicate expiry rows: exact-dup SID/token collapse first-row-wins (per month);
  distinct-SID ambiguity degrades ONLY that month (`expiry: Some(month)`).
- >6 distinct serials for one underlying → `MonthlySerialFlood`, whole underlying
  fail-closed, other underlyings unaffected.
- Unparsable expiry / non-numeric Groww token → existing per-underlying reasons.
- Vendor month-depth divergence: strict far-suffix = DepthOnly info; a HOLE (one-sided
  month ≤ the other feed's max) or nearest divergence = FUTIDX-02 Divergence page;
  whole-underlying one-sided = Divergence.
- Cross-date parity refusal (same-trading-date gate) unchanged.
- v2 snapshot on v3 code → loader rejects (`format < 3`) → one cold build; v3 snapshot on
  reverted v2 code → v2 R4 per-canonical arm returns None on the 2nd month → cold build.
- Groww 1000-cap pressure: futures cap-priority prefix; worst case 24 futures by envelope.
- Midnight-crossing build re-derives the IST date per attempt (existing machinery).

## Failure Modes

- Per-month FUTIDX-01 degrade: month named in the `expiry` payload field; counter stays
  per-underlying (static labels).
- MonthlySerialFlood: whole-underlying fail-closed (never truncated-and-trusted).
- FUTIDX-02 Divergence page (High, never auto-triaged) vs DepthOnly info + counter.
- Planner-drop FUTIDX-01 (post-plan honesty) auto-inherits count semantics.
- Snapshot corruption → whole-snapshot None → cold build (fail-closed doctrine unchanged).
- Quiet far month: WS-GAP-06 per-SID log only — excluded from the two alarm-facing counts,
  so no false many-instruments-silent / SLO-degraded pages.
- Everything fail-soft: boot never blocks on futures; spot universe never affected.

## Test Plan

- `index_futures.rs`: plural-selector boundary tests
  (`test_select_index_future_expiries_returns_all_at_or_after_today`,
  `test_select_index_future_expiry_delegates_to_plural_first`,
  `test_select_index_future_expiries_keeps_expiring_month_and_later_on_t_zero`,
  `test_select_index_future_expiries_drops_expired_month_next_morning`),
  all-months selector (`test_select_index_future_contracts_picks_all_months_per_underlying`,
  `test_select_monthly_serial_flood_degrades_whole_underlying`, per-month degrade asserts in
  the dup/flood tests), proptest bound ≤ `MAX_INDEX_FUTURE_TARGETS` + per-canonical distinct
  expiries, set-parity tests (`test_cross_feed_parity_far_suffix_is_depth_only`,
  `test_cross_feed_parity_hole_is_divergence`,
  `test_cross_feed_parity_multiple_months_identical_ok`).
- Guard: `daily_universe_scope_guard.rs` renamed
  `futidx_scope_pinned_to_4_underlyings_all_monthly_expiries` + re-pointed never-roll pin
  (plural body owns `>= today_ist`; singular delegates).
- Orchestrator fixture `builds_universe_with_futidx_rows_selects_index_futures` → 8
  contracts / 2 months; planner
  `test_daily_universe_plan_futidx_plans_every_monthly_target`.
- Snapshot: `test_snapshot_index_future_same_expiry_distinct_sids_fail_closed`,
  `test_snapshot_index_future_distinct_expiries_per_underlying_accepted`,
  `test_snapshot_format_2_rejected_after_v3_bump`, FUTURE_COUNT 4 → 12 in
  `test_warm_resubscribe_lossless_at_production_scale`.
- Groww: `test_extract_index_future_entries_takes_all_months_four_underlyings`,
  `test_watch_set_includes_every_monthly_fno_future`,
  `test_extract_monthly_serial_flood_degrades_whole_underlying`, T-0 both-months assert,
  per-month degrade asserts, cap-pressure 12-future assert, shared_master_writer 2nd-month
  fixture extension.
- App: `test_far_month_future_sids_keeps_nearest_excludes_rest`,
  `test_far_month_future_sids_single_month_excludes_nothing`.
- Scoped runs: `cargo test -p tickvault-core --features daily_universe_fetcher`,
  `cargo test -p tickvault-app`, `cargo test -p tickvault-common`,
  `cargo test -p tickvault-storage --test daily_universe_scope_guard`; workspace escalation
  because crates/common is touched (doc-only).

## Rollback

`git revert` of the code commits (the §36.7 rule-file record stays — rule history is
append-only). A v3 snapshot read by reverted (v2) code passes the format gate (loader
rejects only `format < CURRENT`) but hits the v2 per-canonical R4 arm on the 2nd month →
whole-snapshot `None` → one clean cold build. No storage schema/DDL change, no config
flag, `dry_run` untouched, no WS connection change.

## Observability

- Gauge semantics change documented (contract count; /metrics-local, not CW-shipped) in
  futidx-4-error-codes.md §1.
- FUTIDX-01 payload gains the `expiry` field (`"ALL"` = whole underlying); counter labels
  unchanged (static, 2×4 bounded).
- New counter `tv_index_futures_parity_depth_mismatch_total{underlying}` (log-sink-only
  companion, no alarm — `tv_index_futures_cap_dropped_total` class).
- Boot-evidence `info!` per contract (~12 lines/feed/boot).
- Silent-gauge + SLO freshness far-month exclusion documented in
  futidx-4-error-codes.md §3 + an `app-alarms.tf` comment (no threshold change).
- `tv_prev_day_futidx_skipped_total` / `tv_cross_verify_futidx_skipped_total` now count
  ~12/day (role-keyed, auto-inherit).

## Plan Items

- [x] Item 1 — Rule-file amendments (commit 1, no code; guard phrase pins stay findable)
  - Files: .claude/rules/project/daily-universe-scope-expansion-2026-05-27.md,
    .claude/rules/project/websocket-connection-scope-lock.md,
    .claude/rules/project/operator-charter-forever.md,
    .claude/rules/project/futidx-4-error-codes.md,
    .claude/rules/project/groww-second-feed-scope-2026-06-19.md,
    .claude/rules/project/live-market-feed-subscription.md,
    .claude/rules/project/wave-5-error-codes.md, .claude/rules/dhan/instrument-master.md
  - Tests: futidx_scope_pinned_to_4_underlyings_nearest_expiry (pre-rename, green on
    commit 1), futidx_scope_rule_file_pins_forbidden_remainder
- [ ] Item 2 — Shared selector core: plural selector + per-month degrade +
  MonthlySerialFlood + set parity comparator + guard literal updates (one commit)
  - Files: crates/core/src/instrument/index_futures.rs,
    crates/storage/tests/daily_universe_scope_guard.rs, crates/common/src/error_code.rs
  - Tests: test_select_index_future_expiries_returns_all_at_or_after_today,
    test_select_index_future_expiry_delegates_to_plural_first,
    test_select_index_future_expiries_keeps_expiring_month_and_later_on_t_zero,
    test_select_index_future_expiries_drops_expired_month_next_morning,
    test_select_index_future_contracts_picks_all_months_per_underlying,
    test_select_monthly_serial_flood_degrades_whole_underlying,
    test_cross_feed_parity_far_suffix_is_depth_only,
    test_cross_feed_parity_hole_is_divergence,
    test_cross_feed_parity_multiple_months_identical_ok,
    futidx_scope_pinned_to_4_underlyings_all_monthly_expiries,
    futidx_scope_never_roll_source_pin, arbitrary_contract_sets_never_select_beyond_allowlist
- [ ] Item 3 — Multi-month plumbing: orchestrator fixture, planner test, snapshot v3 +
  R4 re-key, Groww per-expiry extractor loop + tests
  - Files: crates/core/src/instrument/daily_universe.rs,
    crates/core/src/instrument/daily_universe_orchestrator.rs,
    crates/core/src/instrument/subscription_planner.rs,
    crates/core/src/instrument/instrument_snapshot.rs,
    crates/core/src/feed/groww/instruments.rs,
    crates/core/src/feed/groww/shared_master_writer.rs
  - Tests: builds_universe_with_futidx_rows_selects_index_futures (8 contracts),
    test_daily_universe_plan_futidx_plans_every_monthly_target,
    test_snapshot_index_future_same_expiry_distinct_sids_fail_closed,
    test_snapshot_index_future_distinct_expiries_per_underlying_accepted,
    test_snapshot_format_2_rejected_after_v3_bump,
    test_warm_resubscribe_lossless_at_production_scale (12 futures),
    test_extract_index_future_entries_takes_all_months_four_underlyings,
    test_watch_set_includes_every_monthly_fno_future,
    test_extract_monthly_serial_flood_degrades_whole_underlying,
    test_master_writer_labels_fno_ltp_entries_futidx (2nd month)
- [ ] Item 4 — Far-month quiet-SID alarm gate (SLO freshness + silent gauge)
  - Files: crates/app/src/main.rs, deploy/aws/terraform/app-alarms.tf (comment only)
  - Tests: test_far_month_future_sids_keeps_nearest_excludes_rest,
    test_far_month_future_sids_single_month_excludes_nothing,
    test_cw_agent (cloudwatch_app_alarms_wiring unchanged-green)
- [ ] Item 5 — Final sweep: stale "≤4"/"nearest" comments, gates, workspace escalation
  - Files: crates/app/src/main.rs (comments), crates/app/src/groww_activation.rs (comment),
    crates/core/tests/prev_close_routing_5525125_guard.rs (doc comment)
  - Tests: full scoped battery + hooks (plan-gate, per-item-guarantee-check,
    banned-pattern-scanner)

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — the canonical 15-row 100% Guarantee Matrix and the
7-row Resilience Demand Matrix apply to EVERY item above. Per-item specifics:

- 100% code coverage: tests added for every new pub fn/variant; coverage delta ≥ 0
  (ratcheted floors unchanged).
- 100% audit coverage: N/A — no audit-table change (lifecycle rows auto-inherit
  per-contract; DEDUP keys untouched).
- 100% testing coverage: unit + boundary + property (proptest) + ratchet (source-scan
  guards) + fixture-integration categories named per item above.
- 100% code performance: all changes are cold-path/boot code — no hot-path allocation,
  no new per-tick work; the SLO/gauge scans reuse the existing cold-path walk class.
- 100% monitoring/logging/alerting: FUTIDX-01 `expiry` payload field, DepthOnly counter,
  boot-evidence lines; `error!` sites carry `code =` fields (tag-guard green).
- 100% scenarios: edge cases enumerated above, each with a named test.
- 100% functionalities: every new pub fn (`select_index_future_expiries`,
  `far_month_future_sids`) has a production call site + tests (pub-fn wiring/test guards).
- Zero ticks lost: no new tick-drop path; ring→spill→DLQ, SubscribeRxGuard, pool watchdog
  untouched.
- O(1): selection/parity are once-per-boot cold-path (flagged O(N) over master rows, as
  before); the hot tick path is untouched.
- Uniqueness + dedup: composite `(security_id, exchange_segment)` discipline on every new
  key — the far-month exclusion set is keyed `(sid, ExchangeSegment)`, the snapshot R4
  key is `(canonical, expiry)`, distinct months = distinct SIDs.

## Zero-Loss Guarantee Charter check

Per `zero-loss-guarantee-charter.md` §3 (boxes for THIS plan):
- [x] Code coverage: tests added; coverage delta ≥ 0
- [x] Audit coverage: N/A — no audit-table change (per-contract lifecycle rows auto-inherit)
- [x] Testing coverage: unit / boundary / property / ratchet / integration named above
- [x] Security: no secrets, no new input surface beyond the already-parsed vendor CSVs
  (existing flood/dedup envelopes retained per month); security-reviewer pass pre-PR
- [x] Monitoring: counters/gauge/payload documented in Observability
- [x] Logging: every `error!` carries `code = ErrorCode::X.code_str()`
- [x] Alerting: N/A — no new failure mode requiring an alarm (DepthOnly is deliberately
  log-sink-only; the far-month exclusion PREVENTS false alarms)
- [x] Scenarios/edge: enumerated above with named tests
- [x] Performance: N/A — not hot path (cold-path boot selection)
- [x] Recovery: rescue→spill→DLQ / reconnect untouched; snapshot fail-closed → cold build
- [x] Functionalities: every new pub fn has a test AND a call site
- [x] Extreme check: renamed/re-pointed ratchets fail the build on regression
- [x] No new tick-drop path; WS guards unchanged; no hot-path allocation; O(N) build steps
  flagged; QuestDB schema self-heal untouched; composite uniqueness + DEDUP keys preserved
- [x] Evidence: verbatim gate outputs recorded in the PR body / worker report
- [x] "100%" wording carries the envelope qualifier (§36.4 rewrite — "100% inside the
  tested envelope, with ratcheted regression coverage: …")
