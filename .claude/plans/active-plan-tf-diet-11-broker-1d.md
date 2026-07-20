# Implementation Plan: Timeframe Diet — 11 Frames + Broker-Pulled 1d

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) — two dated verbatim quotes below, recorded under the operator's standing pre-authorization ("every plan file any session writes for work I have ordered — treat its Status as APPROVED by me the moment it exists").

## Operator authority (verbatim, typos included — 2026-07-20)

> **Quote 1:** "Dude one more newer requirement which is have only one to give minutes sequential dude which is 1, 2, 3, 4, 5 and 15 min and 1 hr 2hr 3h 4h and 1 d alone dude so now fell me dude how does this work can you update this dude okay across our entire codebase and workspace dude okay?  See only next day after the pre market gets started pull the 1d data dud eokay? First fell me what do you udnerstand here dude okay?"

> **Quote 2:** "See its simple bro one day shoudl be pulled from its own broker data dude okay? But for these timeframes very simple dude just remove starting 6 till 14 mins dude and just have our current 15m and 1h, 2h, 3h, 4h dude do you understand what Ima kdogn sude"

## The contract

1. **The candle timeframe set becomes EXACTLY 11:** `1m, 2m, 3m, 4m, 5m, 15m, 1h, 2h, 3h, 4h, 1d`.
   **Removed (10 of today's 21):** `6m, 7m, 8m, 9m, 10m, 11m, 12m, 13m, 14m` (Quote 2: "remove starting 6 till 14 mins") **plus `30m`**.
   **30m flag (contract-resolved — surfaced for the operator):** Quote 1's exhaustive "alone" list omits 30m and Quote 2 does not name it; the contract's "EXACTLY 11 ... and any other frame not in the 11" resolves 30m as REMOVED. Arithmetic: 21 − 9 (6m..14m) − 1 (30m) = 11.
2. **1d is NEVER aggregated live.** Each broker's 1d row is pulled from that broker's OWN daily data ("one day shoudl be pulled from its own broker data") — Dhan daily via the granted historical surface, Groww daily via its granted candles surface — each row feed-tagged, on the NEXT trading day after pre-market starts (cadence-scheduled ~09:00–09:15 IST, before open), DEDUP-idempotent, bounded retries, fail-soft + loud on a missing broker day.
3. **Executor defaults (flagged for the operator in the PR):**
   - (a) Stored rows for retired timeframes are KEPT as history: writing stops; the retired tables (candles_6m..candles_14m, candles_30m) leave the DDL/retention name lists; they are NEVER added to RETIRED_QUESTDB_TABLES (that list is DROPPED at boot — keep-as-history forbids it).
   - (b) Intraday frames anchor at the 09:15 IST session open; ragged final buckets are allowed and documented honestly (session 09:15–15:30 IST means the 2h final bucket is 15:15–15:30, 3h final 15:15–15:30, 4h final 13:15–15:30 — the existing session-truncated-end behavior).
   - (c) Adversarial-verifier matrix built in from the start: last-TRADING-day (never calendar-yesterday) via the trading calendar across weekends/holidays; broker-vs-broker daily disagreement kept as TWO feed-tagged rows (DEDUP key `(ts, security_id, segment, feed)`), never merged or reconciled into one; no fabrication of a 1d row from live intraday sums when a broker day is missing (D1 is structurally absent from the live fold loop — the broker-pull lane is the ONLY candles_1d writer); removal completeness enforced by a build-failing source-scan ratchet.

## Plan Items

- [ ] Item 1 — Canonical enum shrink: TfIndex 21 → 11 + INTRADAY_TFS + seal-spill format-version bump
  - Files: crates/trading/src/candles/tf_index.rs, crates/trading/src/candles/seal_ring.rs, crates/storage/src/seal_spill.rs
  - Tests: test_tf_count_is_eleven, test_tf_ordinal_table_pins_the_eleven, test_intraday_tfs_is_all_minus_d1, test_seal_spill_format_bump_refuses_old_version_records
- [ ] Item 2 — Live fold iterates INTRADAY_TFS only (D1 never live-folded)
  - Files: crates/app/src/rest_candle_fold.rs
  - Tests: test_fold_bar_iterates_intraday_only, test_d1_never_produced_by_live_fold, test_ragged_final_buckets_seal_at_session_close
- [ ] Item 3 — Storage DDL / retention / config / metrics on the 11-set
  - Files: crates/storage/src/shadow_persistence.rs, crates/storage/src/shadow_candle_writer.rs, crates/storage/src/partition_manager.rs, crates/common/src/config.rs, config/base.toml, crates/app/src/metrics_catalog.rs, crates/common/tests/tf_symmetry_guard.rs
  - Tests: test_candle_table_names_are_the_eleven, test_retired_candle_tables_not_in_drop_list, test_timeframes_config_default_is_the_eleven, tf_symmetry_guard eleven-pin
- [ ] Item 4 — 1d broker-pull cadence lane (per-feed, pre-market, last-trading-day)
  - Files: crates/core/src/cadence/executor.rs, crates/core/src/cadence/runner.rs, crates/app/src/dhan_cadence_executor.rs, crates/app/src/groww_cadence_executor.rs, crates/app/src/cadence_boot.rs, crates/storage/src/daily_1d_persistence.rs, crates/common/src/config.rs, crates/common/src/constants.rs, crates/common/src/error_code.rs
  - Tests: test_last_trading_day_over_weekend_and_holiday, test_daily_rows_feed_tagged_both_brokers_coexist, test_missing_broker_day_writes_nothing, test_daily_pull_dedup_idempotent_on_rerun, test_daily_pull_bounded_retry_and_cutoff
- [ ] Item 5 — Removal-completeness source-scan ratchet
  - Files: crates/common/tests/tf_diet_guard.rs
  - Tests: no_retired_tf_labels_in_production_source, no_retired_candle_table_names_in_production_source, rule_and_plan_pin_the_eleven
- [ ] Item 6 — Rule-file dated grants + runbook + orphan-flip + docs sweep
  - Files: .claude/rules/project/no-rest-except-live-feed-2026-06-27.md, docs/error-runbooks/daily-1d-error-codes.md, crates/common/tests/dhan_api_coverage.rs, CLAUDE.md
  - Tests: error_code_rule_file_crossref covers DAILY1D-01 and DAILY1D-02, charts-historical constant test flipped orphan-to-live

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Monday pre-market pull | 1d rows for Friday (last trading day), one per feed |
| 2 | Holiday midweek, pull next morning | 1d rows for the last trading day; no holiday row fabricated |
| 3 | Dhan serves daily, Groww errors/empty | Dhan row written; Groww failure counted + logged loud (DAILY1D-01); nothing fabricated |
| 4 | Brokers disagree on daily OHLC | TWO rows, feed='dhan' and feed='groww', both kept — never merged |
| 5 | Pull re-run same morning | DEDUP upsert — idempotent, zero duplicate rows |
| 6 | Seal-spill file from the 21-TF era | Refused by the format-version bump, quarantined loudly; catchup refold from spot_1m_rest recovers |
| 7 | Live tick at 15:20 IST | Folds into the 10 intraday frames only; a live candles_1d write is impossible |

## Design

The canonical enum `TfIndex` (crates/trading/src/candles/tf_index.rs) shrinks from 21 variants to 11 with re-packed ordinals: M1=0, M2=1, M3=2, M4=3, M5=4, M15=5, H1=6, H2=7, H3=8, H4=9, D1=10; TF_COUNT=11. The ordinal==index invariant stays (it backs the fold-state arrays and channel arrays); the reshuffle is the documented SEMVER-break the enum header already names, absorbed in-repo in the same PR. A NEW const `INTRADAY_TFS: [TfIndex; 10]` (ALL minus D1) becomes the ONLY set the live aggregator iterates: `rest_candle_fold.rs` fold/seal/force-seal/refold/catchup paths switch from `TfIndex::ALL` to `INTRADAY_TFS`, making a live-summed 1d structurally impossible. `TfIndex::ALL` (11) remains the storage/DDL/retention/metrics iteration set. Because the seal-spill disk format encodes tf as a raw u8 ordinal and old ordinals 0–20 overlap the new 0–10 with different meanings (old 5 = 6m would decode as new 15m), the spill format version is BUMPED: old-version records are refused and quarantined loudly, never silently re-mapped — the durable truth is spot_1m_rest and the existing catchup refold (catchup_days=35) rebuilds intraday candles from it. The 1d lane extends the cadence machinery: `CadenceExecutor` (crates/core/src/cadence/executor.rs) gains `fetch_daily`, implemented by both `dhan_cadence_executor.rs` (POST /v2/charts/historical, the existing constant re-entering live use; SIDs 13/25/51) and `groww_cadence_executor.rs` (GET v1/historical/candles with a new GROWW_CANDLE_INTERVAL_1DAY="1day" token; the 4 Groww spot indices incl. the runtime-resolved VIX); a new `run_daily_pull_loop` in runner.rs (mirroring run_expiry_resolution_loop: pre-market window, AbortOnDrop, bounded retries until success or the 15:30 cutoff) computes the last N=5 TRADING days via the trading calendar, pulls each broker's own daily bars for that window (self-heals missed days and supplies prev-day closes for the pct columns), stamps ts = 09:15 IST session open of each trading day, and upserts into candles_1d via a new daily-1d writer in crates/storage with the existing DEDUP key (ts, security_id, segment, feed). New config `[daily_1d_pull]` is serde-default-OFF with base.toml enabled=true (config flip pre-authorized), gated on the existing [cadence] lanes.

## Edge Cases

Weekend/holiday: the pull targets the last TRADING day (calendar-driven), so Monday pulls Friday and post-holiday mornings pull the pre-holiday session; a 5-trading-day window makes a missed morning self-heal on the next run. Ragged buckets: the 6h15m session leaves final buckets of 15m (2h/3h) and 2h15m (4h) — kept, sealed at session close by the existing session-truncated-end logic, documented. Groww "1day" interval token is UNVERIFIED-LIVE: first live pull is the probe; empty/reject is counted + logged loud, never fabricated. VIX groww_symbol may be unresolved on a Groww-disabled boot: skipped + counted, core indices never blocked. Old spill records after the format bump: refused + quarantined (scenario 6). Retired-table history stays readable in QuestDB (tables freeze on disk; no DDL touches them). Catchup refold spans up to 35 days and must iterate INTRADAY_TFS only, or it would re-fabricate 1d — pinned by test. Boot after 09:15: the daily loop still runs immediately (late but same-day); boot before 09:00 waits for the window.

## Failure Modes

Broker daily endpoint down: bounded retries inside the pre-market loop, then hourly until the 15:30 cutoff; every failure is a coded structured log DAILY1D-01 (log-sink-only per the Dhan noise-lock 4-item set — NO new Telegram family) with feed + day named; the day self-heals via the 5-day window tomorrow. Partial broker response (some SIDs missing): present rows written, missing SIDs counted + logged; never inferred. QuestDB write failure: the existing writer error path (coded error, bounded retry, never a crash). Row-level anomaly (zero/negative OHLC from a broker): row skipped + DAILY1D-02 logged, never written. Seal-spill version mismatch: refuse + quarantine loudly (never silent decode); durable truth refolds. Config junk in [daily_1d_pull]: serde default OFF = fail-safe; an absent section disables the lane.

## Test Plan

Scoped per the testing-scope rule: cargo test -p tickvault-trading, -p tickvault-app, -p tickvault-storage, -p tickvault-core; crates/common edits escalate to workspace. Every Plan Item lists its named tests above; ratchets added: the tf_index 11-pin, the tf_symmetry_guard 11-set cross-surface pin, tf_diet_guard (source-scan banning retired labels/table names in crates/**/src), the partition_retention_coverage_guard update, the seal-spill format-bump refusal test, and the dhan_api_coverage orphan-to-live flip. The 1d lane is tested against the adversarial-verifier matrix: last-trading-day weekend/holiday fixtures, two-feed coexistence, missing-day non-fabrication, DEDUP idempotence, bounded retry/cutoff. Hostile-reviewer + refuter rounds run to 2 consecutive clean rounds before ready-flip; every push is routed to the external adversarial verifier session; merge additionally gates on its 2-consecutive-clean verdict on the exact final head plus All Green.

## Rollback

One revert of the PR restores the 21-TF world: the enum, const lists, DDL name lists, and fold loop are all in-repo constants with no data migration. Stored data is unaffected in both directions — retired tables were never dropped (keep-as-history), and candles_1d rows written by the lane are plain DEDUP-keyed rows old code ignores. Seal-spill: after a revert, old code refuses NEW-version spill files the same loud way — spills are a transient absorption tier, and the catchup refold from spot_1m_rest recovers intraday state, so the refusal costs nothing durable. Config rollback alone ([daily_1d_pull] removed or enabled=false) disables the 1d lane without a code revert.

## Observability

Counters: tv_daily1d_pull_total{feed,outcome} and tv_daily1d_rows_written_total{feed} land in metrics_catalog. Every failure path is a coded structured log carrying code = DAILY1D-01 (pull/day-level degrade) or DAILY1D-02 (row-level anomaly), both LOG-SINK-ONLY — the Dhan noise-lock 4-item Telegram set is untouched and no new NotificationEvent family is added. Runbook: docs/error-runbooks/daily-1d-error-codes.md (crossref-test mentions both codes; runbook_path() points at it). Boot logs one line naming the 11-frame set and the daily-lane gate state. The RAM/table savings (21 → 11 live tables + fold arrays) are quantified in the PR body.

## Per-Item Guarantee Matrix

Every plan item above carries the 15-row + 7-row guarantee matrix BY CROSS-REFERENCE to `.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrix file), per that rule's cross-reference clause. Plan-Wide Proof Matrix — the load-bearing rows made concrete for this change:

| Demand | Mechanical proof for this plan |
|---|---|
| 100% code coverage | ratcheted per-crate floors (quality/crate-coverage-thresholds.toml) stay green; every new pub fn lands with a matching test (pub-fn test guard) |
| 100% testing coverage | per-item Tests lines above; scoped suites for the five touched crates |
| 100% code checks | the pre-commit + pre-push gate batteries; tf_diet_guard + tf_symmetry_guard + tf_index ratchets added |
| 100% monitoring/logging/alerting | DAILY1D codes carry code= fields; counters in metrics_catalog; LOG-SINK-ONLY per the noise-lock (no new Telegram family) |
| Zero ticks lost | the tick hot path is untouched; the seal chain keeps ring, spill, DLQ; the spill format bump refuses stale records LOUDLY and the durable truth (spot_1m_rest + catchup refold) recovers — zero durable loss inside the envelope |
| O(1) latency | the fold loop iterates a const [TfIndex; 10] — the same O(1)-per-bar shape, strictly less work than the 21-array |
| Uniqueness + dedup | candles_1d DEDUP UPSERT KEYS (ts, security_id, segment, feed) — composite key + feed-in-key preserved |

**Honest 100% claim:** 100% inside the tested envelope, with ratcheted regression coverage: the 11-TF set pinned by build-failing tests (tf_index pin + tf_symmetry_guard + tf_diet_guard); the 1d lane DEDUP-idempotent with last-trading-day tests across weekend/holiday fixtures; the seal-spill format bump refuses stale-era records loudly (quarantine, never silent decode); the 200,000-seal ring to NDJSON spill to DLQ chain unchanged. NOT claimed: live Groww "1day" interval serving (UNVERIFIED-LIVE — the first live pull is the probe, fail-soft + loud); Dhan daily serving behavior after its orphan period (the first live pull measures it).
