# Dry-Run Order Runtime — Error Codes, WAL Confirm Semantics & Pre-Live Follow-Ups

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `websocket-connection-scope-lock.md` (the 2026-07-13 Amendment — the
> order-update WS stays inside `dhan_rest_stack`) > this file.
> **Operator authorization:** coordinator directive 2026-07-14 (cluster A —
> order-side readiness audit): revive the dormant order machinery DRY-RUN
> ONLY — fill bridge, unrealized-P&L lot_size fix, WAL drain + conditional
> confirm, Groww mark tap, alert sinks. NO live orders.
> **Companion code:** `crates/app/src/order_runtime.rs` (the single-owner
> actor), `crates/app/src/oms_wiring.rs`, `crates/app/src/dhan_rest_stack.rs`
> Phase 5a, the `groww_bridge.rs` mark tap,
> `crates/trading/src/oms/{engine,types}.rs` (`FillEvent`),
> `crates/trading/src/risk/engine.rs` (`evaluate_daily_loss_halt`).
> **Companion plan:** `.claude/plans/active-plan-order-runtime-dryrun.md`.
> **Ratchets:** `crates/app/tests/order_runtime_spawn_site_guard.rs`,
> `dhan_rest_stack.rs::test_rest_stack_wires_order_runtime`,
> `crates/app/tests/wal_replay_confirm_symmetry_guard.rs` §C,
> `crates/app/tests/dhat_mark_forward.rs`, `crates/app/benches/order_gate.rs`
> (+ `order_gate_mark_forward` in `quality/benchmark-budgets.toml`).

---

## §0. What this is (one paragraph)

With `[order_runtime].enabled = true` (base.toml ON; serde DEFAULT stays
OFF — an absent section boots byte-identical to Phase A), the dhan-OFF REST
stack's Phase 5a replaces the PR-C1 discard drain with a supervised
SINGLE-OWNER actor that owns an `OrderManagementSystem` (**dry_run
hard-true — no HTTP order call can exist**) + a `RiskEngine`. It consumes
the order-update broadcast (Source=P filter), bridges fills into the risk
book via the widened `handle_order_update → Result<Option<FillEvent>, _>`,
consumes Groww marks through a bounded mpsc fed by an
`Arc<AtomicBool>`-gated per-tick tap, synthesizes next-mark paper fills,
evaluates the mark-to-market daily-loss halt, runs an honest reconcile
heartbeat ("broker reconcile SKIPPED — dry-run" + the REAL local
Σfills==net_lots invariant), sweeps pending paper orders at 15:30 IST,
resets at 16:00 IST, and proves the whole chain once a day via a gated
paper self-test. Zero new QuestDB tables, zero new ErrorCode variants, zero
new NotificationEvent variants.

## §1. Error-code usage per emit site (ZERO new variants)

| Emit site | Level + code |
|---|---|
| Reconcile mismatch / local Σfills≠net_lots divergence / severe receiver lag / mark-apply anomaly (reconcile class) | `error!` OMS-GAP-02 |
| Orphan fill-carrying update on an empty (fresh) book — the WAL-replay-across-restart shape | `warn!` OMS-GAP-02 + `tv_oms_orphan_fill_updates_total` |
| Order rejected (OMS alert sink) / partial-lot fill remainder floored (engine) | `error!` OMS-GAP-01 |
| Circuit breaker opened (sink) | `error!` OMS-GAP-03 |
| Rate-limit exhausted (sink) | `error!` OMS-GAP-04 |
| Self-test failure / non-PAPER order id in dry-run / runtime respawn | `error!` OMS-GAP-06 |
| Risk halt (sink + the `trigger_halt` code fix) | `error!` RISK-GAP-01 |
| Sid-segment TRIPWIRE collision (see §3) / sid parse failure | `error!` RISK-GAP-02 |
| WAL confirm DEFERRED (stale live-feed segments staged) | `warn!` WS-REINJECT-01, reason=`confirm_deferred_stale_livefeed` (NON-PAGING — see §2) |

Telegram: the alert sinks map onto the 5 EXISTING NotificationEvent
variants (OrderRejected, CircuitBreakerOpened/Closed, RateLimitExhausted,
RiskHalt). Metrics (all static labels): `tv_order_runtime_up`,
`tv_order_update_events_total{outcome}`, `tv_oms_orphan_fill_updates_total`,
`tv_risk_fills_recorded_total{kind}`, `tv_mark_forward_dropped_total`,
`tv_paper_fills_synthesized_total`, `tv_paper_fills_deferred_total`,
`tv_oms_reconcile_runs_total{mode}`,
`tv_oms_local_reconcile_divergence_total`,
`tv_paper_selftest_total{outcome}`, `tv_wal_confirm_deferred_total`,
`tv_wal_replay_burst_total`, `tv_order_runtime_respawn_total{reason}`,
`tv_order_update_receiver_lagged_total`.

## §2. WAL confirm-defer semantics + the one-time stale-segment archive runbook

The order-update WAL frames staged by main.rs's STAGE-C boot replay were a
Phase-A residual on every dhan-off boot: both drain sites (fast arm + lane)
are dhan-gated, so segments sat in `replaying/` forever, re-globbed each
boot. Phase 5a now drains them into the stack broadcast BEFORE the WS
spawns (FIFO law F4; the runtime subscribed BEFORE the drain, law F5) and
**conditionally** confirms:

- **Confirm** (`ws_frame_spill::confirm_replayed`, WHOLE-DIR) fires iff the
  drain was parse-clean AND `livefeed_frames_replayed == 0`. A whole-dir
  confirm while stale LIVE-FEED frames sit staged would archive them
  un-reinjected — silent tick loss (design F6).
- **Defer** fires ONE coalesced
  `warn!(code = WS-REINJECT-01, reason = "confirm_deferred_stale_livefeed",
  live_feed_frames, parse_errors)` + `tv_wal_confirm_deferred_total`.
  `warn!` DELIBERATELY: WS-REINJECT-01 carries an ERROR-level CloudWatch
  log-filter alarm, and a per-boot page for EXPECTED stale residue is pager
  noise. The frames stay staged — re-replayed next boot, never lost.
- Per-record-type confirm is IMPOSSIBLE (segment files mix record types);
  documented, not worked around.

**One-time operator archive procedure (clears a permanent Defer):** stop
the app off-market; inspect `data/ws_wal/replaying/` — the stale live-feed
segments predate the Dhan retirement (2026-07-13) and their ticks are
already in QuestDB (DEDUP-idempotent replays); move the segment files to
`data/ws_wal/archive/` manually (`mv data/ws_wal/replaying/*.wal
data/ws_wal/archive/`); restart. Every subsequent boot confirms cleanly and
`replaying/` stays empty. (A dhan-ON boot's live-feed re-injection would
also clear them — not applicable while the lane is retired.)

## §3. I-P1-11 tripwire deferral (dated justification, 2026-07-14)

The OMS (`orders` map, order-id keyed) and RiskEngine (`positions` map,
`security_id: u64` keyed) predate the composite-key rule. Rewriting both to
`(security_id, exchange_segment)` touches the frozen-adjacent risk surface
and every call site — deferred. Instead the runtime keeps a FIRST-SEEN
SEGMENT mirror per sid: a fill or mark whose segment_code differs from the
sid's first-seen segment is SKIPPED with `error!` RISK-GAP-02 (a
cross-segment sid collision becomes a LOUD skip, never a silent P&L
merge). Honest bound: the colliding instrument's fills/marks are dropped
for the day — visible, never wrong.
**The composite `(u64, u8)` rewrite is a MANDATORY pre-live follow-up** —
no `dry_run = false` flip without it.

## §4. Scope guard — EXPLICITLY OUT (a violating PR is REJECTED)

1. Strategy/indicator activation — §28 boundary; the runtime never
   constructs IndicatorEngine/StrategyInstance (no signal source exists;
   the only order producer is the gated self-test).
2. Live mode ANYTHING: `enable_live_mode` stays `#[cfg(test)]`; `dry_run`
   hard-true (ratcheted by `ratchet_order_runtime_is_dry_run_hardcoded`);
   no config flag can flip it.
3. A second spawn site (main.rs / fast arm / trading_pipeline /
   groww_bridge) — dual-OMS split-brain; ratcheted by
   `ratchet_order_runtime_spawned_only_from_rest_stack`.
4. `order_audit` / `pnl_audit` QuestDB tables — flagged follow-up (this PR
   adds NO tables).
5. HTTP command endpoints for the runtime — cut (attack surface); the
   heartbeat + metrics are the observability surface.
6. RiskEngine/OMS composite-key rewrite — §3 (mandatory pre-live).
7. Feed toggles, `[feeds.groww.scale]`, GDF — untouched.

## §5. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage:
> dry-run only — the OMS `dry_run` flag is hard-true with the only
> live-mode flip test-gated (build-failing ratchet); fills reach the risk
> book via an in-engine delta computation that is double-count-safe on
> same-status refreshes and duplicate updates (unit-pinned); unrealized
> P&L now multiplies lot_size (the pre-existing understatement bug is
> fixed + finiteness-guarded); the Groww per-tick tap costs ONE Relaxed
> load when disarmed (DHAT ≤1KiB/8 blocks over 10K calls; Criterion
> budget `order_gate_mark_forward` ≤100ns). NOT claimed: marks are
> BEST-EFFORT (a full channel drops the mark, counted — the next tick
> supersedes it; positions stay exact); the broker-side reconcile is
> SKIPPED in dry-run (the heartbeat says so honestly — the REAL invariant
> checked is the local Σfills==net_lots mirror); a runtime respawn starts
> a FRESH paper book (in-RAM state; replayed updates surface as loud
> orphan warns, never silent fills); cross-segment sid collisions are
> loudly SKIPPED, not composite-keyed (§3)."

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/app/src/order_runtime.rs` or `crates/app/src/oms_wiring.rs`
- `crates/app/src/dhan_rest_stack.rs` (Phase 5a)
- `crates/trading/src/oms/engine.rs` / `types.rs` (FillEvent),
  `crates/trading/src/risk/engine.rs`
- `crates/common/src/config.rs` (`OrderRuntimeConfig`) or `config/base.toml`
  `[order_runtime]`
- Any file containing `spawn_order_runtime`, `MarkForwarder`, `FillEvent`,
  `confirm_decision`, `tv_order_runtime_up`, or
  `tv_wal_confirm_deferred_total`
