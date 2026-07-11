# Implementation Plan: Dual-Feed Scoreboard PR-D — per-instrument cross-feed coverage (presence registry + unique wins)

**Status:** VERIFIED
**Date:** 2026-07-11
**Approved by:** Parthiban (operator) — dual-feed scoreboard directive 2026-07-10 (verbatim: *"run these two websockets live for a month... all tracked, captured, visualized, logged, monitored, 100% automated"*), PR-4 (final) slice of the synthesized scoreboard design (judge-approved 2026-07-10; PR-A #1473 + PR-B #1475 + PR-C #1476 merged). Task delegated by the coordinator session 2026-07-11.

> **Guarantee matrices:** this plan carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (the canonical tables), with the per-item proof column instantiated in
> the Test Plan + Observability sections below.

## Design

The FINAL scoreboard series PR: real per-instrument cross-feed coverage.

1. **`FeedPresenceRegistry`** (`crates/core/src/pipeline/feed_presence.rs`
   — sibling of `feed_lag_monitor`, the SAME crate both hot hook sites
   already import; the design sketch's `crates/common` placement is
   deliberately moved to core because papaya + the DHAT/bench infra live
   there and no consumer needs it below core). Cold-built slot map:
   per-feed `papaya::HashMap<(u64 sid, u8 seg), u32 slot>` over a CANONICAL
   cross-feed slot space — stocks paired by ISIN (`CsvRow.isin` ↔
   `WatchEntry.isin`), indices by `canonicalize_index_symbol`, §36 futures
   by `(canonical underlying, expiry)` contract identity. Per slot per
   feed: `[AtomicU64; 6]` = 375 session-minute presence bits ([09:15,
   15:30) IST). Fixed 2,048-slot planes = 192 KiB total. Unmapped
   instruments get feed-local slots (reported, never dropped); slot-cap
   overflow + unregistered folds are counted. IST-midnight `reset_daily`
   from BOTH existing force-seal tasks + a per-slot day-change clear at
   registration (belt and braces). Process-local honesty:
   `covers_full_session=false` when the process started after session
   open.
2. **Hot fold** `record_presence(feed, sid, seg, exchange_ts_secs)` —
   co-located with `record_dhan_tick` (both tick_processor arms) and
   `record_groww_tick` (the bridge drain): one relaxed enabled-gate load
   (`[scoreboard] presence_fold_enabled`, boot-read), pure minute math on
   the exchange timestamp already in hand (zero new clock reads), one
   lock-free papaya read, one relaxed `fetch_or`. Zero-alloc O(1) —
   DHAT-ratcheted + Criterion-budgeted (`feed_presence_record`, 100 ns).
3. **Cold slot builds** at the existing seams: Dhan =
   `presence_registration::register_dhan_presence_from_universe` next to
   `record_dhan_selection_from_universe` (cold orchestrator Step 3d + the
   §29 warm-snapshot path — the FUTIDX-02 both-paths lesson); Groww =
   `register_groww_presence_from_watch` in the `groww_activation` watch
   `Ok(set)` arm.
4. **15:45 drain** (`run_feed_scoreboard` step 5d): same-day runs drain
   the registry — feed-level `unique_win_minutes`/`both_minutes` flip from
   the SQL minute-set approximation to registry truth,
   `mapped/unmapped_instruments` + `covered_instrument_minutes` fill,
   `coverage_source` stamps `in_memory` (full session) / `mixed` (mid-day
   restart); config-gated `feed_coverage_daily` per-instrument rows
   (~1.5K, DEDUP-idempotent at the deterministic 15:45 ts). Step 6d
   coverage keep-better (mirror of the lag/outcome guards, shared step-5a
   read): a lower-ranked-source rerun folds the existing measured
   coverage columns forward.
5. **Dashboard slot-2 rows 2-4** (existing metrics/alarms ONLY — zero new
   EMF series): stall counters | per-feed catch-up seals | WS health |
   feed alarm strip | the honesty text widget.
6. **Docs**: runbook PR-4 caveats flipped to measured-since-PR-D (capture-
   boundary honesty kept), rule-file PR-D section, metrics_catalog
   zero-metrics stance note.

## Edge Cases

- Pre-open/post-close/Muhurat ticks: `presence_minute_index` discards
  outside [09:15, 15:30) — the 375 denominator stays honest (§30 always-on
  SIDs fold nothing off-session).
- Minute boundaries: 09:15:00→0, 09:15:59→0, 15:29:59→374, 15:30:00→None
  (unit-pinned).
- Unmapped instruments (ISIN-less stocks, unmatched index names, one-sided
  futures): feed-local slots, `-1` comparison sentinels on their coverage
  rows (exclusive-vs-nothing is not a measurement — the round-5 feed-off
  lesson), counted + named (bounded ≤20 sample) in the drain log.
- Groww index ids (bit-62 range) and future exchange_tokens never collide
  with Dhan SIDs: slots pair by IDENTITY keys, never native ids; the
  canonical row id is the Dhan SID when mapped (Groww-first registration
  order handled).
- Same-day re-registration (warm snapshot + background reconcile) is
  idempotent — bits survive; a NEW-day registration clears that slot-feed's
  bits even if the midnight reset was missed.
- Stale yesterday-only slots: drain filters on `registered_day ==
  target_day` — never leak into today's rows.
- Feed-off day: partner keeps `-1` unique/both sentinels even when the
  registry measured (pinned).
- Slot-cap overflow (>2,048): registrations counted + SCOREBOARD-01
  errored, drain flags — never silent.
- Unparsable SID / unknown FUTIDX segment rows: skipped at registration
  (the planner already pages that drop class); a key the fold can't see is
  never registered.

## Failure Modes

- Registry uninitialized (fold disabled / pre-init tick): free fns no-op —
  never panic, never allocate; scoreboard keeps the SQL fallback
  (`coverage_source='sql_backfill'`).
- Mid-day restart: process-local registry loses the pre-restart window —
  `covers_full_session=false` ⇒ `mixed` + the existing restart partial
  floor; per-instrument rows stamp `partial_coverage=true`.
- Post-close catch-up rerun / past-day backfill (empty registry): the
  coverage keep-better folds the day's measured registry columns forward
  (`stage="coverage_regression"`) — never erased with the SQL
  approximation (mirror of the lag guard, non-regressed).
- feed_coverage_daily append/flush failure: SCOREBOARD-01
  (`stage="coverage_append"/"coverage_flush"`), daily rows already flushed
  — the DEDUP-idempotent forced rerun backfills.
- Hot-path corruption defense: slot indices bounds-checked (skip, never
  panic); poisoned registration Mutex recovered via `into_inner`.

## Test Plan

- `feed_presence.rs` unit suite (14 tests): minute boundaries, ISIN/index/
  future pairing, Dhan-canonical-wins, unmapped sentinels, unregistered
  fold counter, out-of-session discard, disabled gate, day-change clear,
  midnight reset, same-day idempotency, overflow, partial-session flag,
  pre-init safety.
- `presence_registration.rs`: role→segment/pairing derivation, skip arms,
  `ist_day_from_date` epoch anchor, wiring ratchets (orchestrator +
  main.rs warm path + both tick_processor fold arms).
- `groww_activation.rs`: watch-entry derivation (index prefix-strip,
  ISIN, future, negative-id skip), Ok(set)-arm wiring ratchet.
- `groww_bridge.rs`: producer-site + midnight-reset ratchets (lag-test
  siblings). `secret_manager.rs`: init×2 + Dhan reset main.rs meta-guard.
- `feed_scoreboard_boot.rs`: apply_presence_coverage (truth flip, mixed,
  feed-off sentinels), detail-row builder (feed labels, partial flags),
  extended keep-better SQL pin, parse_existing_daily_coverage (short-row
  sentinels), coverage_source_rank + fold semantics.
- DHAT `crates/core/tests/dhat_feed_presence.rs` (≤1 KiB/≤8 blocks per
  10K folds) + Criterion `feed_presence` bench + `feed_presence_record`
  budget.
- Full 6-crate workspace suites green (summaries in the PR).

## Rollback

- `[scoreboard] presence_fold_enabled = false` → the hot fold no-ops
  (boot-read AtomicBool), the drain returns None, the scoreboard keeps the
  PR-A..C SQL behavior byte-identically; `coverage_detail_rows = false`
  stops the per-instrument rows; `[scoreboard] enabled = false` spawns
  nothing (the B12 switch, unchanged).
- Revert-safe: additive columns already shipped in PR-A; the coverage
  keep-better tolerates pre-PR-D short rows (sentinels + empty source).

## Observability

- Drain log line: per-feed mapped/unmapped counts, unregistered folds,
  covers_full_session, bounded unmapped symbol sample (Rule 11 naming).
- SCOREBOARD-01 arms: `presence_register_overflow`, `presence_overflow`,
  `coverage_append`, `coverage_flush`, `coverage_regression` — every
  `error!` carries the code (tag-guard).
- `feed_scoreboard_daily.coverage_source` + `feed_coverage_daily` rows are
  the queryable truth; the card's "Minutes only I had" line becomes
  registry truth on in_memory/mixed days (no card code change).
- Zero new metrics / EMF series / alarms (metrics_catalog stance note);
  dashboard rows 2-4 chart EXISTING series only.
- O(N) flags: drain O(slots × 12 words) cold; registration O(universe)
  cold; only `record_presence` is hot and only it is O(1).
- **Deviation record (review round 1, LOW):** the design-sketch
  `/api/stats` intraday presence surface is DEFERRED — the 15:45 drain
  log line + the `feed_coverage_daily` / `feed_scoreboard_daily` QuestDB
  tables are the intraday/queryable operator surface; a follow-up may add
  it to the debug handler. No code under `crates/api` changes in PR-D.

## Plan Items

- [x] Item 1 — `FeedPresenceRegistry` (slots, bitsets, pairing, drain,
  reset, global init/free fns)
  - Files: crates/core/src/pipeline/feed_presence.rs, crates/core/src/pipeline/mod.rs
  - Tests: test_presence_minute_index_session_boundaries, test_record_presence_and_drain_day_isin_paired_slot_compared, test_canonical_fields_are_dhan_even_when_groww_registers_first, test_future_pairing_key_is_contract_identity_not_native_id, test_unmapped_singleton_reported_with_sentinel_comparison_columns, test_unregistered_fold_counts_never_silent, test_out_of_session_ticks_never_set_bits, test_disabled_gate_no_bits_and_no_drain, test_stale_previous_day_slot_excluded_and_bits_cleared_on_new_day, test_reset_daily_clears_bits_and_counters, test_register_instruments_reregistration_is_idempotent_same_day, test_overflow_beyond_slot_cap_is_counted_never_silent, test_mid_day_process_start_flags_partial_session, test_init_feed_presence_and_global_free_fns_are_safe_pre_init
- [x] Item 2 — hot-path hooks (2 Dhan arms + Groww drain) + init + midnight resets
  - Files: crates/core/src/pipeline/tick_processor.rs, crates/app/src/groww_bridge.rs, crates/app/src/main.rs
  - Tests: test_dhan_presence_fold_sites_wired_into_tick_processor, test_record_presence_producer_site_wired_into_drain, test_feed_presence_is_wired_into_main
- [x] Item 3 — cold slot builds (Dhan universe + Groww watch)
  - Files: crates/core/src/instrument/presence_registration.rs, crates/core/src/instrument/mod.rs, crates/core/src/instrument/daily_universe_orchestrator.rs, crates/app/src/main.rs, crates/app/src/groww_activation.rs
  - Tests: test_dhan_presence_registrations_cover_all_roles_with_correct_keys, test_dhan_presence_registrations_skip_unparsable_sid_and_bad_segment, test_ist_day_from_date_epoch_anchor, test_dhan_presence_registration_sites_wired, test_groww_presence_registrations_index_stock_future_keys, test_groww_presence_registrations_skip_non_positive_ids, test_groww_presence_registration_site_wired
- [x] Item 4 — 15:45 drain + coverage keep-better + feed_coverage_daily rows
  - Files: crates/app/src/feed_scoreboard_boot.rs
  - Tests: test_apply_presence_coverage_flips_to_registry_truth, test_apply_presence_coverage_respects_partner_feed_off_sentinels, test_build_coverage_detail_rows_feed_labels_and_partial_flag, test_parse_existing_daily_coverage_reads_columns_6_through_11, test_coverage_source_rank_and_fold_existing_coverage_keep_better, test_build_existing_daily_outcome_sql_micros_window
- [x] Item 5 — DHAT + Criterion + budget
  - Files: crates/core/tests/dhat_feed_presence.rs, crates/core/benches/feed_presence.rs, crates/core/Cargo.toml, quality/benchmark-budgets.toml
  - Tests: dhat_record_presence_hot_path_zero_allocation
- [x] Item 6 — dashboard rows 2-4 (existing metrics only)
  - Files: deploy/aws/terraform/dashboard.tf
  - Tests: (tf — pinned by review; zero new EMF series by construction)
- [x] Item 7 — docs (runbook, rule file, metrics_catalog) + plan hygiene
  - Files: docs/runbooks/dual-feed-scoreboard.md, .claude/rules/project/dual-feed-scoreboard-error-codes.md, crates/app/src/metrics_catalog.rs, .claude/plans/archive/2026-07-11-scoreboard-groww-lag.md
  - Tests: (docs)
- [x] Item 8 — review round 1 fixes (4 HIGH + 3 MEDIUM confirmed + LOWs):
  dual-field Groww index pairing (token + display name vs the allowlist —
  the 2026-06-28 token-only lesson); ONE process-global-prefix
  `init_feed_presence` site ordered before the Groww watcher + both
  `load_instruments` (source-order ratchet); one-sided-drain refusal
  (`presence_drain_one_sided_feed` + `stage="presence_one_sided"`);
  Groww fold same-IST-day gate (cross-day re-tail replay); late-first-
  registration `covers_full_session` degrade; mixed-day unique_win/both
  flip gated on `covers_full_session` (SQL minute sets stand); equal-rank
  value-aware coverage keep-better (zero-measured rerun never erases);
  step-8b detail rows skipped when 6d folded existing coverage forward;
  feed-off own-row untouched (doc/code match); Feed::COUNT arrays in
  drain_day; Dhan Future expiry key normalized to zero-padded ISO.
  - Files: crates/core/src/pipeline/feed_presence.rs, crates/core/src/instrument/presence_registration.rs, crates/core/src/auth/secret_manager.rs, crates/app/src/groww_activation.rs, crates/app/src/groww_bridge.rs, crates/app/src/main.rs, crates/app/src/feed_scoreboard_boot.rs
  - Tests: test_groww_index_pairing_dual_field_matches_dhan_canonicals, test_late_first_registration_degrades_covers_full_session, test_pre_open_first_registration_keeps_full_session_stamp, test_tick_is_same_ist_day_gates_cross_day_replay, test_apply_presence_coverage_mixed_day_keeps_sql_unique_win_both, test_presence_drain_one_sided_feed_detection, test_fold_existing_coverage_keep_better_equal_rank_zero_drain, test_future_expiry_key_is_normalized_to_zero_padded_iso, test_feed_presence_is_wired_into_main

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy full day, both feeds on | coverage_source=in_memory; unique/both = registry truth; ~1.5K coverage rows |
| 2 | Mid-day restart | coverage_source=mixed; rows partial; restart footnote (existing) |
| 3 | Post-close deploy + catch-up rerun | keep-better folds registry columns forward (stage=coverage_regression) |
| 4 | Past-day backfill | SQL fallback; measured day's registry columns preserved |
| 5 | Groww off all day | Dhan unique/both = -1 sentinels; feed_off outcome (unchanged) |
| 6 | presence_fold_enabled=false | byte-identical PR-C behavior (sql_backfill) |
| 7 | Slot overflow / unregistered SIDs | counted + SCOREBOARD-01 / drain log — never silent |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage:
per-instrument presence over the registered universe at OUR capture
boundary (never proof the exchange traded); fixed 192 KiB registry;
zero-alloc O(1) hot fold (DHAT `dhat_feed_presence.rs`, Criterion budget
`feed_presence_record`); drain + registration honestly O(slots)/O(universe)
cold; process-local registry — restarts degrade to mixed/partial +
keep-better, never fabricated. Beyond the envelope, the SQL minute sets +
`ws_event_audit`/`ticks` tables remain the durable fallback.
