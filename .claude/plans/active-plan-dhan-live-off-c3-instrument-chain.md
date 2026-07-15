# Implementation Plan: Dhan Live-WS Retirement — PR-C3 (delete the Dhan instrument-download chain + tick-gap detector)

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — 2026-07-13 directive, verbatim Q3: "hereafter no Dhan
instrument download/parsing — just direct hardcoded security IDs passed to spot 1m and option
chain" + Q4-ii "agreed dude" (tick-gap detector + WS-GAP-06 deleted; the Groww feed-stall
watchdog owns stall detection). Authority chain:
`.claude/rules/project/websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B
(the Phase C deletion inventory + the KEEP/REWIRE seam) + the retirement banners already on
`daily-universe-instr-fetch-error-codes.md`, `ntm-constituency-error-codes.md`,
`prev-day-ohlcv-error-codes.md`, `cross-verify-1m-error-codes.md`, and `wave-2-error-codes.md`
(WS-GAP-06). Coordinator confirmed go after C2 (#1522 → main `bce924c9`) landed green.

> **Guarantee matrices:** this plan cross-references the 15-row + 7-row matrices in
> `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per-item matrix). This is a
> DELETION PR — the per-row proof is "no new code path": no new tick-drop path (the Groww
> WAL→ring→spill→DLQ chain is untouched; the order-update WAL residuals become a LOUD
> archive-only leg mirroring the C2 LiveFeed leg — counted, never silent), no new hot-path
> allocation (only cold boot/fetch/audit code deleted; the one hot-path EDIT removes the
> tick-gap `record_tick_global` call from `tick_processor.rs` — a strict allocation/work
> REDUCTION), DEDUP keys untouched, composite `(security_id, exchange_segment)` uniqueness
> untouched, SEBI tables (`instrument_lifecycle` / `index_constituency` /
> `instrument_fetch_audit`) and their storage persistence modules KEPT (un-gated), and every
> surviving invariant stays pinned by its ratchet (adjusted with dated tombstones, never
> silently). Crates touched: **tickvault-core**, **tickvault-app**, **tickvault-storage**,
> **tickvault-common** (feature removal + subscription/LOCKED_UNIVERSE surface), **tickvault-api**
> (comment truth-sync only if needed); terraform under `deploy/aws/terraform/`; the two
> approved workflow riders under `.github/workflows/`.

## Design

Delete the Dhan instrument-download chain + the tick-gap detector per the scope-lock §B
inventory, now that C1 relocated `parse_intraday_1m_candles`/`MinuteCandle` into
`dhan_intraday_parse.rs` (verified: `spot_1m_rest_boot` + `groww_spot_1m_boot` import from
`dhan_intraday_parse`, NOT `cross_verify_1m_boot`) and C2 deleted the lane runtime (both
dormant boot modules below have ZERO spawn sites — verified by grep).

1. **`daily_universe_fetcher` feature DELETED** from all four Cargo.tomls (app default,
   core→storage forward, storage decl, core's gated example/harness entries). The KEEP
   modules it gated are UN-GATED (compiled unconditionally): storage's
   `instrument_lifecycle_persistence` / `index_constituency_persistence` /
   `instrument_fetch_audit_persistence` / `lifecycle_reconciler` (SEBI never-delete tables +
   the pure `classify_transition` the Groww writer consumes) and core's
   `shared_master_writer` internals (the Groww feed='groww' master writer — live prod path).
   Touching crates/common escalates local tests to `cargo test --workspace`.
2. **core/src/instrument/ chain DELETED:** `csv_downloader.rs`, `csv_parser.rs` (with
   `CsvRow` + the field surface `index_extractor`/`index_futures` need SPLIT OUT first into a
   new surviving `csv_row.rs` — the C1 mod.rs note's mandated split),
   `fno_underlying_extractor.rs`, `daily_universe.rs`, `daily_universe_orchestrator.rs`,
   `constituent_resolver.rs`, `index_constituency/` (module; the
   `INDEX_CONSTITUENCY_BASE_URL`/`INDEX_CONSTITUENCY_SLUGS` constants in
   `crates/common/src/constants.rs` are KEPT — the Groww watch build consumes the
   niftyindices CSV via its own client), `instr_fetch_loop.rs`, `instr_fetch_retry_policy.rs`,
   `instr_fetch_retry_adapter.rs`, `instr_fetch_runner.rs`, `instrument_snapshot.rs`
   (with `is_valid_trading_date` RELOCATED to a new surviving ungated
   `trading_date.rs` — live consumers: `groww_activation.rs` ×2, plus doc references),
   `boot_time_of_day_guard.rs`, `boot_day_classifier.rs`, `boot_day_classifier_extended.rs`,
   `l3_anomaly_check.rs`, `nse_holiday_cross_check.rs`, `boot_complete_by_guard.rs`,
   `presence_registration.rs` (with `ist_day_from_date` RELOCATED into the same
   `trading_date.rs` — live consumer: `groww_activation.rs`; the Dhan slot-build halves
   `dhan_presence_registrations`/`register_dhan_presence_from_universe` die with
   `DailyUniverse`), `subscription_planner.rs`, `subscription_distribution.rs`.
   KEEP verified live: `index_extractor.rs` (NSE_INDEX_ALLOWLIST + canonicalize_index_symbol
   — Groww consumes), `index_futures.rs` (de-gated in C1; its one comparative test importing
   the deleted stock-roll selector is retired with a dated note), `market_open_self_test.rs`
   + `slo_score.rs` (dormant contract stubs per the C2 precedent — doc-comment truth-sync
   only), `boot_ordering_gate` etc. untouched.
3. **app/src/ chain DELETED:** `daily_universe_boot.rs`, `today_instrument.rs`,
   `lifecycle_reconcile_orchestrator.rs`, `lifecycle_reconcile_plan.rs`,
   `lifecycle_apply.rs`, `apply_reconcile_plan.rs`, `lifecycle_cache_loader.rs`,
   `instr_fetch_audit_writer.rs`, `prev_day_ohlcv_boot.rs`, `cross_verify_1m_boot.rs`
   (dormant since C2; the sweep-timing const
   `CROSS_VERIFY_TRIGGER_SECS_OF_DAY_IST` consumed by `spot_1m_rest_boot` is RELOCATED into
   `spot_1m_rest_boot.rs` with a dated note — the 15:33:30 sweep timing itself is unchanged).
4. **`index_constituency_boot.rs` SPLIT:** the process-global ts-pin migration half
   (`run_index_constituency_ts_pin_migration_at_boot` + its readiness constants + the F13/F14
   ratchets) is KEPT and UN-GATED (load-bearing for the Groww append MigrationGate); the
   lane-mapping half (`persist_index_constituency_mapping`, `build_index_constituency_rows`,
   `OwnedConstituencyRow` + their tests) is DELETED (its inputs — ConstituencyDownloader,
   CsvDownloader, resolve_constituents — all die in item 2).
5. **`InstrumentRegistry` KEPT** (verified LIVE consumers: `tick_processor.rs`,
   `trading/candles/aggregator_cell.rs`, `multi_tf_aggregator.rs`, `prev_day_cache.rs`) —
   reported, not forced.
6. **Tick-gap detector DELETED (Q4-ii):** `core/src/pipeline/tick_gap_detector.rs`, the
   `record_tick_global` call in `tick_processor.rs`, the main.rs global-install + 60s
   summary task (the WS-GAP-06 emit site) + the 15:35 daily-reset task, the far-month
   alarm-gate exclusion plumbing (the tick-gap consumer site of the nearest/far split),
   the `TickGapsSummary` NotificationEvent variant + its coalescer/service arms, the
   `TICK_GAP_RESET_*` constants (orphaned), the `tick_gap_detector_60s_coalesce` config
   toggle + base.toml key, benches (`tick_gap_detector.rs` + the `full_tick_processing`
   arm) + their `benchmark-budgets.toml` keys, dhat/loom tests, and the observability.rs
   LiveTicks target-list entry. KEPT: `trading/src/risk/tick_gap_tracker.rs` (RISK-GAP-03 —
   a different subsystem) + the `TICK_GAP_ALERT/ERROR_THRESHOLD` constants it consumes.
   Terraform: `aws_cloudwatch_metric_alarm.tick_gap_instruments_silent` (app-alarms.tf) +
   its membership in BOTH armed lists (app-alarms.tf window-gate + the
   market-hours-liveness-alarm.tf list) + the EMF allowlist rows
   (user-data.sh.tftpl + cloudwatch-agent.json) retired with dated notes; alarm-count/cost
   notes corrected in place.
7. **CROSS-VERIFY-1M paging entries retired:** the 2026-07-14 automation-gaps PR-3 added
   `cross-verify-1m-01`/`-02` filters+alarms for the module this PR deletes — those two
   `error_code_alerts` map entries are retired in the SAME PR (dated note), keeping the
   paging-filter drift guard green (no dead filters). `tick-conserve-01` STAYS
   (tick_conservation_boot survives). ErrorCode variants (InstrFetch01..04,
   NtmConstituency01, PrevDay01, CrossVerify1m01/02, WsGap06...) are RETAINED for the C4
   sweep — the Phase B rule-file banners keep the crossref tests green both ways.
8. **STAGE-C.2b order-update WAL leg (C2 leftover, fixed honestly):** post the 2026-07-14
   noise lock (`dhan-rest-only-noise-lock-2026-07-14.md` §A.1) NOTHING spawns the
   order-update WS or subscribes its broadcast — the boot drain was re-broadcasting staged
   order-update WAL frames into a permanently receiver-less channel and then confirming.
   Fix: the leg becomes a LOUD archive-only arm mirroring the C2 LiveFeed leg — counted
   (`tv_ws_frame_wal_reinjected_dropped_total{ws_type="order_update"}`) + warn!-logged with
   a dated note; `confirm_replayed` still MOVES segments to the WAL archive (never deletes),
   so no silent loss and no re-stage growth. `drain_replayed_order_updates_to_broadcast` +
   the now-consumer-less `order_update_sender` broadcast in `build_shared_infra` are
   deleted (dated notes; the live-trading re-wire recreates the consumer chain).
9. **Riders (operator-approved, separate commits):** (a) `postmerge-catchup.yml` backfills
   `terraform-apply.yml` (same head_sha-filtered existence check) + a dated §3.1 edit to
   `merge-gate-lock-2026-07-04.md`; (b) `deploy-aws-after-close.yml` gains an 08:15–09:15
   IST prep-window suppression with a dated comment.
10. **Guard/test retirement:** every guard pinning deleted code is retired or trimmed with a
   dated tombstone naming this PR + the 2026-07-13 authority — never silently. Notably
   `daily_universe_scope_guard.rs` is TRIMMED, not deleted (the `futidx_scope_*` pins and the
   surviving storage-persistence pins stay); `wal_replay_confirm_symmetry_guard.rs` is
   re-shaped to the archive-only order-update leg; `far_month_alarm_gate_wiring_guard.rs`,
   `tick_gap_reset_wiring_guard.rs`, `daily_universe_boot_wiring_guard.rs`,
   `cross_verify_1m_visibility_guard.rs`, `indices4only_scope_lock_guard.rs`,
   `fno_extraction_verification.rs`, `ntm_subscription_contract_guard.rs`, the
   `ntm_subscription_proof` example + `warm_resubscribe_latency` example are deleted with
   the chain; `feature_flag_rollback_guard.rs` + `i_p1_11_codepath_grep_guard.rs` +
   `claude_session_bootstrap_guard.rs` + sibling wiring guards get needle adjustments.
11. **Subscription surface (scope-lock §B item 2, verified consumer-free at runtime):**
   `SubscriptionScope` + `effective_main_feed_pool_size` + `SubscriptionConfig` +
   base.toml `[subscription]` + `locked_universe.rs` (`LOCKED_UNIVERSE`) +
   `FnoUniverse::locked_4_idx_i` die together — the only non-test consumers are each other
   (verified: main.rs has zero `config.subscription` reads post-C2). If a hidden live
   consumer surfaces during the build, that item is KEPT and reported instead of forced.

## Edge Cases

- **Groww seam must not move:** `index_extractor` (allowlist + canonicalizer),
  `index_futures` (§36.7 selector, de-gated), `shared_master_writer`, `feed_presence`,
  `feed_scoreboard`, `groww_activation`'s presence build + `ist_day_from_date` +
  `is_valid_trading_date` consumers, the ts-pin MigrationGate ordering (its F14 main.rs
  source-order ratchet must stay green after the split), and the storage SEBI tables all
  compile and test identically after the feature removal. Both former feature modes collapse
  to ONE mode — the un-gated build must equal today's default (feature-ON) build.
- **CsvRow split:** `index_extractor`/`index_futures` consume `CsvRow` (struct + test
  constructors) — the split module preserves the exact field surface; no parse logic moves.
- **The order-update WAL archive-only leg** must keep: exactly ONE `confirm_replayed` call,
  loud counting BEFORE confirm, and the "MOVES, never deletes" semantics (the symmetry guard
  is re-shaped to pin the new arm).
- **Paging-filter drift guard:** deleting `cross_verify_1m_boot.rs` without retiring its two
  2026-07-14 tf map entries would fail the guard as dead filters — both changes land in the
  same commit.
- **`spot_1m_rest_boot` sweep const-asserts** reference the cross-verify trigger constant —
  relocated const, assertions unchanged (the 15:33:30 sweep still clears the 15:31 window;
  the window's producer is gone but the box-stop upper bound still binds).
- **Ratchet baselines:** test-count + pub-fn baselines DECREASE (deletions). The
  `.claude/hooks/.test-count-baseline` is gitignored — local-only re-baseline per the C2
  Edge-Cases truth-sync; CI unaffected (merge-gate-lock §3 row 6).
- **Sandbox test envelope:** the 2 known credential-less failures
  (`test_fetch_api_bearer_token_errors_without_real_ssm`,
  `test_verify_public_ip_fails_without_real_ssm`) are pre-existing and pass in CI.

## Failure Modes

- **Silent loss of a live invariant:** prevented by adjusting (never deleting) every failing
  guard with a dated tombstone, plus the full workspace test sweep (common is touched →
  workspace scope).
- **Broken Groww futures mandate:** the §36.7 selector losing compilation with the feature
  removal would silently drop the Groww futures — prevented by the existing
  `test_futidx_selector_is_not_feature_gated` ratchet + the futidx pins in
  `daily_universe_scope_guard.rs` (kept) + compiling/testing core after the removal.
- **Dead paging filter:** the CROSS-VERIFY-1M tf entries are retired in the same commit as
  the module; the drift guard fails the build otherwise.
- **MigrationGate regression:** the ts-pin migration keeps its main.rs boot-prefix spawn +
  the F13/F14 ratchets; the Groww append gate wait is untouched.
- **Order-update WAL silent loss:** the archive-only arm counts + logs every residual frame;
  `confirm_replayed` archives (never deletes); a future live-trading re-wire replays from
  the archive if ever needed.
- **Hidden live consumer of a deletion target:** the rule is KEEP + report, never force a
  deletion through a broken build (already exercised: `InstrumentRegistry` is kept).

## Test Plan

- `cargo fmt --check`; `cargo clippy` for touched crates (`-D warnings -W clippy::perf`)
- `cargo test --workspace` (common is touched — escalated scope per testing-scope.md);
  expect ONLY the 2 known sandbox `_without_real_ssm` failures
- Hooks: `bash .claude/hooks/banned-pattern-scanner.sh`, `bash .claude/hooks/plan-verify.sh`
- Guard reshape verification: the trimmed `daily_universe_scope_guard` futidx pins,
  the re-shaped `wal_replay_confirm_symmetry_guard`, the adjusted wiring guards all green
- terraform: `grep`-level shape checks via the existing
  `cloudwatch_app_alarms_wiring.rs` / drift-guard tests (no live tf apply from the sandbox)

## Rollback

Single-branch staged commits — `git revert` of the PR's squash-merge commit restores the
chain byte-identically (no schema/data migration; QuestDB tables untouched — the SEBI
tables persist regardless). Until merge, the branch can be abandoned with zero prod impact:
prod runs `dhan_enabled=false` since Phase A, and every deleted module was dormant (zero
spawn sites) or Dhan-lane-dead since C2.

## Observability

- The CROSS-VERIFY-1M paging retirement + the tick-gap alarm retirement are LOUD: dated
  notes in `error-code-alarms.tf` / `app-alarms.tf` / both armed lists / the EMF allowlists,
  alarm-count/cost notes corrected in place.
- The order-update WAL residual arm gains its own counter series
  (`tv_ws_frame_wal_reinjected_dropped_total{ws_type="order_update"}`) + a dated warn! —
  never a silent drop (Rule 11).
- No new ErrorCode; every deleted emit site's variant is retained for the C4 sweep with the
  rule-file banners keeping both crossref directions green.
- WS-GAP-06 / INSTR-FETCH-* / NTM-CONSTITUENCY-01 / PREVDAY-01 / CROSS-VERIFY-1M-01/-02 lose
  their last emit sites by design (the Phase B retirement banners are the operator-facing
  record); FEED-STALL-01 (Groww) owns stall detection per Q4-ii.
- The 15:45 scoreboard, feed_presence, and the Groww master audit chain (GROWW-MASTER-01)
  are untouched.

## Plan Items

- [x] Create this plan (Files: .claude/plans/active-plan-dhan-live-off-c3-instrument-chain.md)
- [x] Delete app dormant Dhan REST-history modules + retire their paging entries + relocate the sweep const (Files: crates/app/src/{cross_verify_1m_boot.rs,prev_day_ohlcv_boot.rs,spot_1m_rest_boot.rs,lib.rs}, crates/api/src/handlers/debug.rs, deploy/aws/terraform/error-code-alarms.tf, .claude/rules/project/cross-verify-1m-error-codes.md, .claude/rules/project/observability-architecture.md; Tests: cross_verify_1m_visibility_guard retired, error_code_paging_filter_drift_guard green, spot_1m_rest sweep const-asserts green)
- [x] Delete the Dhan instrument chain + feature flag + relocations (Files: crates/{core,app,storage,common} per Design items 1–5 + 11, config/base.toml, crates/core/src/instrument/csv_row.rs NEW (is_valid_trading_date stayed IN PLACE in a trimmed instrument_snapshot.rs — import path unchanged for groww_activation, so no trading_date.rs split was needed); Tests: daily_universe_scope_guard trimmed, futidx pins green, MigrationGate ratchets green, workspace suite)
- [x] Delete the tick-gap detector + WS-GAP-06 + far-month gate plumbing + terraform (Files: crates/core/src/pipeline/{tick_gap_detector.rs deleted,tick_processor.rs,mod.rs}, crates/app/src/{main.rs,observability.rs}, crates/core/src/notification/{events.rs,coalescer.rs,service.rs}, crates/common/src/{constants.rs,config.rs}, config/base.toml, quality/benchmark-budgets.toml, deploy/aws/terraform/{app-alarms.tf,market-hours-liveness-alarm.tf,user-data.sh.tftpl}, deploy/aws/cloudwatch-agent.json; Tests: far_month_alarm_gate_wiring_guard + tick_gap_reset_wiring_guard retired with tombstones, cloudwatch wiring guards adjusted, feature_flag_rollback_guard adjusted)
- [x] Fix the STAGE-C.2b order-update WAL leg (archive-only, loud) (Files: crates/app/src/{main.rs,boot_helpers.rs}; Tests: wal_replay_confirm_symmetry_guard re-shaped)
- [x] Rider (a): postmerge-catchup backfills terraform-apply.yml (Files: .github/workflows/postmerge-catchup.yml, .claude/rules/project/merge-gate-lock-2026-07-04.md §3.1 dated edit)
- [x] Rider (b): deploy dispatcher 08:15–09:15 IST prep-window suppression (Files: .github/workflows/deploy-aws-after-close.yml)
- [x] Verify + ship: fmt/clippy/workspace tests/hooks, hostile review rounds, DRAFT PR driven to All Green (stays DRAFT per the coordinator amendment)
