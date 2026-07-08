# Implementation Plan: FUTIDX-4 nearest-expiry subscription (Dhan Quote + Groww watch set)

**Status:** VERIFIED
**Date:** 2026-07-08
**Approved by:** Parthiban (operator) — dated verbatim quote in daily-universe-scope-expansion §36 (2026-07-08): "for both dhan and groww we need to add futures and those also should be subscribed along with this, especially only for nifty banknifty and sensex nifty midcap."

## Plan Items

- [x] Item 1 — Shared selection module (constant + boundary fn + selector + parity comparator)
  - Files: crates/core/src/instrument/index_futures.rs (NEW), crates/core/src/instrument/mod.rs
  - Tests: test_index_futures_underlyings_pinned_exactly_four, test_select_index_future_expiry_picks_first_at_or_after_today, test_select_index_future_expiry_keeps_expiring_contract_on_t_zero, test_select_index_future_expiry_advances_day_after_expiry, test_select_index_future_expiry_none_when_all_past, test_index_future_selection_never_rolls_unlike_stocks, test_select_index_future_contracts_excludes_optidx_futstk_optstk, test_select_index_future_contracts_excludes_fifth_underlying, test_select_index_future_contracts_fails_closed_on_duplicate_expiry, test_select_index_future_contracts_sensex_from_bse_fno_only, test_select_index_future_contracts_canonicalizes_midcpnifty_alias, test_select_index_future_contracts_missing_underlying_degrades, test_select_index_future_contracts_skips_unparsable_expiry_counted, arbitrary_contract_sets_never_select_beyond_allowlist, test_compare_index_future_selections_ok_on_identical_pairs, test_cross_feed_parity_flags_expiry_mismatch, test_cross_feed_parity_flags_one_sided_underlying
- [x] Item 2 — InstrumentRole::IndexFuture + universe Pass 5 + orchestrator wiring + error codes
  - Files: crates/core/src/instrument/daily_universe.rs, crates/core/src/instrument/daily_universe_orchestrator.rs, crates/common/src/error_code.rs, crates/app/src/daily_universe_boot.rs, crates/app/src/main.rs
  - Tests: test_build_daily_universe_pass5_promotes_futidx_targets, test_count_by_role_includes_index_future, test_daily_universe_spot_only_when_no_future_rows
- [x] Item 3 — Planner IndexFuture arm (segment from csv_row, Quote, IndexDerivative)
  - Files: crates/core/src/instrument/subscription_planner.rs
  - Tests: test_daily_universe_plan_index_future_targets_quote_mode_fno_segments, test_daily_universe_plan_futidx_capped_at_4, test_daily_universe_plan_index_future_unknown_segment_skipped, test_daily_universe_plan_futures_dedup_composite_key
- [x] Item 4 — Snapshot format v2 (role label + optional segment/expiry fields + format gate)
  - Files: crates/core/src/instrument/instrument_snapshot.rs
  - Tests: test_parse_role_round_trips_index_future, test_snapshot_roundtrip_preserves_index_future_segment_and_expiry, test_snapshot_index_future_missing_segment_fails_closed, test_snapshot_format_v1_rejected_forces_cold_build_once, test_snapshot_format_v2_zero_futures_accepted_no_rebuild_loop, test_old_snapshot_without_new_fields_parses_with_empty_defaults
- [x] Item 5 — REST-loop skips + lifecycle dedup + canary hardening
  - Files: crates/app/src/prev_day_ohlcv_boot.rs, crates/app/src/main.rs, crates/app/src/today_instrument.rs, crates/core/src/pipeline/tick_processor.rs
  - Tests: test_instrument_type_for_role_none_for_index_future, test_cross_verify_targets_exclude_index_future_role, test_extract_today_instruments_dedups_futidx_between_targets_and_contracts, test_canary_gauges_ignore_non_idx_i_segments
- [x] Item 6 — Ratchets: scope-guard pins + Quote+FNO parser routing tests
  - Files: crates/storage/tests/daily_universe_scope_guard.rs, crates/core/tests/prev_close_routing_5525125_guard.rs
  - Tests: futidx_scope_pinned_to_4_underlyings_nearest_expiry, futidx_scope_rule_file_pins_forbidden_remainder, futidx_scope_never_roll_source_pin, futidx_scope_legacy_gate_still_false, test_prev_close_routing_nse_fno_from_quote_close_field_bytes_38_to_41, test_prev_close_routing_bse_fno_from_quote_close_field_bytes_38_to_41
- [x] Item 7 — Groww: master-row fields + FUT extractor + watch-set fold + master labeling + parity record
  - Files: crates/core/src/feed/groww/instruments.rs, crates/core/src/feed/groww/shared_master_writer.rs, crates/app/src/groww_activation.rs
  - Tests: test_groww_row_parses_underlying_and_expiry_columns, test_groww_row_missing_new_headers_degrades_not_fails_master, test_extract_index_future_entries_picks_nearest_expiry_four_underlyings, test_extract_index_future_entries_sensex_on_bse_others_nse, test_extract_index_future_entries_uses_underlying_symbol_not_trading_symbol, test_extract_index_future_entries_missing_underlying_degrades, test_extract_index_future_entries_ambiguous_duplicate_fails_closed, test_extract_index_future_entries_never_rolls_on_expiry_day, test_watch_set_includes_exactly_4_fno_futures, test_watch_file_roundtrip_with_fno_entries, test_master_writer_labels_fno_ltp_entries_futidx

## Design

Quote-mode resolution (retired Full ratchet + locked fact #5525125 + §8 lock); new
`InstrumentRole::IndexFuture` promoted into `subscription_targets` at build time (dispatcher
contract literally unchanged — reads `DailyUniverse::subscription_targets` only); ONE shared
pure never-roll expiry function in `crates/core/src/instrument/index_futures.rs`
(`select_index_future_expiry` — nearest = first expiry >= today, NO calendar parameter so
accidental stock-style T-0 roll activation is unrepresentable) used by BOTH the Dhan
orchestrator (`daily_universe_orchestrator.rs`) and the Groww watch builder
(`crates/core/src/feed/groww/instruments.rs`); snapshot format v2 with fail-closed gates
(format gate keyed on FORMAT, never futures count); per-underlying degrade + FUTIDX-01/02
paging. Crates touched: core, common (error_code only), app, storage (tests only).
`should_subscribe_index_derivatives` stays `false` forever; no identifier contains the
`subscribe_index_derivatives` substring outside the allowlisted files.

## Edge Cases

Expiry day T-0 (KEEP expiring contract through close; never roll); T+1 auto-advance; all
expiries past (degrade + FUTIDX-01 page); duplicate same-expiry rows (fail-closed per
underlying); MIDCPNIFTY alias ("NIFTY MIDCAP SELECT" canonicalization via
`INDEX_SYMBOL_ALIASES`; Groww symbol drift → evidence in the FUTIDX-01 payload); SENSEX =
BSE_FNO not NSE_FNO (cross-exchange listing skipped); unparsable SM_EXPIRY_DATE (skip +
`BadExpiryFormat`); FUTIDX SID numerically colliding with a spot SID (composite
`(sid, segment)` keys, I-P1-11); deploy-day old-format same-day snapshot (format gate →
exactly one cold build); rollback day (old binary reads v2 snapshot → unknown role fails
closed → cold build); Groww master missing underlying_symbol/expiry_date columns (futures
degrade; cash/index build untouched); Groww FUT rows ISIN-less (ISIN path deliberately
unused); watch-set cap 1000 with +4; boot spanning IST midnight (both builders re-run for
the new date).

## Failure Modes

FUTIDX-01 selection degraded (per feed, per underlying — subscribe the resolved subset, page
High, never HALT); FUTIDX-02 cross-feed expiry mismatch (page High, both feeds stay live);
snapshot v1 reject (one deterministic cold build, self-heals); Dhan silent no-frames on a
FUTIDX SID (tick-gap detector auto-seeded from plan.registry pages WS-GAP-06 within 30s);
Groww FNO subscribe rejected (existing sidecar reject logging + FEED-STALL chain); code-5 OI
packets counted-and-dropped (documented non-goal, not a failure); REST loops never touch
FUTIDX (skip arms + counters `tv_prev_day_futidx_skipped_total` /
`tv_cross_verify_futidx_skipped_total`).

## Test Plan

All tests named per Plan Item above. Scoped runs: `cargo test -p tickvault-core -p
tickvault-app -p tickvault-storage`; crates/common (`error_code.rs`) touched → workspace-wide
`cargo test --workspace` escalation per testing-scope.md. Selector boundary tests at
T-1 / T-0 / T+1 / all-past + a proptest over arbitrary contract soups; snapshot cross-version
matrix (4 cases); Quote+FNO prev-close routing parser ratchets; scope-guard rule-file pins.
Pre-push battery + Repo Guards + All Green. First-live-session verification checklist per the
design spec (record dated Verified notes for the live Unknowns: BSE_FNO Quote field
population, Groww FNO streaming, MIDCPNIFTY symbol literals).

## Rollback

`git revert` of the code commits restores byte-identical spot-only behavior (no schema
change, no config-default change, no new config flag — deliberate; pinned by
`test_daily_universe_spot_only_when_no_future_rows`). An old binary reading a v2 snapshot
fails closed to a cold build (`test_to_universe_fails_closed_on_unknown_role` stays green).
The §36 rule-file grant remains as a dormant historical authorization (grant != obligation).

## Observability

Counters: `tv_index_futures_selection_missing_total{feed,underlying}`,
`tv_index_futures_parity_mismatch_total`, `tv_prev_day_futidx_skipped_total`,
`tv_cross_verify_futidx_skipped_total`; gauge `tv_index_futures_selected{feed}` (0-4); coded
`error!(code = "FUTIDX-01"/"FUTIDX-02")` → 5-sink chain + Telegram High (runbook
`.claude/rules/project/futidx-4-error-codes.md`); one structured `info!` per feed per boot:
`index-futures selection feed=<f> underlying=<U> expiry=<date> native_id=<id> segment=<seg>`
(the machine-readable evidence table) + the parity OK/mismatch verdict line; existing per-SID
telemetry inherited automatically (tick-gap seeding, registry gauges, candle DEDUP rows,
ws_event_audit).

## Guarantee matrices

See per-wave-guarantee-matrix.md — all 15 + 7 rows apply to every item in this plan.
100% inside the tested envelope, with ratcheted regression coverage: exactly 4 contracts
pinned by `INDEX_FUTURES_UNDERLYINGS` (arity ratcheted in code AND rule text);
nearest-expiry never-roll pinned by boundary tests at T-1 / T-0 / T+1 / all-past + proptest;
Quote mode per the ratcheted §8 lock; both boot paths proven identical by the extended
snapshot plan-identity ratchet; per-underlying degrade is loud (FUTIDX-01, High) and
cross-feed expiry divergence pages FUTIDX-02 — never silently absorbed. NOT claimed: futures
OI capture; prev-day pct coverage for futures; Dhan live Quote cadence for NSE_FNO/BSE_FNO
FUTIDX (UNVERIFIED-LIVE); Groww live FNO subscribe_ltp delivery (UNVERIFIED-LIVE).

## Hostile-review round 1 (2026-07-08) — 13 confirmed findings, all fixed

Scope addendum (Status stays VERIFIED; fixes landed post-plan as review remediation):

- [x] R1-A (HIGH, 4 duplicate reports): Groww parity canonical was re-derived via
      first-match SUBSTRING (`symbol_name.contains(canonical)`) — BANKNIFTY/MIDCPNIFTY
      collapsed into NIFTY → false FUTIDX-02 every dual-feed boot. Fixed: `GrowwIndexFuture`
      carries the EXACT-match canonical + expiry out of `extract_index_future_entries`;
      the recorder consumes it verbatim.
      - Files: crates/core/src/feed/groww/instruments.rs
      - Tests: test_extract_index_future_entries_records_exact_canonicals
- [x] R1-B (MEDIUM): §36 futures appended LAST before the 1000-cap prefix truncate +
      pre-cap gauge. Fixed: futures PREPENDED (cap-priority), gauge set from the POST-cap
      live set, loud `tv_index_futures_cap_dropped_total` + FUTIDX-01 error on any drop.
      - Files: crates/core/src/feed/groww/instruments.rs
      - Tests: test_assemble_cap_pressure_keeps_all_futures_in_live_set
- [x] R1-C (MEDIUM): parity recorder compared across trading dates. Fixed: date-keyed
      store; cross-date pair refused + stale entry evicted.
      - Files: crates/core/src/instrument/index_futures.rs, daily_universe_orchestrator.rs,
        feed/groww/instruments.rs
      - Tests: test_record_selection_cross_date_refuses_compare_and_evicts_stale
- [x] R1-D (LOW): non-numeric Groww token misreported as BadExpiryFormat. Fixed: new
      `IndexFutureMissReason::BadNativeToken`.
      - Tests: test_extract_index_future_entries_bad_token_reason_is_bad_native_token
- [x] R1-E (LOW): Groww FUTIDX-01 lacked the runbook-promised `candidates_seen` evidence.
      Fixed: `collect_fut_underlying_symbols_seen` (shared bound) + payload field.
      - Tests: test_collect_fut_underlying_symbols_seen_bounded_distinct
- [x] R1-F (LOW): false "ONLY test touching the global" comment. Fixed: honest comment.
- [x] R1-G (LOW): prev-day coverage denominator counted the always-skipped futures.
      Fixed: role-filtered `pd_expected` in main.rs (mirror of the 15:31 cross-verify).
- [x] R1-H (MEDIUM): scope-lock forbidden table carve-out on the WRONG row. Fixed:
      carve-out moved Stock-F&O → BSE-F&O row.
- [x] R1-I (LOW): Dhan Step-3d recording block + mismatch arm untested. Fixed:
      builds_universe_with_futidx_rows_selects_index_futures +
      test_record_selection_mismatch_verdict_through_recorder.
- [x] R1-J (LOW): plan-builder doc block displaced onto the private segment helper.
      Fixed: doc reattached to `build_subscription_plan_from_daily_universe`.

## Hostile-review round 2 (2026-07-08) — 7 confirmed findings, all fixed

- [x] R2-1 (MEDIUM): exact-duplicate CSV/master lines (same SECURITY_ID /
      exchange_token) counted as AmbiguousDuplicateExpiry → mandated future
      dropped for the day. Fixed: first-row-wins identity dedup BEFORE the
      ambiguity count on BOTH feeds; truly-distinct ids stay fail-closed.
      - Files: crates/core/src/instrument/index_futures.rs,
        crates/core/src/feed/groww/instruments.rs
      - Tests: test_select_dedups_exact_duplicate_security_id_before_ambiguity,
        test_extract_dedups_exact_duplicate_token_before_ambiguity
- [x] R2-2 (MEDIUM): plan edge-case claim "boot spanning IST midnight → both
      builders re-run for the new date" was not delivered (date frozen at boot
      entry across the §4 retry loop → stale T-0 selection after midnight).
      Fixed: `run_daily_universe_fetch_runner` takes a date CLOSURE re-derived
      per build attempt; boot passes the live IST wall clock.
      - Files: crates/core/src/instrument/instr_fetch_runner.rs,
        crates/app/src/daily_universe_boot.rs
      - Tests: runner_rederives_trading_date_per_build_attempt
- [x] R2-3 (LOW): feed='groww' FUTIDX lifecycle rows had empty
      underlying_symbol. Fixed: canonical threaded via
      WatchEntry.underlying_symbol into the master-row writer;
      underlying_security_id stays 0 (documented — no numeric id exists).
      - Files: crates/core/src/feed/groww/{instruments,shared_master_writer}.rs
      - Tests: test_master_writer_labels_fno_ltp_entries_futidx (extended),
        test_extract_dedups_exact_duplicate_token_before_ambiguity (thread assert)
- [x] R2-4 (LOW): Dhan gauge pre-plan + planner-stage drops only warn!.
      Fixed: gauge moved to the plan builder (POST-PLAN IndexDerivative
      count) + FUTIDX-01 error on any planner-stage future drop.
      - Files: crates/core/src/instrument/subscription_planner.rs,
        daily_universe_orchestrator.rs (pre-plan gauge removed)
      - Tests: test_plan_futidx_planner_drop_is_counted_post_plan
- [x] R2-5 (LOW): Groww cap-drop FUTIDX-01 misattributed an intra-futures
      dedup collapse to a cap override. Fixed: expected = distinct
      (exchange, token, segment) count; the collapse is its own loud
      FUTIDX-01 cause + tv_index_futures_dedup_dropped_total.
      - Tests: test_distinct_future_key_count_folds_duplicate_tokens
- [x] R2-6 (LOW): warm-boot day had no Dhan parity record / gauge / evidence
      until the background reconcile (never, if it failed). Fixed: shared
      `record_dhan_selection_from_universe` called from BOTH orchestrator
      Step 3d (post-build) and the main.rs warm-snapshot path; gauge covered
      by R2-4 (the warm path calls the same plan builder).
      - Files: index_futures.rs (helper), daily_universe_orchestrator.rs,
        crates/app/src/main.rs
      - Tests: builds_universe_with_futidx_rows_selects_index_futures (extended
        with dhan_selections_from_universe derivation asserts)
- [x] R2-7 (LOW): charter §I banner claimed the forbidden row was carved down
      while the row text + stale "compile-time impossible" rationale were
      unchanged. Fixed: row edited inline with the carve-out + corrected
      rationale.
      - Files: .claude/rules/project/operator-charter-forever.md

## Hostile-review round 3 (2026-07-08) — 7 confirmed findings, all fixed

- [x] R3-1 (MEDIUM): unbounded O(n²) same-expiry dedup on untrusted CSV rows,
      both selectors. Fixed: HashSet O(n) dedup (I-P1-11 single-segment
      justification comments) + hard envelope cap
      `FUTIDX_SAME_EXPIRY_CANDIDATE_CAP = 16` with fail-closed
      `SameExpiryCandidateFlood` degrade above it (both feeds).
      - Files: index_futures.rs, feed/groww/instruments.rs
      - Tests: test_select_flood_beyond_cap_degrades_fail_closed,
        test_select_dedup_at_cap_scale_same_sid_still_chosen,
        test_extract_flood_beyond_cap_degrades_fail_closed
- [x] R3-2 (MEDIUM): Groww watch date frozen across the pull-until-success
      loop (Edge Cases claim untrue on the Groww side). Fixed: per-attempt
      today_ist_date() inside the loop (build + watch-file name + persist
      date), entry-time value kept only as the validity probe/fallback —
      the Edge Cases claim "both builders re-run for the new date" is now
      TRUE on both sides (Dhan via R2-2's date closure, Groww via this).
      - Files: crates/app/src/groww_activation.rs
      - Tests: ratchet_watch_date_rederived_per_attempt_inside_loop
- [x] R3-3 (LOW): snapshot to_universe IndexFuture arm silently defaulted
      missing expiry/underlying while segment failed closed → subscribed
      future invisible to the parity recorder (false one-sided FUTIDX-02).
      Fixed: expiry + underlying fail closed (None/empty → whole snapshot
      None → cold rebuild).
      - Files: crates/core/src/instrument/instrument_snapshot.rs
      - Tests: test_snapshot_index_future_missing_expiry_or_underlying_fails_closed
- [x] R3-4 (LOW): FUTIDX-02 shipped auto-triage-safe=true against the design
      contract. Fixed: severity-independent override arm in
      is_auto_triage_safe (false for FUTIDX-02); runbook prose de-drifted.
      - Files: crates/common/src/error_code.rs
      - Tests: test_futidx_codes_contract
- [x] R3-5 (LOW): /feeds subscribed counts excluded the 4 futures while the
      FeedInstrumentsLoaded Telegram included them (off-by-4). Fixed: the
      non-index bucket is entries.len() - indices (mirror of the Dhan call).
      - Files: crates/app/src/groww_activation.rs
- [x] R3-6 (MEDIUM): scope-guard FUTIDX membership pin satisfiable by
      test-region literals (vacuous whole-file scan). Fixed: production-
      region split at #[cfg(test)] + non-vacuity self-assert (prod region
      must exclude "FINNIFTY").
      - Files: crates/storage/tests/daily_universe_scope_guard.rs
- [x] R3-7 (LOW): wave-5 PREVCLOSE-03 matrix kept contradictory unmarked
      NSE_FNO|Full / BSE_FNO|Full rows beside the §36 Quote row. Fixed:
      stale rows marked inline (superseded 2026-07-08), Quote row marked
      authoritative — no future runtime-assertion implementer can honor the
      stale cells.
      - Files: .claude/rules/project/wave-5-error-codes.md

## Hostile-review round 4 (2026-07-08) — 4 confirmed findings, all fixed

- [x] R4-1 (MEDIUM): snapshot v2 fail-closed arm validated only Some+non-empty
      — a non-empty UNPARSABLE expiry ("30-07-2026") or NON-CANONICAL
      underlying ("NIFT") still passed `to_universe`, got SUBSCRIBED by the
      planner but SKIPPED by `dhan_selections_from_universe` → the exact
      false one-sided FUTIDX-02 the R3-3 fix claimed closed. Fixed: the
      expiry must PARSE (%Y-%m-%d) AND the underlying must canonicalize to a
      §36 `INDEX_FUTURES_UNDERLYINGS` entry (the SAME two gates the parity
      derivation applies), else whole-snapshot None → cold rebuild. Secondary
      hole closed in the same arm: exact-duplicate future entries are deduped
      first-entry-wins (previously fired a spurious planner-drop FUTIDX-01
      via the planner's composite-key dedup); two DISTINCT SIDs for one
      canonical underlying fail closed.
      - Files: crates/core/src/instrument/instrument_snapshot.rs
      - Tests: test_snapshot_index_future_unparsable_expiry_or_noncanonical_underlying_fails_closed,
        test_snapshot_index_future_duplicate_entries_deduped_or_fail_closed
- [x] R4-2 (MEDIUM): Groww parity recording + FUTIDX-01/gauge emissions ran
      BEFORE `assemble_watch_set`, which can still fail (NtmDanglingExceeded
      — live precedent 2026-06-08 — or UniverseSizeOutOfBounds). On such a
      day the ≤300s activation pull-until-success retry re-recorded the
      Groww selection per attempt: a genuine cross-feed divergence re-paged
      FUTIDX-02 every retry all day (non-edge-triggered, audit Rule 4), and
      the matching case logged "parity OK" while Groww subscribed NOTHING
      (false-OK, audit Rule 11). Fixed: the §36 block now EXTRACTS only;
      every emission (miss errors + counters, boot-evidence lines, parity
      recording, dedup-collapse error, post-cap gauge/cap-drop) moved AFTER
      `assemble_watch_set` succeeds — mirror of the Dhan Step-3d post-build
      ordering.
      - Files: crates/core/src/feed/groww/instruments.rs
      - Tests: ratchet_groww_futidx_emissions_run_after_assemble_watch_set
- [x] R4-3 (LOW): both plan-snapshot write sites stamped the `today_date`
      frozen at `load_daily_universe_plan` entry while the universe was
      built with the per-attempt re-derived date (R2-2) — a midnight-
      crossing cold build / background reconcile wrote a D-labeled snapshot
      whose IndexFuture targets were selected for D+1 (internally
      inconsistent forensic artifact; the date-keyed loader already
      prevented any wrong SERVE). Fixed: `DailyUniverseBootOutcome` gains
      `build_trading_date_ist` (captured from the LAST per-attempt date
      derivation via `recording_date_fn` — the provenance-capture pattern);
      both the slow-path write and the warm-path background-reconcile write
      stamp it.
      - Files: crates/app/src/daily_universe_boot.rs, crates/app/src/main.rs
      - Tests: test_recording_date_fn_captures_last_derived_date,
        ratchet_main_rs_snapshot_writes_stamp_the_build_date
- [x] R4-4 (LOW): the R3-4 auto-triage override was justified in two
      permanent artifacts by a citation to "FINAL.md D0.5/T3.4" — a
      scratchpad-phase design doc that never landed in the tree (dangling
      provenance; unverifiable under engineering-execution-standard §2).
      Fixed: both citations now point at the in-repo authority
      (futidx-4-error-codes.md §2 + this R3-4 record), with a dated note
      that the FINAL.md pointer was replaced.
      - Files: .claude/rules/project/futidx-4-error-codes.md,
        crates/common/src/error_code.rs
