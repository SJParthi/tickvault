# Implementation Plan: Dhan order-surface build (umbrella — clusters A–E)

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) via coordinator directive 2026-07-14 (Dhan-order build parallelization; umbrella plan replaces per-PR plan files to respect the V7 active-plan cap)
**Crates:** tickvault-common, tickvault-core, tickvault-trading, tickvault-storage, tickvault-api, tickvault-app

> **Guarantee matrices:** this plan carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (the canonical copy) — all 15 + 7 rows apply to every item in this plan,
> instantiated below in Design / Test Plan / Observability.
> Honest 100% claim (charter §F wording): 100% inside the tested envelope,
> with ratcheted regression coverage — a dry-run-only order runtime whose
> every fill/halt/reconcile path is ratchet-tested, zero new
> ErrorCode/NotificationEvent variants in cluster A, and a 4-lock OFF
> switch (hardcoded `dry_run: true`, no live-mode code path). NOT
> claimed: any live Dhan order placement (no code path reaches a live
> Dhan POST — the operator's explicit enable + the mandatory pre-live
> follow-ups gate that), sub-second fill confirmation (MPP market→LIMIT
> may sit PENDING — see the order-placement research 2026-07-14), or
> broker-side reconcile truth in dry-run (the heartbeat says "broker
> reconcile SKIPPED", never "reconciled ✅" — Rule-11 no-false-OK).

> **Umbrella convention (coordinator directive 2026-07-14):** EVERY
> Dhan-order PR (clusters A–E) references THIS plan file in its PR body
> and commit trail. Sessions MUST NOT add per-PR
> `.claude/plans/active-plan-*.md` files for Dhan-order work — the V7
> design-first cap (max 5 active plans, `plan-gate.sh`) is fleet-shared.
> Seam ownership + collision contracts are in the table at the end and
> mirrored in team memory (`dhan-order-build-seam-contracts`).

## Design

The Dhan order surface is revived across six crates — tickvault-common
(config + constants), tickvault-core (notification events, calendar,
auth token handles), tickvault-trading (`crates/trading/src/oms/`,
`crates/trading/src/risk/engine.rs`), tickvault-storage
(`crates/storage/src/` order-audit-family persistence), tickvault-api
(read-only status surfaces only; no mutating order endpoints), and
tickvault-app (`crates/app/src/dhan_rest_stack.rs`,
`crates/app/src/order_runtime.rs`, `crates/app/src/main.rs`,
`crates/app/src/groww_bridge.rs`, `crates/app/src/trading_pipeline.rs`)
— in five clusters. Baseline facts are the hostile-verified 2026-07-14
readiness audit (team memory `order-side-readiness-audit-2026-07-14`):
zero OMS instances on today's dhan-off boot, fills never reach
positions/P&L, `reconcile()` never scheduled, all 5 order-side audit
tables deleted 2026-05-20, both date-based live-order gates expired
2026-07-01, and the orphan-position watchdog dead on dhan-off boots.

### Cluster A — OMS revival + fills→P&L bridge (session: this one; full detail in the judge-approved final design)

One supervised single-owner tokio task (NEW
`crates/app/src/order_runtime.rs`, ~600 prod LoC) owning
`OrderManagementSystem` (dry_run hard-true) + `RiskEngine`, spawned
ONLY from `crates/app/src/dhan_rest_stack.rs` Phase 5a *(shipped as
**Phase 5b** — 2026-07-17 truth-sync, post-#1562 audit: Phase 5a is the
RETIRED order-update WS spawn slot per the noise lock, so the runtime
spawn landed as 5b)* (the dhan-OFF
arm) replacing the discard drain — structural dual-OMS exclusion: the
rest stack is the dhan-off arm; `trading_pipeline` (which owns its own
OMS+Risk) is dhan-ON only. Config-gated `[order_runtime]` (serde
default OFF = byte-identical boot; `config/base.toml` opts in). The 12
normative resolutions from the judge synthesis:

1. **Module + spawn ordering law** (dhan_rest_stack Phase 5a) —
   *⚠ PARTIALLY CUT 2026-07-14 (the operator Dhan noise lock,
   `dhan-rest-only-noise-lock-2026-07-14.md` §3 + scope-lock §A.1 —
   annotated 2026-07-17, post-#1562 audit): the WAL drain, the
   conditional `confirm_replayed`, and the
   `run_order_update_connection(.., wal_spill: Some(..))` socket spawn
   below were REMOVED from the shipped scope; re-arming any of them
   without a fresh dated operator quote in the noise-lock file first is
   FORBIDDEN. What shipped (as Phase 5b): `broadcast::channel` →
   `subscribe()` → `spawn_order_runtime(..)` with ZERO producers.*
   Original (retained for the gated live re-arm spec —
   `order-runtime-dryrun.md` §2): `broadcast::channel` →
   `order_update_sender.subscribe()` → `spawn_order_runtime(..)` →
   `drain_replayed_order_updates_to_broadcast(..)` → conditional
   `confirm_replayed` → `run_order_update_connection(.., wal_spill:
   Some(..))`. NOT spawned from `crates/app/src/main.rs`, NOT on the
   fast arm.
2. **Fill delta = return-widening**: `handle_order_update` in
   `crates/trading/src/oms/engine.rs` returns
   `Result<Option<FillEvent>, OmsError>`; delta computed in-engine from
   old vs new `traded_qty` (double-count-safe; unknown orders →
   `Ok(None)` + loud orphan warn).
3. **Ticks→`update_market_price` gate**: `Arc<AtomicBool>
   marks_wanted` + bounded `try_send` of a 16-byte Copy `MarkUpdate`
   at the `crates/app/src/groww_bridge.rs` consume seam →
   `mpsc::channel(8192)` → runtime. Empty-book fast path = ONE Relaxed
   load; zero alloc, zero lock, no await (DHAT + Criterion evidence).
4. **Paper filler**: next-mark fill of pending `PAPER-n` orders,
   fill-once (terminal never re-fills), finite mark > 0 required (else
   deferred + counted), `PAPER-` prefix assertion; plus a once-daily
   gated end-to-end self-test (calendar + [09:20, 15:00) IST +
   once-per-day latch + `is_dry_run` assertion).
5. **WAL drain + conditional confirm**: order-update frames drained
   FIFO to the stack broadcast BEFORE the WS spawns; `confirm_replayed`
   fires iff drain parse-clean AND `livefeed_frames_replayed == 0`;
   else ONE coalesced `warn!(code = WS-REINJECT-01,
   reason="confirm_deferred_stale_livefeed")` + counter (warn-level
   deliberately — non-paging; segments stay staged, zero silent loss).
6. **Reconcile scheduler**: 300s market-hours-gated + boot+60s + on
   `ou_reconnect_notifier`. Dry-run = HEARTBEAT wording ("broker
   reconcile SKIPPED") + a REAL local invariant check (Σ FillEvent
   mirror == `risk.net_lots_for` per sid) — divergence →
   `error!(code=OMS-GAP-02)`.
7. **Alert sinks**: `NotifierAlertSink` wires OMS + Risk
   `set_alert_sink` (first-ever production callers) to the 5 EXISTING
   NotificationEvents (OrderRejected, CircuitBreakerOpened/Closed,
   RateLimitExhausted, RiskHalt). ZERO new NotificationEvent variants,
   ZERO new ErrorCode variants — every emit mapped to existing
   OMS-GAP-01/02/03/04/06, RISK-GAP-01/02, WS-REINJECT-01.
8. **I-P1-11 tripwire**: first-seen `segment_code` per sid; a
   mark/fill arriving with a different segment →
   `error!(code=RISK-GAP-02, reason="sid_segment_collision")` + skip.
   The full composite-key `(u64, u8)` rewrite is DEFERRED — a
   MANDATORY pre-live follow-up (see OUT OF SCOPE).
9. **Risk P&L lot_size fix**: `crates/trading/src/risk/engine.rs:295`
   unrealized P&L omits lot_size (25–75× understated on options — a
   false-guarantee, Rule-11 class). Fix: `PositionInfo` gains
   `lot_size: u32`; unrealized multiplies by
   `f64::from(lot_size.max(1))`; NEW
   `evaluate_daily_loss_halt()` extracted so mark-to-market drawdown
   halts between signals; `trigger_halt` `error!` gains its missing
   `code =` field.
10. **Ratchets**: the dormant-shape ratchet is REPLACED by
    `test_rest_stack_wires_order_runtime` (source-order pins);
    `wal_replay_confirm_symmetry_guard` extended, never weakened;
    `dhan_live_off_phase_a_guard` untouched; NEW
    `ratchet_order_runtime_spawned_only_from_rest_stack`.
11. **Config**: `[order_runtime]` — `enabled`, `paper_fill`,
    `self_test`, `reconcile_interval_secs` (≥60),
    `mark_channel_capacity` ([256, 65536]) in
    `crates/common/src/config.rs`.
12. **Scope OUT** enumerated in the OUT OF SCOPE section below.

### Cluster C — order-side audit tables revival (owning session TBD; parallel-safe)

Re-create the order_audit-family QuestDB persistence deleted 2026-05-20
(#T2a/#T2b): `order_audit` (SEBI 5y), `pnl_audit`, and the sibling
order-event tables, following the `static_ip_audit_persistence.rs`
8-element template with **feed-in-key DEDUP** (`data-integrity.md`
"feed-in-key EVERYWHERE": composite `(security_id, exchange_segment,
feed)` + `trading_date_ist` + `outcome` where lifecycle rows must both
survive), idempotent `ALTER ADD COLUMN IF NOT EXISTS` self-heal, and
ILP-over-HTTP per-flush ACK (the 2026-07-05 fire-and-forget lesson).
Files: `crates/storage/src/` (new `*_audit_persistence.rs` modules) +
the `crates/app/src/order_runtime.rs` emit seam. Tests: TBD by the
owning session (template ratchets: table-name const, DEDUP-key
designated-timestamp test, DDL-columns test, micros-conversion test,
per-outcome tokio tests; `dedup_segment_meta_guard.rs` must stay green).

### Cluster D — safety gates: orphan-watchdog re-homing + expired date-gate re-arm (PR #1545 in flight)

The orphan-position watchdog (15:25 IST `GET /v2/positions`
cross-check, ORPHAN-POSITION-01) does not run on dhan-off boots — both
`spawn_post_market_tasks` callers are lane-gated and
`dhan_rest_stack.rs` excludes it. Re-home it into the process-global
prefix / rest-stack family, and re-arm the two EXPIRED (2026-07-01)
date-based live-order gates. Files: `crates/app/src/main.rs`
(process-global prefix), `crates/trading/src/oms/engine.rs`
(SANDBOX_DEADLINE region), `crates/common/src/constants.rs`
(LIVE_TRADING_EARLIEST). Owned by the cluster D session; PR #1545 in
flight.

### Cluster E1 (exit-order layer) / E2 (portfolio + margin) — coarse placeholders (sessions being spun up)

E1: the exchange-resident exit layer — Super Order (3-leg
entry/TP/SL + trailingJump) / OCO wiring of the already-complete but
caller-less `crates/trading/src/oms/api_client.rs` typed wrappers,
MPP-aware execution verification, slicing; serial after cluster A;
must resolve the hostile-review ladder holes (post-fill ENTRY_LEG
cancel race, ghost-order re-entry block, SL-leg-REJECT rung, batched
`GET /super/orders`). E2: portfolio + funds/margin surface. Margin
gate contract (design-only, held for the operator's REST grant —
`/v2/margincalculator` is a Dhan REST call needing a
`no-rest-except-live-feed-2026-06-27.md` dated edit first):
`OrderIntent{Entry{required_paise}, Exit}` appended inside
`RiskEngine::check_order` (`crates/trading/src/risk/engine.rs`);
**exits are never margin-gated** — an exit must always be placeable.

## Plan Items

Cluster A (this session's PR; Files/Tests per the judge-approved final design):

- [x] A1 — order_runtime.rs actor: supervised single-owner task, select! arms (order-update / marks / reconcile / 16:00 reset / 15:30 sweep / self-test), NotifierAlertSink — merged via PR #1562 (checkbox ticked 2026-07-17, post-merge follow-up)
  - Files: crates/app/src/order_runtime.rs, crates/app/src/oms_wiring.rs
  - Tests: test_apply_fill_reaches_risk_engine_net_lots_nonzero, test_alert_sink_event_mapping, test_halt_fires_risk_halt_once_per_episode, prop_fill_mirror_matches_risk_net_lots
- [x] A2 — FillEvent widening of handle_order_update (+ 4-line trading_pipeline graft) — merged via PR #1562
  - Files: crates/trading/src/oms/engine.rs, crates/trading/src/oms/types.rs, crates/trading/src/oms/mod.rs, crates/app/src/trading_pipeline.rs
  - Tests: test_same_status_refresh_applies_delta_not_cumulative, test_duplicate_update_zero_delta_skipped, test_partial_lot_remainder_floors_and_errors, test_fill_sign_from_managed_order_transaction_type, test_parse_segment_chars_matrix
- [x] A3 — ticks→update_market_price gate (marks_wanted AtomicBool + MarkUpdate try_send tap) — merged via PR #1562. *Seam re-home note (2026-07-17): the planned `groww_bridge.rs` consume seam died with the Groww live feed (#1581, 2026-07-15); the tap shipped at the Groww per-minute REST legs' persist-confirm choke points (`groww_spot_1m_boot.rs` + `groww_contract_1m_boot.rs`), ≤4 spot + ~30 contract marks/min*
  - Files: crates/app/src/main.rs (the Groww spot-1m and contract-1m boot modules this item also touched were deleted with the Groww feed on 2026-08-21) (originally planned: crates/app/src/groww_bridge.rs — deleted 2026-07-15)
  - Tests: test_mark_forward_skips_send_when_marks_wanted_false, test_mark_forward_channel_full_counts_drops_and_never_blocks, dhat_mark_forward (0 alloc / 10K), Criterion order_gate/mark_forward ≤ 50ns
- [x] A4 — WAL drain + conditional confirm in dhan_rest_stack Phase 5a — **CUT 2026-07-14, not implemented** (resolved-by-supersession; ticked 2026-07-17 so the cut is recorded, never silently skipped): the operator Dhan noise lock (`dhan-rest-only-noise-lock-2026-07-14.md` §3 + scope-lock §A.1) removed the order-update WAL drain/confirm AND the `run_order_update_connection(.., wal_spill: Some(..))` socket spawn from the shipped scope. The retained live re-arm spec lives in `order-runtime-dryrun.md` §2 — re-arming needs a fresh dated operator quote in the noise-lock file FIRST
  - Files: (none shipped — see the §2 re-arm spec)
  - Tests: test_rest_stack_wires_order_runtime + test_rest_stack_order_update_push_gated_and_no_canary pin the shipped socket-free/WAL-free shape; ratchet_order_runtime_spawned_only_from_rest_stack shipped
- [x] A5 — reconcile scheduler with honest dry-run heartbeat + local Σfills==net_lots invariant — merged via PR #1562
  - Files: crates/app/src/order_runtime.rs
  - Tests: test_dry_run_reconcile_returns_empty_report, test_local_reconcile_invariant_holds_and_diverges (2026-10-01: names updated to the current tests. The original `classified_heartbeat_not_ok` test no longer exists, and no current test asserts that a dry-run reconcile heartbeat is classified "not ok"; recorded, not fixed)
- [x] A6 — paper filler + once-daily gated self-test + orphan-fill loudness — merged via PR #1562
  - Files: crates/app/src/order_runtime.rs
  - Tests: test_paper_fill_deferred_until_finite_positive_mark, test_terminal_order_never_refilled, test_selftest_single_cycle_latched, test_selftest_window_gate_boundaries, test_orphan_fill_update_tolerated_book_unchanged, test_source_n_filtered_empty_tolerated, order_runtime_e2e (crates/app/tests/)
- [x] A7 — risk P&L lot_size fix + evaluate_daily_loss_halt + trigger_halt code field + sid-segment tripwire — merged via PR #1562
  - Files: crates/trading/src/risk/engine.rs, crates/app/src/order_runtime.rs
  - Tests: test_unrealized_pnl_multiplies_lot_size, test_evaluate_daily_loss_halt_boundary, test_tripwire_divergence_error_latched_per_sid, test_apply_fill_refuses_unknown_segment, test_daily_reset_clears_book_mirror_tripwire_flag_atomically
- [x] A8 — config section + rule files (order-runtime-dryrun.md; dated notes in ws-reinject-error-codes.md + websocket-connection-scope-lock.md) — merged via PR #1562
  - Files: crates/common/src/config.rs, config/base.toml, .claude/rules/project/order-runtime-dryrun.md, docs/error-runbooks/ws-reinject-error-codes.md
  - Tests: config validation unit tests (interval ≥ 60, capacity bounds), serde-default-off test

Other clusters (checked off by their owning sessions' PRs, all referencing THIS plan):

- [x] C1 — order_audit-family QuestDB persistence revival (feed-in-key DEDUP)
  - Files: crates/storage/src/order_audit_persistence.rs, crates/storage/src/pnl_audit_persistence.rs, crates/app/src/order_observability.rs
  - Tests: TBD by owning session (8-element template ratchets)
  - Ticked 2026-10-01 (Item 48 of active-plan-feed-hardening.md): the box was never ticked, but `OrderAuditWriter` and `ensure_order_audit_table` are live — `order_observability.rs` writes the rebuilt SEBI `order_audit` + `pnl_audit` rows.
- [x] CT1 — Conditional & Multi Order surface (dhanhq v2 /alerts family): typed constructors +
      `POST /alerts/multi/orders` wrapper, dormant behind the hardcoded alerts gate,
      Equities/Indices fail-closed segment lock
  - Gate: `alerts_gate_armed: bool = false` inside `OrderApiClient::new()` (house Lock-1 mirror:
    hardcoded default + #[cfg(test)]-only `arm_alerts_gate_for_test`); ALL SIX /alerts senders
    (5 existing Phase-6 fns + new `place_multi_order`) call `require_alerts_gate` FIRST and
    refuse with the NEW additive `OmsError::AlertsSurfaceDisarmed{operation}` before any
    URL/socket work. Zero engine.rs / app-crate / config edits. Ratcheted by NEW
    `crates/trading/tests/conditional_gate_guard.rs` (7 source-scan tests incl. scanner
    self-test, production-region #[cfg(test)] split, single-choke-point path grep,
    zero-production-caller dormancy pin).
  - Types (types.rs additive): `MultiOrderLeg`/`DhanMultiOrderRequest`/`DhanMultiOrderResponse`/
    `MultiOrderLegResult` (DISTINCT schema: float prices + int disclosedQuantity + sequence/
    correlationId/AMO vs the conditional legs' string prices + discQuantity), `DhanNumeric`
    (number-or-string tolerance), `TriggerConditionDetail` (response-only, string-comparingValue
    safe), `TriggerOrderDetail` (response-only order-leg echo mirror — all-defaulted,
    DhanNumeric prices; review round 1 fix 2026-07-14: the echo was initially typed with the
    strict request-side `TriggerOrder`, which would brick a GET/GET-all on one sparse leg),
    GET-detail fields (createdTime/triggeredTime UTC-Z + lastPrice + condition + orders,
    all #[serde(default)] additive), modify-body optional `alert_id`.
  - Constructors (NEW crates/trading/src/oms/conditional.rs, pure): `ConditionalSegment`
    {NSE_EQ, BSE_EQ, IDX_I} (condition, docs-verbatim) + `ConditionalLegSegment` {NSE_EQ, BSE_EQ}
    (legs — IDX_I not orderable, F&O fail-closed per the family support note; widening needs an
    operator quote + .claude/rules/dhan/conditional-trigger.md edit FIRST); `TriggerIndicatorName`
    (21 wire values), `AmoTime`, `TriggerConditionSpec` (4 variants = mandatory-field matrix
    unrepresentable-wrong), `ConditionalBuildError`; `build_trigger_condition`,
    `build_trigger_order`, `build_conditional_trigger_request`, `with_alert_id`,
    `build_multi_order_request` (1..=15 legs via DHAN_CONDITIONAL_MAX_ORDERS_PER_REQUEST,
    auto-stamped "1".."N" sequences, paise-integer price inputs — exact string formatting for
    conditional legs, capped paise→f64 for multi legs).
  - Ledger + docs: dhan_api_coverage conditional 5→6, rest_paths/constants 16→17→19, totals
    re-derived (review round 1 fix 2026-07-14: the 2 LIVE option-chain constants — §8 rebuild,
    app-crate scheduled pull — were absent and falsely narrated "no longer implemented";
    review round 3 fix 2026-07-14: the 53/57 path-side arithmetic was irreproducible — the
    inline set is now MEASURED by a source scan of api_client.rs' production region and
    pinned as 19 templates incl. get_positions' inline /positions, websocket_count corrected
    4→2 per the file's own two-WS test; round-8 truth-sync: the 2 depth WS URL constants are
    orphan-PINNED, not implemented — honest grand totals 37 unique path templates + 2 live WS
    = 39 implemented, + 2 orphaned depth WS constants + 4 intentionally skipped = 45 known,
    over 41 per-method REST operations); `DHAN_ALERTS_MULTI_ORDERS_PATH` constant + the constants.rs
    slash-test array; NEW `.claude/rules/dhan/conditional-trigger.md` (closes the CLAUDE.md
    index drift; 18 mechanical rules incl. the multi-order divergence traps, UTC-Z, bare-array
    GET-all, ONCE-vs-ALWAYS, no-CONFIRM); dated `2026-07-14 Upstream Update (2)` append to
    `docs/dhan-ref/07c-conditional-trigger.md`. ZERO ErrorCode, ZERO NotificationEvent, ZERO
    Telegram (noise lock).
  - Honest envelope: multi endpoint response schema yaml-only UNVERIFIED-LIVE; rate bucket
    Assumed (Order class); atomicity + quantity-on-modify undocumented; everything dormant —
    dry_run true, zero production callers (ratcheted), no boot task.
  - Files: crates/trading/src/oms/types.rs, crates/trading/src/oms/conditional.rs (NEW),
    crates/trading/src/oms/api_client.rs, crates/trading/src/oms/mod.rs,
    crates/common/src/constants.rs, crates/common/tests/dhan_api_coverage.rs,
    crates/trading/tests/conditional_gate_guard.rs (NEW),
    .claude/rules/dhan/conditional-trigger.md (NEW), docs/dhan-ref/07c-conditional-trigger.md
  - Tests: test_alerts_gate_defaults_disarmed_in_constructor, test_alerts_gate_arm_is_cfg_test_only,
    test_every_alerts_sender_checks_gate_first, test_no_production_caller_of_alerts_sender_fns,
    test_leg_segment_enums_are_fail_closed, test_alerts_paths_single_choke_point,
    test_gate_guard_scanner_self_test, test_place_multi_order_success,
    test_place_multi_order_blocked_when_gate_disarmed_no_socket,
    test_place_multi_order_rate_limited_429, test_place_multi_order_dhan_error_400_records_metric,
    test_place_multi_order_malformed_json_error, test_url_expression_multi_order_constant_joins_alerts_multi_path (renamed round-4 — the old test_url_construction_* name claimed sender behavior; the sender's actual POST + path is now wire-observed by test_place_multi_order_wire_sends_post_to_alerts_multi_path via the round-4 request-capturing mock),
    test_existing_conditional_fns_blocked_when_gate_disarmed, test_arm_alerts_gate_for_test_arms_gate,
    test_multi_order_request_serializes_camel_case_exact, test_conditional_vs_multi_price_type_split,
    test_multi_order_response_parses_unknown_order_status_modified_inactive_no_panic,
    test_trigger_response_detail_fields_roundtrip, test_dhan_numeric_accepts_number_and_string,
    test_modify_request_alert_id_absent_when_none, test_build_trigger_condition_all_four_comparison_types_serialize_mandatory_fields,
    test_build_trigger_order_price_boundary_market_refused_limit_zero_rejected,
    test_build_trigger_order_quantity_boundary_zero_negative_i32_max_edge,
    test_build_trigger_order_disclosed_quantity_boundary_30pct_edge,
    test_build_conditional_trigger_request_leg_count_boundary_0_1_15_16,
    test_build_multi_order_request_boundary_15_legs_max_16_rejected_zero_rejected,
    test_build_multi_order_request_assigns_sequences_one_to_n,
    test_build_multi_order_request_correlation_id_boundary_30_max_31_rejected_and_charset,
    proptest_build_multi_order_request_never_panics_on_arbitrary_spec
    + review-hardening tests (rounds 1-9: tolerant echo mirrors, gate-site census —
    comment-blanking / destructuring-assignment / mut-borrow arms + anchored /alerts
    allowlist, dhan-client-id + security-id content validation, slashless family needles,
    wire-observed multi-order POST, bodyless-200 tolerance, orphan pins + extractor
    self-test). Branch-added test-fn inventory: 7 conditional_gate_guard.rs source-scan
    tests (scanner internals hardened across all 9 rounds), 11 api_client.rs
    multi-order/gate tests, 23 conditional.rs constructor tests (incl. the proptest),
    14 types.rs wire-shape tests, 3 dhan_api_coverage.rs orphan/extractor tests —
    58 total.
- [x] D1 — orphan-watchdog re-homing + expired date-gate re-arm (PR #1545)
  - Files: crates/app/src/main.rs, crates/trading/src/oms/engine.rs, crates/common/src/constants.rs
  - Tests: crates/app/tests/orphan_position_watchdog_wiring_guard.rs
  - Ticked 2026-10-01 (Item 48 of active-plan-feed-hardening.md): the daily 15:25 IST orphan-position watchdog is spawned process-wide in `main.rs` (the 2026-07-14 re-home); the box was never ticked.
- [x] E1 — exit-order layer (Super Order/OCO wiring, MPP verify, slicing) — serial after A
  - Files: crates/trading/src/oms/ (engine.rs, api_client.rs call sites), crates/app/src/order_runtime.rs
  - Tests: TBD by owning session
  - Implemented (owning session label "Cluster B", branch claude/dhan-exit-order-layer): crates/trading/src/oms/{engine,exit_rules,types,api_client,state_machine}.rs + crates/app/src/exit_execution.rs (dispatcher hub) + the trading_pipeline.rs dispatcher seam; guard crates/trading/tests/dhan_exit_order_lockout_guard.rs + rule .claude/rules/project/dhan-exit-order-lockout-2026-07-14.md. Landed 4-lock-dormant (default-off config, hardcoded dry_run); order_runtime integration remains Cluster A's seam.
- [ ] E2 — portfolio + margin gate (OrderIntent in RiskEngine::check_order; design-only until the operator REST grant)
  - Files: crates/trading/src/risk/engine.rs, crates/trading/src/oms/api_client.rs
  - Tests: TBD by owning session (exit-never-gated invariant test mandatory)
- [x] B1 — Scheduled OMS reconcile loop, config-gated default-OFF
  - Files: crates/app/src/trading_pipeline.rs, crates/common/src/config.rs, config/base.toml, crates/app/tests/oms_reconcile_wiring_guard.rs
  - Tests: should_run_scheduled_reconcile decision table, oms_reconcile_wiring_guard ratchets (7), config round-trip/defaults
- [x] B2 — Dhan order-update decode hardening (dual-casing Value-stage normalizer, frame-size cap, terminal-guard fill preservation)
  - Files: crates/core/src/parser/order_update.rs, crates/core/src/websocket/order_update_connection.rs, crates/trading/src/oms/engine.rs, crates/core/tests/order_update_choke_point_guard.rs
  - Tests: parser T3-T16 + proptests, choke-point guard (3), reconcile terminal-guard scenarios

> B1/B2 added+ticked by the ingestion session (coordinator-approved scope 2026-07-14); build lead notified for #1561 de-dup.

## Hard invariants (every Dhan-order PR states these)

1. **4-lock OFF switch**: `dry_run` stays `true`, hardcoded in
   `crates/trading/src/oms/engine.rs` (`enable_live_mode` stays
   `#[cfg(test)]`); no code path reaches a live Dhan order POST without
   the operator's explicit enable (code change + fresh dated quote +
   the pre-live follow-ups) — config flips alone can never go live.
2. **§28 boundary**: `crates/trading/src/strategy/` +
   `crates/trading/src/indicator/` stay frozen
   (`daily-universe-scope-expansion-2026-05-27.md` §28); no cluster
   constructs IndicatorEngine/StrategyInstance or spawns
   trading_pipeline on dhan-off.
3. **RAM-first hot path**: no QuestDB SELECT between tick receipt and
   any order decision; the marks tap is 1 Relaxed atomic load +
   try_send of a Copy struct (DHAT + Criterion evidence per PR that
   touches the seam).
4. **Broker attribution**: every order-side Telegram carries 🔷 DHAN
   broker attribution (the operator's dual-broker readability demand).
5. **DEDUP keys include segment + feed** on every new/revived QuestDB
   table (I-P1-11 + `data-integrity.md` feed-in-key;
   `dedup_segment_meta_guard.rs` stays green).
6. **Every `error!` carries `code = ErrorCode::X.code_str()`**
   (tag-guard); flush/persist failures use `error!`, never `warn!`.

## OUT OF SCOPE / OPERATOR GATES

- **trading_pipeline / strategy / indicator activation** — operator §28
  gate; the order runtime never activates them; any future signal
  source needs its own dated operator scope FIRST.
- **RiskEngine/OMS composite-key `(u64, u8)` rewrite** — deferred with
  the cluster A tripwire; a MANDATORY pre-live follow-up (a live Dhan
  order path requires it before `dry_run` ever flips).
- **Any live-mode flip** — `enable_live_mode` stays `#[cfg(test)]`;
  sandbox/date gates re-armed by cluster D are re-ARMED, not opened;
  live trading requires a fresh dated operator quote + the pre-live
  follow-up list.
- **Groww order surface** — 100% absent by design
  (`groww-second-feed-scope-2026-06-19.md` §1 forbids Groww orders);
  a SEPARATE umbrella + dated operator quote required before any Groww
  order code exists.
- **Dhan margin-calculator REST call (E2)** — held for the operator's
  `no-rest-except-live-feed-2026-06-27.md` grant; E2 ships the
  `OrderIntent` contract design-only until then.
- **D2's `POST /api/order-runtime/*` command endpoints** — cut (attack
  surface); tickvault-api gains read-only status surfaces at most.

## Edge Cases

Duplicate/same-status cumulative order updates (delta = 0); replayed
WAL frames hitting an empty book after restart (loud orphan warn, never
silent fill loss); empty `Source` tolerated / `Source=N` (manual Dhan
app) filtered at the runtime consumer while the WAL keeps capturing ALL
frames (SEBI); partial-lot fill remainder floored + coded; 0/NaN mark
defers the paper fill (never fabricate a price); mark channel full
(counted drop, next tick supersedes — positions exact, marks
best-effort); >200 replayed frames vs broadcast(256) (warn + counter
envelope); sid-segment collision tripwire (loud skip, never a merged
P&L); 16:00 IST daily reset racing an in-flight fill (single-task
serialization — one reset block); holiday/off-hours self-test refusal
(calendar + window + once-per-day latch); disabled `[order_runtime]`
config = byte-identical dormant boot; stale live-feed WAL segments on a
dhan-off boot (confirm deferred, segments re-staged, operator archive
runbook ends the deferral); cluster D's watchdog on a dhan-off boot
with zero positions (clean `no_orphans` row, no page); cluster C ILP
flush reject (discard-pending poisoned-buffer defense, DEDUP-idempotent
re-append).

## Failure Modes

Cluster A carries the full F1–F22 catalogue from the judge-approved
final design (silent-stuck-position, cumulative double-count, hidden
book reset, WAL ordering/confirm loss, never-confirm growth, dual-OMS
split-brain, hot-path alloc at the groww_bridge seam, stale marks,
I-P1-11 sid merge, paper-fill feedback loop, halt flapping, dry-run
reconcile false-OK, live reconcile storm, reset race, foreign-source
corruption, sink stall, zero-mark fabrication, WAL replay burst,
understated daily-loss halt) — each with a named countermeasure and a
named test. Cluster-level failure modes: C — audit ILP outage loses
forensic rows only (never order state; best-effort + typed error +
counter, the AUDIT-WS-01 pattern); D — watchdog REST failure at 15:25
IST degrades loudly (ORPHAN-POSITION-01 semantics preserved), date-gate
re-arm can never SOFTEN a gate (re-arm = future date, ratchet-pinned);
E1 — post-fill leg-cancel race / ghost-order re-entry / SL-leg-REJECT
are named pre-live design changes (UNVERIFIED-LIVE, refused until
probed); E2 — margin-gate unavailability fails CLOSED for entries and
OPEN for exits (an exit must always be placeable). Cross-cluster: seam
collisions are prevented by the ownership table below — a session
touching another cluster's seam rebases and re-runs that seam's
ratchets, never force-merges.

## Test Plan

Per cluster, block-scoped per `testing-scope.md` (`cargo test -p
tickvault-app -p tickvault-trading` for A; `-p tickvault-storage` for
C; workspace escalation whenever `crates/common/` is touched — A8/D1
touch config.rs/constants.rs, so those PRs run `cargo test
--workspace`). Cluster A: the ~24 named unit tests + proptest invariant
+ `order_runtime_e2e` integration + DHAT/Criterion perf evidence listed
per item above; ratchet replacements (dormant-shape →
`test_rest_stack_wires_order_runtime`; `wal_replay_confirm_symmetry_guard`
extended; `dhan_live_off_phase_a_guard` verified untouched; new
spawn-site ratchet). Cluster C: the 8-element audit-table template
ratchets + `dedup_segment_meta_guard.rs`. Cluster D: watchdog wiring
source-scan guard + date-gate re-arm pin tests. Cluster E: named by the
owning sessions before code (design-first — items stay unchecked until
their PR merges). Every PR: banned-pattern + pub-fn-test +
pub-fn-wiring + secret scan + the adversarial 3-agent review
(hot-path + security + hostile) before AND after implementation; merge
only via All Green (`merge-gate-lock-2026-07-04.md`).

## Rollback

Cluster A: `[order_runtime] enabled = false` (or deleting the section —
serde default OFF) restores the exact current dormant shape (discard
drain, `wal_spill: None`), pinned by the disabled-branch ratchet; full
revert = `git revert` (no schema changes, no data migration; WAL
segments stay replayable either way). Cluster C: audit tables are
additive — revert removes the writers; tables remain harmless
(never-delete, SEBI); re-land re-runs idempotent DDL. Cluster D: revert
restores the (current, known-bad) lane-gated watchdog spawn — a
documented regression, not data loss; date-gate re-arm reverts to
expired constants (the hardcoded dry_run + no-spawn locks still hold).
E1/E2: config-gated OFF by default; revert = git revert, no persisted
state. Umbrella-level: this plan file itself is archived (per
`plan-enforcement.md` rule 7) when the last cluster merges or the
operator supersedes the build; no cluster's rollback depends on another
cluster being present.

## Observability

Cluster A (all static labels): `tv_order_runtime_up`,
`tv_order_update_events_total{outcome}`,
`tv_oms_orphan_fill_updates_total`,
`tv_risk_fills_recorded_total{kind}`, `tv_mark_forward_dropped_total`,
`tv_paper_fills_synthesized_total`, `tv_paper_fills_deferred_total`,
`tv_oms_reconcile_runs_total{mode}`,
`tv_oms_local_reconcile_divergence_total`,
`tv_paper_selftest_total{outcome}`, `tv_wal_confirm_deferred_total`,
`tv_order_runtime_respawn_total{reason}`; the existing
`tv_realized_pnl`/`tv_unrealized_pnl` gauges start moving. Telegram:
RiskHalt (Critical), CircuitBreakerOpened/Closed, OrderRejected,
RateLimitExhausted, the self-test heartbeat — every one carrying 🔷
DHAN attribution. `error!` codes: OMS-GAP-01/02/03/04/06,
RISK-GAP-01/02; `warn!` WS-REINJECT-01
(reason=`confirm_deferred_stale_livefeed`, non-paging). Rule files: NEW
`.claude/rules/project/order-runtime-dryrun.md` + dated notes in
`ws-reinject-error-codes.md` + `websocket-connection-scope-lock.md`.
Cluster C adds the durable QuestDB forensic layer (order_audit family)
+ its write-failure counters; cluster D re-arms ORPHAN-POSITION-01's
daily row + page; E1/E2 name their metrics in their own dated plan
updates before code. Delivery-boundary honesty: no new CloudWatch
log-filter alarms are added by cluster A (zero new codes); any cluster
adding one records the cost note per `aws-budget.md`.

---

## Seam ownership (collision contracts)

| Seam | Owner | Notes |
|---|---|---|
| `crates/app/src/dhan_rest_stack.rs` (Phase 5a + params) | Cluster A session (this round) | Ordering-law rewrite; others rebase on A's merge |
| `crates/app/src/main.rs` process-global prefix | Cluster D | A's main.rs delta is the mark-channel plumbing only (before the groww_bridge spawn) — disjoint region; D owns the prefix task family |
| `crates/trading/src/oms/engine.rs` — `handle_order_update` + FillEvent region | Cluster A | Return-widening + delta math |
| `crates/trading/src/oms/engine.rs` — SANDBOX_DEADLINE const region | Cluster D | Date-gate re-arm (PR #1545) |
| `crates/trading/src/risk/engine.rs` | Cluster A this round (lot_size fix + halt extraction); E2's margin gate REBASES after A merges | `check_order` gains OrderIntent in E2 only |
| `crates/app/src/groww_bridge.rs` tick seam | Cluster A | marks tap (one guarded try_send block) |
| Storage order-audit writers (`crates/storage/src/`) | Cluster C | A emits via a seam C fills in; A ships without tables (flagged follow-up) |
| `oms/{api_client.rs error/ladder additions, error_taxonomy.rs, order_readiness.rs}` + engine gate wiring (outside :256-277 / :549-660) | Cluster F (branch claude/dhan-order-error-taxonomy) | consumption-side classifier; SANDBOX + handle_order_update regions untouched; ~35 legacy api_client error sites byte-identical |
| `crates/trading/src/oms/conditional.rs` (+ the api_client.rs conditional-family block :575-780 and the types.rs conditional/multi sections) | Cluster CT (this item) | New module — no other cluster touches it; api_client edits confined to the /alerts family block + end-appends; mod.rs gains one additive line |

Build-lead: the cluster A session. Conflicts on any seam: the
non-owner rebases; never a force-merge over an owner's in-flight PR.

## Cluster F — Dhan order-path error taxonomy + readiness gate (branch claude/dhan-order-error-taxonomy, PR #1558)

Cluster F claimed by #1558 (Dhan order-path error taxonomy + readiness gate).
This cluster was ABSENT from the original umbrella (A/C/D/E only); the letter
"B" was reserved by the order-side ALERTING PR (`claude/dhan-order-alerting`),
so this block is relabeled B→F and moved to a distinct anchor to avoid a
textual + semantic collision with that B block. build-lead (Cluster A) is the
letter registrar — please confirm.

- [x] F1 — order-path DH/DATA error taxonomy + per-endpoint retry matrix + 805 STOP-ALL latch + DH-904 ladder wiring
  - Files: crates/trading/src/oms/error_taxonomy.rs, api_client.rs, mod.rs, crates/common/src/constants.rs
  - Tests: ratchet_dh904_ladder_constants_and_wiring, ratchet_stop_all_cooldown_is_60s_per_annexure_rule_12, prop_classify_never_panics_and_is_deterministic_on_arbitrary_status_and_body
- [x] F2 — order-readiness gate (profile/token headroom, fail-closed, live-path-only) + supervised probe refresher (boot spawn = seam handoff)
  - Files: crates/trading/src/oms/order_readiness.rs, engine.rs, types.rs
  - Tests: test_place_order_live_refused_when_no_readiness_installed_zero_http_zero_token_fetch, test_place_order_dry_run_ignores_readiness_gate_byte_identical, test_evaluate_order_readiness_stale_boundary_2100_passes_2101_refuses
- [x] F3 — ORDER-READY-01 ErrorCode + rule file + OmsAlert 🔷 DHAN attribution
  - Files: crates/common/src/error_code.rs, docs/error-runbooks/order-readiness-error-codes.md, engine.rs
  - Tests: test_order_ready_01_contract, test_all_oms_alert_operator_messages_start_with_dhan_badge

## Cluster G — order decision path under 1 µs in memory (branch claude/order-path-sub-us, 2026-10-30)

Owner (Parthiban), 30 Oct 2026, relayed by the coordinator: every in-memory
step of the order path under 1 µs at p99, proven by benches that fail the
build if it slips, with identical behaviour. Network calls, `dry_run`, the
indicator and strategy trees, and the REST surface are out of scope. Matrices:
by cross-reference, as for the clusters above.

- [x] Criterion bench `crates/app/benches/order_path.rs` (production Prometheus recorder installed) + budget key `order_path = 1000` and elements row in `quality/benchmark-budgets.toml`
  - Files: crates/app/benches/order_path.rs, crates/app/Cargo.toml, quality/benchmark-budgets.toml, CLAUDE.md (key count)
  - Tests: benchmark_budget_reaches_a_bench_guard, claude_md_states_the_real_budget_key_count
- [x] Per-call p50/p99 harness (ignored, wall clock): crates/app/tests/order_path_p99_harness.rs
  - Tests: order_path_per_call_latency_distribution
- [x] Correlation ids drawn ahead of use (pool of 256, cold refill at construction and the daily reset; on-demand fallback) and one-allocation encoder
  - Files: crates/trading/src/oms/idempotency.rs, crates/trading/src/oms/engine.rs
  - Tests: proptest_encoder_matches_the_legacy_two_allocation_form, test_next_id_is_a_v4_prefix_of_the_dhan_length_and_never_repeats, test_clear_refills_the_pool_for_the_next_day
- [x] Metric handles cached at first use (rate limiter allowed counter, breaker state gauge, paper placed counter, placement histogram); literal labels where a label variable allocated (order budget refusal, rejected mark, fills recorded); `track_ref` removes one String copy per paper placement
  - Files: crates/trading/src/oms/rate_limiter.rs, crates/trading/src/oms/circuit_breaker.rs, crates/trading/src/oms/engine.rs, crates/trading/src/risk/engine.rs, crates/app/src/order_runtime.rs
  - Tests: order_path_metrics_keep_their_names_labels_and_counts

- [x] Owner decisions, Fri 30 Oct 2026 ~16:50 IST, relayed by the coordinator session (card answered there):
  (1) Q: "Fix it the way we already fixed the tick-delay measurement: fixed counters set up at startup, same metric name, nothing new to read?" A: "Yes, fixed counters (Recommended)" — "Same name on the dashboard and alerts; the internal bucket layout changes slightly. Removes about 400 ns from the slowest orders."
  (2) Q: the per-placement "PAPER TRADE" INFO line (~2.5 µs). A: "Write it after the order (Recommended)" — "Same log line, same content, but written by a background helper right after the order is placed, so it no longer slows the order step. Nothing is lost."
  - Files: crates/trading/src/oms/placement_telemetry.rs, crates/trading/src/oms/engine.rs, crates/trading/src/oms/mod.rs, crates/app/src/order_runtime.rs (once-a-second publish)
  - Tests: fixed_placement_buckets_render_what_the_recorder_histogram_rendered, test_helper_line_is_identical_to_the_old_inline_line, dhat_order_path_with_the_production_recorder, test_apply_fill_counts_paper_and_live_under_their_literal_labels, test_track_ref_matches_track_outcomes_and_keeps_the_callers_id

- [x] Per-call p99 gate in the bench (house `tail_gate`): exits 1 when any order-path step's p99 is over 1,000 ns; book tables prefaulted at construction; `ahash` on the order book, alias map and correlation index; paper id written without `format!`; old-vs-new decisions proptest
  - Files: crates/app/benches/order_path.rs, crates/trading/src/oms/engine.rs, crates/trading/src/oms/idempotency.rs, crates/trading/src/oms/reconciliation.rs
  - Tests: proptest_pooled_ids_decide_exactly_like_on_demand_ids, proptest_paper_order_id_matches_format, test_paper_order_id_matches_format_at_the_extremes

Design: no output changes beyond the two the owner chose above; ids, metric names, labels and counts are the same.
Edge cases: pool empty (falls back to `generate_id`), recorder installed after
first use (handle stays bound to the earlier one; production installs at boot),
log queue full or helper thread not started (the line is written inline and
counted on `tv_oms_paper_trade_log_inline_total{reason}`), abrupt process stop
(at most one 20 ms poll of PAPER TRADE lines unwritten; the orders themselves
are in the book and the audit).
Failure modes: none new; a refused order is refused exactly as before.
Rollback: revert the PR; no data or config change.
Observability: unchanged series (pinned by the parity test).

## Cluster H — self-trader rule fit: strict 10/sec cap, no MARKET/IOC (branch claude/brave-noether-tr1cog, 2026-11-28)

Owner (Parthiban), Sat 28 Nov 2026, after the read-only self-trader rules audit
(SEBI retail algo framework 4 Feb 2025, NSE/INVG/69255, NSE notice 20 Nov 2026):
"go ahead with 1 and 2 dude" — item 1 the strict per-second cap, item 2 refuse
MARKET and IOC orders. Two serial PRs (H1 first). `dry_run`, the exit-order
lockout, the indicator and strategy trees are untouched. Matrices: by
cross-reference, as for the clusters above.

- [x] H1 — strict sliding 1-second window beside the GCRA in `OrderRateLimiter`: refuse when the last N admissions all fall within 1 s (closed window), so no 1-second window ever admits more than N (was up to 2N−1 with burst N + refill). Window checked before the GCRA, recorded only after both admit (a refusal spends nothing). Fixed array of `WINDOW_SLOTS` (64), O(1) time and space, no allocation; above 64 (tests and benches only, config caps production at 10) the window is off and the GCRA alone applies.
  - Files: crates/trading/src/oms/rate_limiter.rs, crates/app/src/order_runtime_live_fill.rs (burst test now asserts the 10-per-second contract), crates/app/benches/order_path.rs (windowed admit bench), crates/app/tests/dhat_order_path_recorder.rs (windowed allocation window)
  - Tests: test_window_never_admits_more_than_n_in_any_closed_second, test_window_exact_one_second_boundary_is_refused_one_nano_later_admitted, test_window_backward_or_equal_instant_is_conservative, test_window_limit_one_admits_one_per_closed_second, test_window_off_above_slot_cap, test_production_limit_has_the_window, test_window_refusal_spends_no_gcra_cell, test_gcra_refusal_records_nothing_in_the_window, test_window_survives_a_poisoned_lock, proptest_window_never_exceeds_n_in_any_closed_second, proptest_window_admits_whenever_room_remains, test_regression_exit_burst_makes_one_refused_attempt_per_round, dhat_order_path_with_the_production_recorder
- [x] H2a — nothing sends MARKET (first of two serial PRs for item 2): every production order builder sends a LIMIT order. The paper self-test entry is a LIMIT at the index mark that picked the sid. Its close is a LIMIT at the entry's average fill price; with no usable price (not finite and > 0) no close is sent, the position is reported dangling (`close_price_unknown`) and the day fails loudly. The paper fill names the order's own type (was always "MKT"). `ExitCommand::CloseAll` and `execute_exit_for_security` take a `close_price: Option<f64>`; with no usable price the actives are still cancelled, no close is placed (never a MARKET fallback), coded `EXIT-ORDER-01` log-sink-only plus `tv_exit_close_refused_total{reason="no_close_price"}`, and CloseAll returns `RiskRejected`. The dormant trading pipeline sends LIMIT entries and exits at the tick's last traded price (a traded price is on the exchange tick; `f32_to_f64_clean`), refusing an entry with no usable price (`tv_pipeline_entry_refused_total{reason="no_price"}`). The order-book prefault placeholder is LIMIT (never sent). Validators are unchanged here.
  - Files: crates/app/src/order_runtime.rs, crates/app/src/exit_execution.rs, crates/app/src/trading_pipeline.rs, crates/trading/src/oms/exit_rules.rs, crates/trading/src/oms/engine.rs, crates/app/benches/order_path.rs, crates/app/tests/dhat_order_path_recorder.rs, crates/app/tests/order_path_metrics_parity.rs, crates/app/tests/order_path_p99_harness.rs
  - Tests: (test_selftest_entry_and_close_are_limit_orders: removed or renamed by H4a), (test_self_test_close_price_needs_a_usable_fill_price: removed or renamed by H4a), test_paper_order_type_label_names_every_order_type, (test_close_all_places_limit_slices_at_the_close_price: removed or renamed by H4a), (test_close_all_without_a_usable_price_places_no_close: removed or renamed by H4a), (test_legacy_exit_without_a_usable_price_places_no_close: removed or renamed by H4a), (test_legacy_exit_close_is_limit_at_the_close_price: removed or renamed by H4a), (test_usable_close_price_keeps_only_finite_positive: removed or renamed by H4a), (test_pipeline_limit_price_refuses_unusable_ltp: removed or renamed by H4a), test_place_order_request_for_long_signal
- [x] H2b — the OMS refuses MARKET, STOP_LOSS_MARKET and IOC (second PR; H2a merged as #2315): one helper `exit_rules::refuse_retail_algo_disallowed` returning `OmsError::RiskRejected`, called first in `validate_order_fields`, `validate_modify_fields`, `validate_modify_super_fields`, `validate_super_order_request` and `validate_forever_oco` (which before H2b never checked validity, so IOC got through), before the dry-run branch and before the budget and rate checks (a refusal spends no slot). It counts `tv_oms_order_refused_total{kind="market"|"stop_loss_market"|"ioc"}` (literal labels) and logs one `warn!` (log-sink only). New `validate_priced_order`: a LIMIT or STOP_LOSS needs a finite price > 0, a STOP_LOSS a finite trigger > 0 (before, a LIMIT at 0 or NaN reached the wire unless the caller checked). Super and forever/OCO entries must be LIMIT. The conditional and multi-order builders refuse MARKET / STOP_LOSS_MARKET legs (`ConditionalBuildError::OrderTypeNotAllowed`) and IOC (`ValidityNotAllowed`). The unreachable MARKET price-0 arms are deleted; enum variants kept (parsing).
  - Files: crates/trading/src/oms/exit_rules.rs, crates/trading/src/oms/engine.rs, crates/trading/src/oms/conditional.rs, crates/trading/tests/oms_integration.rs, crates/trading/tests/gap_enforcement.rs
  - Tests: test_refuse_retail_algo_disallowed_every_type_and_validity, test_refuse_retail_algo_disallowed_names_what_it_refused, (test_validate_forever_oco_refuses_market_slm_and_ioc: removed or renamed by H4a), test_validate_super_order_request_refuses_slm_as_stop_loss_market, (test_validate_super_order_request_refuses_market_with_zero_price: removed or renamed by H4a), test_validate_order_fields_refuses_ioc_validity, test_validate_order_fields_limit_needs_a_positive_finite_price, test_validate_order_fields_stop_loss_needs_a_positive_finite_trigger, test_validate_modify_fields_refuses_ioc_and_unpriced_limit, (test_validate_modify_super_fields_refuses_market_entry: removed or renamed by H4a), test_validate_modify_fields_refuses_priced_stop_loss_market, test_place_order_ioc_refused_before_any_slot, (test_market_order_refused_before_any_slot: removed or renamed by H4a), test_slm_order_refused_before_any_slot, (test_validate_order_fields_market_order_refused: removed or renamed by H4a), test_validate_order_fields_slm_with_trigger_refused, (test_validate_modify_fields_market_refused: removed or renamed by H4a), test_build_trigger_and_multi_order_refuse_ioc, (test_build_trigger_order_price_boundary_market_refused_limit_zero_rejected: removed or renamed by H4a), test_build_trigger_order_price_and_trigger_refusal_arms
  - Deferred to H3 (done there): the paper filler still fills a LIMIT at any mark. Making it honour the limit would make the daily paper self-test's close (a SELL LIMIT at the entry's fill price) wait for the price to come back when the index falls, so the day would fail falsely at the 180 s timeout. It needs a marketable-limit band on the self-test close first.

- [x] H3 — paper fills honour the LIMIT price (third PR, after H2b merged as #2316; owner "fix and resolve evryhtign build implement m erge and deploy it dude okay?", 2026-11-28): the paper filler fills a LIMIT BUY only at a mark at or below its price and a SELL only at or above it, at the mark (as on the exchange, where a marketable limit fills at the best price up to its limit); otherwise the order rests, stays pending and is retried on the next mark, counted on `tv_paper_fills_resting_total` (local `/metrics` only). Other order types keep the old rule. The paper self-test's entry BUY and close SELL are priced with the house marketable-limit builder (`brutex_host::order_price::marketable_limit`, `DEFAULT_BAND_BPS` 10%, 0.05 grid), so a self-test still fills when the index moves up to 10% inside its 180 s window.
  - Files: crates/app/src/order_runtime.rs
  - Tests: test_paper_limit_reached_buy_and_sell, (test_self_test_limit_price_band_on_the_grid: removed or renamed by H4a), test_paper_limit_buy_rests_until_the_mark_reaches_it, test_selftest_passes_when_the_index_falls_before_the_close, (test_selftest_entry_and_close_are_limit_orders: removed or renamed by H4a), a_mark_on_another_segment_must_not_fill_a_pending_paper_order, test_e2e_place_fill_mark_halt_fires_risk_halt_sink

- [x] H4a — MARKET orders again, entry and exit (owner, Sat 28 Nov 2026: "i bleive 10 would be betetr right dude see always we wil lsend the market roders and continuosul ykeep tarckign fo ti whetehr it executed ro not right dude se eebcasue dhan will take care of market orders right dude ?"; card "Send MARKET, Dhan converts"; then "what bro nowhere we can assume the limit roder right dude see always it shodu leb amrket roder rigth dude and continuously tarck libe for each and evry tick what sent and whether it got executed alsi as well right dude?"). Reverses H2a and the MARKET half of H2b; keeps the strict 10/sec cap (H1). Dhan converts an API MARKET order to a LIMIT with market price protection, so MARKET is legal to send. `refuse_retail_algo_disallowed` now refuses only STOP_LOSS_MARKET and IOC (any order type); MARKET needs price 0 (`validate_priced_order`), and super, forever/OCO, modify and conditional legs accept MARKET at price 0 again. The H2a LIMIT builders in the exit layer and the dormant pipeline are reverted to MARKET (the `close_price` argument and its refusal counters are gone). The paper self-test entry and close are MARKET at price 0; the H3 band helpers are removed. The paper filler still honours a LIMIT order's price (H3) for any LIMIT order that is placed. Tracking every order to its fill, with cancel and re-send at 30 s, is H4b.
  - Files: crates/trading/src/oms/exit_rules.rs, crates/trading/src/oms/engine.rs, crates/trading/src/oms/conditional.rs, crates/app/src/exit_execution.rs, crates/app/src/trading_pipeline.rs, crates/app/src/order_runtime.rs
  - Tests: test_refuse_retail_algo_disallowed_every_type_and_validity, test_refuse_retail_algo_disallowed_names_what_it_refused, test_validate_forever_oco_refuses_slm_and_ioc_accepts_market, test_validate_super_order_request_accepts_market_with_zero_price, test_market_order_zero_price_accepted, test_validate_order_fields_market_order_valid, test_validate_modify_fields_market_valid, test_validate_modify_super_fields_market_entry_and_slm, test_build_trigger_order_price_boundary_zero_market_ok_limit_zero_rejected, test_selftest_entry_and_close_are_market_orders, test_selftest_passes_when_the_index_falls_before_the_close

Design (H4a): MARKET orders only, as the owner set; Dhan's price protection is the limit. Edge cases: a MARKET order with a non-zero price is refused; IOC and STOP_LOSS_MARKET stay refused. Failure modes: a MARKET order converted by Dhan can stay pending (H4b tracks and re-sends it). Rollback: revert the PR. Observability: `tv_oms_order_refused_total{kind="stop_loss_market"|"ioc"}`; `tv_paper_selftest_total{outcome}` loses `close_price_unknown`. Limits: paper fills a MARKET order at the mark; live, an unfilled converted order rests until H4b's re-send.

Design (H3): a resting paper order is the honest outcome of a limit the market has not reached; the self-test, which must fill to prove the order path, prices both legs with the same 10% marketable band the live paper chain uses. Edge cases: a mark exactly at the limit fills; a non-finite or non-positive mark still defers as before (counted on `tv_paper_fills_deferred_total`, never as resting); a reference price the builder refuses places no entry (the 180 s timeout then skips the day) or no close (dangling position reported, day failed). Failure modes: an index that moves more than 10% inside 180 s leaves a self-test leg resting, and the day fails at the timeout (loud, as before). Rollback: revert the PR. Observability: `tv_paper_fills_resting_total`. Limits: paper fills at the mark, not at a simulated order book; the 10% band is a paper self-test price, never sent to an exchange (index orders are dry run only).
Design (H2): the SEBI retail-algo framework and Dhan's MPP conversion make MARKET orders a liability for a self-trader, so the system sends only LIMIT orders and (H2b) the OMS refuses the rest. A LIMIT needs a price on the exchange tick: the paper self-test uses the index mark (dry run only), the pipeline uses the last traded price (always a traded, on-tick price), and the exit layer takes the caller's price.
Edge cases: a mark or fill price that is NaN, infinite, zero or negative (no order sent, counted); an entry order unknown to the OMS (no close); a CloseAll that must still cancel actives when the close is refused.
Failure modes: a refused close leaves the position open rather than closing at a guessed price (dormant today: the exit layer is locked off and the pipeline has no production caller); a LIMIT at the last traded price may rest unfilled where a MARKET order would have filled.
Rollback: revert the PR; no data or config change.
Observability: `tv_exit_close_refused_total{reason}`, `tv_pipeline_entry_refused_total{reason}` (both on paths with no production caller today), the existing `tv_paper_selftest_total{outcome}` (new outcome `close_price_unknown`), coded `EXIT-ORDER-01` (log-sink-only).
Limits (H2a): how Dhan treats a LIMIT exactly at the last traded price live is Unknown (paper fills at the mark regardless of the limit); the pipeline has no tick-size source, so it uses only the LTP itself, never an offset price.

Design (H1): the vendor/SEBI figure is "10 orders per second"; how NSE counts a second (rolling or calendar) is Unknown, so the window is closed and rolling, which is at least as strict as either. The GCRA keeps its smoothing.
Edge cases: equal or backward instants (monotonic clock; `saturating_sub` reads them as inside the window, so it refuses: errs toward refusal); a poisoned lock (recovered with `into_inner`, never panics); N above 64 (window off, documented); exactly 1 s after the oldest admission (refused; 1 s + 1 ns admitted).
Failure modes: a refused order is refused exactly as before (`OmsError::RateLimited`, same counters); more refusals only for bursts that the old limiter let past 10 in one second.
Rollback: revert the PR; no data or config change.
Observability: unchanged series (`tv_rate_limiter_allowed_total`, `tv_rate_limiter_denied_total`). The denied counter is now cached like the allowed one: the refusal path allocated 32 B per refused order with the production recorder (pre-existing, found by the new DHAT window, fixed here).
Limits (H1, stated, not fixed): the window counts orders when they leave the OMS, not when Dhan receives them, so uneven network delay can still put 11 inside one of Dhan's seconds (a margin, e.g. a cap of 9, is an owner decision); the window is per process and starts empty after a restart; a sliced or super/OCO exit order spends one slot for several exchange orders (exit layer locked off); the order-status probe `verify_order_execution` spends a slot like an order (as it did under the GCRA).
