# Implementation Plan: Dhan chain bounded retry + decision ceiling + per-underlying not-served pager + coverage hardening

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — coordinator-relayed directive 2026-07-14

> Per-item guarantee matrices: this plan cross-references the 15-row + 7-row
> guarantee matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (operator-charter-forever.md §C) — every item below inherits both matrices;
> per-item deltas are named in Design/Observability.

## Plan Items

- [x] Item 1 — Bounded same-key retry + 15s decision ceiling on the chain fire (concurrency KEPT per the Dhan doc)
  - Files: crates/app/src/option_chain_1m_boot.rs, crates/common/src/constants.rs
  - Tests: test_chain_retry_allowed_below_ceiling, test_chain_retry_allowed_refused_at_and_past_ceiling, test_chain_retry_allowed_refused_negative_elapsed, test_chain_retry_allowed_final_fire_retains_full_rights, test_retry_gate_includes_min_gap_wait, test_retry_only_for_empty_or_failed_never_entitlement_or_found, test_retry_found_replaces_first_pass_outcome, test_retry_entitlement_outcome_becomes_entitlement_stop, test_at_most_one_retry_per_underlying_per_minute, test_chain_decision_ceiling_constants_pinned

- [x] Item 2 — Dhan UnderlyingServedTracker + typed HIGH page (mirror #1537) + noise-lock §2/§2.1/§4 edit
  - Files: crates/app/src/option_chain_1m_boot.rs, crates/common/src/constants.rs, crates/core/src/notification/events.rs, .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md
  - Tests: test_underlying_tracker_pages_at_threshold_with_sibling_ok, test_underlying_tracker_global_outage_minute_holds, test_underlying_tracker_recovery_resets_and_pings_once, test_underlying_tracker_no_double_page_while_latched, test_underlying_tracker_missing_verdict_is_hold, test_underlying_tracker_disjoint_from_escalation_edge, test_chain_1m_underlying_not_served_is_high_names_dhan_and_underlying, test_chain_1m_underlying_served_recovered_is_info

- [x] Item 3 — strikes/legs histograms + log-only ladder-shrink watermark
  - Files: crates/app/src/option_chain_1m_boot.rs
  - Tests: test_parse_option_chain_counts_kept_strikes, test_parse_strike_count_excludes_invalid_and_truncated, test_ladder_watermark_ratchets_up, test_ladder_shrunk_below_half_day_max, test_ladder_day_start_small_never_flags, test_ladder_shrunk_latch_once_per_episode_clears_on_recovery

- [x] Item 4 — loud trading-day-flip exits (3 sites, both Dhan legs)
  - Files: crates/app/src/option_chain_1m_boot.rs, crates/app/src/spot_1m_rest_boot.rs
  - Tests: ratchet_trading_day_flip_exits_are_coded (wiring guard)

- [x] Item 5 — rule files: new cross-source-chain-coverage-2026-07-14.md + rest-1m §2c end pointer paragraph
  - Files: .claude/rules/project/cross-source-chain-coverage-2026-07-14.md, .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: n/a (docs; cross-ref test unaffected — no new ErrorCode)

- [x] Item 6 — wiring-guard ratchets
  - Files: crates/app/tests/option_chain_1m_wiring_guard.rs
  - Tests: ratchet_chain_fire_keeps_joinset_concurrency, ratchet_chain_retry_pass_is_ceiling_gated, ratchet_not_served_counter_labels_static_underlying_symbols, ratchet_trading_day_flip_exits_are_coded, ratchet_trading_day_flip_exits_return_immediately

## Design

The chain fire keeps its Dhan-DOCUMENTED JoinSet concurrency (distinct underlyings concurrently;
the 3s bound is per unique (underlying, expiry) key, already enforced by min_gap_wait_ms through
the per-underlying last_request_ms stamps). The fire restructures into collect → retry → process:
first-pass Empty/transport-Failed outcomes get ≤1 retry each, gap-gated (same-key ≥3s via the
existing min_gap machinery) and launch-gated by CHAIN_1M_DECISION_CEILING_SECS=15 (const-asserted:
2.5s fallback + 10s request timeout = 12.5s ≤ 15s; ceiling + 20s budget = 35s < 60s); refused
retries counted (tv_chain1m_retry_total{outcome="skipped_ceiling"}), never silent. A
ChainServingHealth bundle (UnderlyingServedTracker port of PR #1537 + LadderWatermark array)
threads as ONE new fire param; served verdicts collected at a single pre-match line seeing the
FINAL post-retry outcome; the 120-line outcome match stays byte-identical (the #1540-recognizable
region). Trading-day-flip exits upgrade info!→coded error!+counter on both legs. No new ErrorCode;
no limiter change; no RAM publication (#1540); no schema change. Touched crates: tickvault-app,
tickvault-common, tickvault-core.

## Edge Cases

No-token minute = tracker HOLD (JoinSet never built, empty verdicts); EntitlementStop skips the
sink (CHAIN-01 owns the day) and a RETRY returning Entitlement is honored identically; global-
failure minutes HOLD inside the tracker (any_served=false); join-failure (unwind-only) yields a
per-underlying HOLD (missing verdict); persist failures never retried and never flip served=false
(the M1 escalation gate keeps them); all-3-timeout storm → 1 retry launches at 12.5s, the rest
ceiling-refused + counted; clock step-back / stale wake unchanged (fire_is_fresh +
stale_wake_backoff_ms untouched); the FINAL fire — the 15:29 candle fires at the 15:30:00
boundary — retains full retry rights because the decision ceiling is fire-relative (elapsed since
the minute close, never time-of-day-gated), verified by the fire-relative ceiling tests incl.
test_chain_retry_allowed_final_fire_retains_full_rights; supervisor respawn resets tracker/watermark streaks (documented
~10-min re-detection envelope, the #1537/FailureEdge precedent); shrink-to-zero is Empty ⇒ the
paging edges, never the shrink detector (Found-only input); VIX cannot enter (existing const-assert).

## Failure Modes

Retry storms impossible (≤1/underlying/minute, ceiling-gated, counted skips); pager storms
impossible (edge-latched once per underlying per episode, re-armed only by own recovery);
double-paging impossible for the fetch class (tracker needs any_served; escalation needs ok==0 —
disjoint; the persist-failed+empty-sibling overlap is two distinct wanted signals); a 429 storm
under concurrent-distinct is absorbed by the shared limiter's RpsTuner step-down (existing) and
surfaces as ceiling-refused retries + the existing minute accounting; a wedged retry is bounded
by the unchanged 20s budget and the existing missed-boundary accounting; noise-lock §3 satisfied
by the same-PR §2/§2.1 edit with the dated verbatim-intent-labeled directive.

## Test Plan

cargo test -p tickvault-app (fire/tracker/ladder/retry unit tests at the END of the tests module
— dodges #1540's top-of-module insert; wiring-guard ratchets); cargo test -p tickvault-core
(events variant bodies + severity); constants.rs is crates/common ⇒ per testing-scope.md the
escalation rule applies: cargo test --workspace before push; const-asserts P1-P3 + the threshold
pin are compile-time proofs.

## Rollback

Single-PR revert restores the no-retry fire + info! exits; commits are feature-separable (plan/
rules/constants/events/retry/tracker/ladder/flip/ratchets) for partial revert; no schema change,
no config-default change, no new ErrorCode, no persisted state; events variants are additive
(unreferenced after reverting the emit sites); rule files are documentation.

## Observability

New counters: tv_chain1m_retry_total{outcome=recovered|still_failed|skipped_ceiling},
tv_chain1m_underlying_not_served_total{underlying}, tv_chain1m_ladder_shrunk_total{underlying},
tv_chain1m_trading_day_flip_exit_total, tv_spot1m_trading_day_flip_exit_total; new histograms:
tv_chain1m_strikes_per_chain, tv_chain1m_legs_per_chain; new stages: CHAIN-02
underlying_not_served (error, edge) / ladder_shrunk (warn, edge-latched) / trading_day_flip_exit
(error, one-shot) + SPOT1M-01 trading_day_flip_exit; typed Telegram: Chain1mUnderlyingNotServed
(High) / Chain1mUnderlyingServedRecovered (Info), Dhan-named bodies with the honest-MAY Groww
cross-pointer. Delivery boundary honest: CHAIN-02/SPOT1M-01 stay log-sink-only (rest-1m §3);
the typed events ARE the pages; no CloudWatch filter added (paging drift guard sees no drift).
