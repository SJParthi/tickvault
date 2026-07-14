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
ONLY from `crates/app/src/dhan_rest_stack.rs` Phase 5a (the dhan-OFF
arm) replacing the discard drain — structural dual-OMS exclusion: the
rest stack is the dhan-off arm; `trading_pipeline` (which owns its own
OMS+Risk) is dhan-ON only. Config-gated `[order_runtime]` (serde
default OFF = byte-identical boot; `config/base.toml` opts in). The 12
normative resolutions from the judge synthesis:

1. **Module + spawn ordering law** (dhan_rest_stack Phase 5a):
   `broadcast::channel` → `order_update_sender.subscribe()` →
   `spawn_order_runtime(..)` →
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

- [ ] A1 — order_runtime.rs actor: supervised single-owner task, select! arms (order-update / marks / reconcile / 16:00 reset / 15:30 sweep / self-test), NotifierAlertSink
  - Files: crates/app/src/order_runtime.rs, crates/app/src/oms_wiring.rs
  - Tests: test_traded_update_reaches_risk_engine_net_lots_nonzero, test_alert_sink_event_mapping, test_halt_fires_risk_halt_once_per_episode, prop_fill_mirror_matches_risk_net_lots
- [ ] A2 — FillEvent widening of handle_order_update (+ 4-line trading_pipeline graft)
  - Files: crates/trading/src/oms/engine.rs, crates/trading/src/oms/types.rs, crates/trading/src/oms/mod.rs, crates/app/src/trading_pipeline.rs
  - Tests: test_same_status_refresh_applies_delta_not_cumulative, test_duplicate_update_zero_delta_skipped, test_partial_lot_remainder_floors_and_errors, test_fill_sign_from_managed_order_transaction_type, test_segment_char_parse_matrix
- [ ] A3 — ticks→update_market_price gate (marks_wanted AtomicBool + MarkUpdate try_send tap at the groww_bridge seam; channel plumbed in main.rs)
  - Files: crates/app/src/groww_bridge.rs, crates/app/src/main.rs
  - Tests: test_marks_wanted_false_skips_send, test_mark_channel_full_drops_counted_never_blocks, dhat_mark_forward (0 alloc / 10K), Criterion order_gate/mark_forward ≤ 50ns
- [ ] A4 — WAL drain + conditional confirm in dhan_rest_stack Phase 5a (ordering law; wal_spill capture restored)
  - Files: crates/app/src/dhan_rest_stack.rs, crates/app/tests/wal_replay_confirm_symmetry_guard.rs
  - Tests: test_confirm_decision_matrix (4 arms), test_rest_stack_wires_order_runtime, ratchet_order_runtime_spawned_only_from_rest_stack
- [ ] A5 — reconcile scheduler with honest dry-run heartbeat + local Σfills==net_lots invariant
  - Files: crates/app/src/order_runtime.rs
  - Tests: test_dry_run_reconcile_classified_heartbeat_not_ok, test_local_reconcile_divergence_errors
- [ ] A6 — paper filler + once-daily gated self-test + orphan-fill loudness
  - Files: crates/app/src/order_runtime.rs
  - Tests: test_paper_fill_deferred_until_finite_positive_mark, test_terminal_order_never_refilled, test_selftest_single_cycle_latched, test_selftest_refused_on_holiday_and_off_hours, test_orphan_fill_update_warns_and_counts, test_source_n_filtered_empty_tolerated, order_runtime_e2e (crates/app/tests/)
- [ ] A7 — risk P&L lot_size fix + evaluate_daily_loss_halt + trigger_halt code field + sid-segment tripwire
  - Files: crates/trading/src/risk/engine.rs, crates/app/src/order_runtime.rs
  - Tests: test_unrealized_pnl_multiplies_lot_size, test_evaluate_daily_loss_halt_boundary, test_sid_segment_collision_skips_and_errors, test_daily_reset_clears_book_mirror_tripwire_flag_atomically
- [ ] A8 — config section + rule files (order-runtime-dryrun.md; dated notes in ws-reinject-error-codes.md + websocket-connection-scope-lock.md)
  - Files: crates/common/src/config.rs, config/base.toml, .claude/rules/project/order-runtime-dryrun.md, .claude/rules/project/ws-reinject-error-codes.md
  - Tests: config validation unit tests (interval ≥ 60, capacity bounds), serde-default-off test

Other clusters (checked off by their owning sessions' PRs, all referencing THIS plan):

- [ ] C1 — order_audit-family QuestDB persistence revival (feed-in-key DEDUP)
  - Files: crates/storage/src/ (new *_audit_persistence.rs modules)
  - Tests: TBD by owning session (8-element template ratchets)
- [ ] D1 — orphan-watchdog re-homing + expired date-gate re-arm (PR #1545)
  - Files: crates/app/src/main.rs, crates/trading/src/oms/engine.rs, crates/common/src/constants.rs
  - Tests: TBD by cluster D session (watchdog wiring source-scan guard)
- [ ] E1 — exit-order layer (Super Order/OCO wiring, MPP verify, slicing) — serial after A
  - Files: crates/trading/src/oms/ (engine.rs, api_client.rs call sites), crates/app/src/order_runtime.rs
  - Tests: TBD by owning session
- [ ] E2 — portfolio + margin gate (OrderIntent in RiskEngine::check_order; design-only until the operator REST grant)
  - Files: crates/trading/src/risk/engine.rs, crates/trading/src/oms/api_client.rs
  - Tests: TBD by owning session (exit-never-gated invariant test mandatory)

Cluster B (branch claude/dhan-order-error-taxonomy; MINIMAL ADDITION — this
cluster was absent from the original umbrella; flagged in the PR body):

- [ ] B1 — order-path DH/DATA error taxonomy + per-endpoint retry matrix + 805 STOP-ALL latch + DH-904 ladder wiring
  - Files: crates/trading/src/oms/error_taxonomy.rs, api_client.rs, mod.rs, crates/common/src/constants.rs
  - Tests: ratchet_dh904_ladder_constants_and_wiring, ratchet_stop_all_cooldown_is_60s_per_annexure_rule_12, prop_classify_never_panics_and_is_deterministic_on_arbitrary_status_and_body
- [ ] B2 — order-readiness gate (profile/token headroom, fail-closed, live-path-only) + probe refresher (boot spawn = seam handoff)
  - Files: crates/trading/src/oms/order_readiness.rs, engine.rs, types.rs
  - Tests: test_place_order_live_refused_when_no_readiness_installed_zero_http_zero_token_fetch, test_place_order_dry_run_ignores_readiness_gate_byte_identical, test_evaluate_order_readiness_stale_boundary_2100_passes_2101_refuses
- [ ] B3 — ORDER-READY-01 ErrorCode + rule file + OmsAlert DHAN attribution
  - Files: crates/common/src/error_code.rs, .claude/rules/project/order-readiness-error-codes.md, engine.rs
  - Tests: test_order_ready_01_contract, test_all_oms_alert_operator_messages_start_with_dhan_badge

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
| `oms/{api_client.rs error/ladder additions, error_taxonomy.rs, order_readiness.rs}` + engine gate wiring (outside :256-277 / :549-660) | Cluster B (branch claude/dhan-order-error-taxonomy) | consumption-side classifier; SANDBOX + handle_order_update regions untouched; ~35 legacy api_client error sites byte-identical |

Build-lead: the cluster A session. Conflicts on any seam: the
non-owner rebases; never a force-merge over an owner's in-flight PR.
