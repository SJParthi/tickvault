# Implementation Plan: C2 — Converge Dhan+Groww seal routing into one shared `route_seal`

**Status:** APPROVED
**Date:** 2026-06-27
**Approved by:** coordinator (C2 task)
**Crate:** `tickvault-app`

> **Guarantee matrix:** this item cross-references the canonical 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (per-wave-guarantee-matrix.md). This is a **behavior-preserving** pure refactor
> — no new tick-drop path, no new hot-path allocation, no DEDUP-key change, no
> new error code, no observability change. The 7-row resilience demand
> ("Zero ticks lost", "O(1) latency") is preserved by construction: the shared
> function is the UNION of the two existing seal-routing closure bodies with the
> per-feed differences hoisted into explicit parameters; each feed's emitted
> output (counters / drops / `BufferedSeal` fields / D1-drop / pct-stamp) is
> byte-identical to today.

## Design

Today the per-seal routing logic is duplicated as two inline `on_seal` closures
passed to `MultiTfAggregator::consume_tick`:

- **Dhan** — `crates/app/src/main.rs::spawn_engine_b_aggregator` (the
  `|tf, mut state| { … }` closure, ~lines 3631-3676): DROPS the `TfIndex::D1`
  seal; looks up `prev_day_cache` + overrides `refs.prev_day_close` from
  `state.prev_day_close` + calls `stamp_seal_pct_fields`; computes
  `close_pct_nonzero`; builds `BufferedSeal::new(…, Feed::Dhan)`; sends via
  `global_seal_sender()`; on send-error increments `tv_seal_mpsc_dropped_total`
  (NO `feed` label) + `heartbeat.record_drop()`; on success increments
  `tv_aggregator_seals_emitted_total` + `heartbeat.record_emit()` + (if
  close_pct_nonzero) `tv_aggregator_close_pct_nonzero_total` +
  `heartbeat.record_close_pct_nonzero()`.

- **Groww** — `crates/app/src/groww_bridge.rs` inline `on_seal` (~lines 518-534):
  NO D1 drop; NO pct-stamp; builds `BufferedSeal::new(…, Feed::Groww)`; sends via
  `global_seal_sender()`; on send-error increments
  `tv_seal_mpsc_dropped_total{feed="groww"}` (HAS `feed` label); on success, if
  `tf == TfIndex::M1`, calls `feed_health.record_candle(Feed::Groww)`.

**Extraction:** a new `crates/app/src/seal_routing.rs` with a single
`pub fn route_seal(...)` that is the UNION of both bodies. The per-feed
differences become explicit parameters carried in a borrowed, `Copy`,
zero-alloc `SealRouteParams<'a>`:

| Param | Dhan value | Groww value | Controls |
|---|---|---|---|
| `feed: Feed` | `Feed::Dhan` | `Feed::Groww` | `BufferedSeal.feed` + the drop-counter label (Dhan = no label, Groww = `feed=groww`) — byte-identical to today |
| `drop_d1: bool` | `true` | `false` | the `if tf == D1 { return }` early-return |
| `prev_day_cache: Option<&PrevDayCache>` | `Some(&cache)` | `None` | the lookup + `prev_day_close` override + `stamp_seal_pct_fields` |
| `heartbeat: Option<&AggregatorHeartbeatCounters>` | `Some(&hb)` | `None` | the Dhan emit/drop/close-pct heartbeat + `tv_aggregator_*` counters |
| `feed_health_on_m1: Option<&FeedHealthRegistry>` | `None` | `Some(&fh)` | the Groww `record_candle` on the M1 seal |

`route_seal` signature (zero-alloc — only references + `&'static str` labels, no
`format!`/`String`/`Vec`/`clone`):

```rust
pub fn route_seal(
    params: SealRouteParams<'_>,
    security_id: u32,
    exchange_segment_code: u8,
    tf: TfIndex,
    mut state: LiveCandleState,
    sender: &mpsc::Sender<BufferedSeal>,
)
```

The two call sites resolve `global_seal_sender()` once (as today: the
`let Some(sender) = global_seal_sender() else { return/continue }` guard stays at
the call site so the `&'static` borrow lifetime is unchanged) and pass `sender`
in. The Dhan call site passes `drop_d1=true, prev_day_cache=Some, heartbeat=Some,
feed_health_on_m1=None`; the Groww call site passes
`drop_d1=false, prev_day_cache=None, heartbeat=None, feed_health_on_m1=Some`.
The result is byte-identical to today for BOTH feeds.

**Why a shared `mut state` param (not `&mut`):** the closure receives
`LiveCandleState` by value (`F: FnMut(TfIndex, LiveCandleState)`), and the Dhan
path mutates it via `stamp_seal_pct_fields(&mut state, refs)`. `route_seal` takes
`state` by value (`mut`) and `BufferedSeal::new` consumes it — identical move
semantics to both current closures.

## Edge Cases

- `tf == D1` with `drop_d1=true` (Dhan): early-return BEFORE build/send — no seal
  emitted, no counter touched. Identical to today's `if tf == TfIndex::D1 { return; }`.
- `tf == D1` with `drop_d1=false` (Groww): the D1 seal is routed (Groww has never
  dropped D1). Preserved.
- `prev_day_cache=None` (Groww): no lookup, no pct-stamp — `state` is sent as-is.
  Preserved.
- `prev_day_cache.lookup` miss (Dhan): `.unwrap_or_default()` → zero refs, then
  `refs.prev_day_close = state.prev_day_close` override — identical to today.
- Send-error (mpsc full): Dhan → unlabeled drop counter + heartbeat drop; Groww →
  `feed=groww` labeled drop counter. The drop-counter label is selected by
  `params.feed` inside `route_seal` to reproduce each EXACTLY. No panic — the
  ring→spill→DLQ chain downstream is the durable absorber (unchanged).
- `tf != M1` with `feed_health_on_m1=Some` (Groww): no `record_candle` — matches
  today's `else if tf == TfIndex::M1` guard.

## Failure Modes

- This is a pure refactor: NO new failure mode is introduced. The only seal-loss
  path remains the AGGREGATOR-DROP-01 ring→spill→DLQ exhaustion (storage crate,
  untouched here). The `tv_seal_mpsc_dropped_total` increment on a full mpsc is
  preserved per-feed exactly.
- If `route_seal` ever diverged from a call site (regression), the new
  source-scan ratchet (`seal_routing_convergence_guard`) fails the build: it
  asserts BOTH call sites invoke `route_seal` and NEITHER hand-builds the routing
  inline outside `seal_routing.rs`.

## Test Plan

- Unit test `route_seal` in `seal_routing.rs` (pub-fn-test-guard): drive
  `route_seal` for both feeds with a real bounded `mpsc::channel`, assert the
  `BufferedSeal` reaches the receiver with the correct `feed` / `tf` / ids, and
  assert the D1-drop param suppresses the D1 seal (Dhan) while a non-D1 TF is
  routed; Groww (drop_d1=false) routes D1.
- Source-scan ratchet `crates/app/tests/seal_routing_convergence_guard.rs`:
  asserts exactly ONE seal-routing body — both `main.rs` (Dhan) and
  `groww_bridge.rs` (Groww) call `route_seal`, and neither file hand-builds the
  inline routing (`BufferedSeal::new(` + `global_seal_sender(` + D1-drop pattern)
  outside `seal_routing.rs`. Mirrors `test_run_groww_bridge_routes_seals_through_shared_writer`.
- `cargo test -p tickvault-app`, `cargo fmt`, `cargo clippy -p tickvault-app -- -D warnings`.
- banned-pattern-scanner, pub-fn-test-guard, per-item-guarantee-check all green.
- Behavior-preservation confirmation: re-read the diff and confirm Dhan + Groww
  seal outputs (counters / drop labels / keep / `BufferedSeal` fields) are
  byte-identical to before — pure refactor, no behavior change.

## Rollback

Single-commit, self-contained: revert the commit to restore both inline
closures. No schema change, no DEDUP key change, no config flag, no migration.
`route_seal` is internal to `tickvault-app`; nothing else depends on it.

## Observability

NONE changed. The exact same counters fire from the same conditions:
`tv_seal_mpsc_dropped_total` (Dhan unlabeled / Groww `feed=groww`),
`tv_aggregator_seals_emitted_total`, `tv_aggregator_close_pct_nonzero_total`
(Dhan only, via `heartbeat`), and `FeedHealthRegistry::record_candle` (Groww M1
only). No new metric, no new `error!`/`warn!`/`info!`, no new ErrorCode. The
shared function relocates WHERE the emit happens, never WHETHER/HOW MUCH.

## Plan Items

- [x] Write `crates/app/src/seal_routing.rs` with `SealRouteParams` + `route_seal`
      (union of both bodies, per-feed params, zero-alloc) + `#[test]`
  - Files: crates/app/src/seal_routing.rs, crates/app/src/main.rs (mod decl)
  - Tests: test_route_seal_dhan_routes_non_d1_and_drops_d1, test_route_seal_groww_routes_all_tfs
- [x] Rewire the Dhan call site (`spawn_engine_b_aggregator`) to call `route_seal`
  - Files: crates/app/src/main.rs
  - Tests: seal_routing_convergence_guard
- [x] Rewire the Groww call site (`groww_bridge.rs` inline `on_seal`) to call `route_seal`
  - Files: crates/app/src/groww_bridge.rs
  - Tests: seal_routing_convergence_guard
- [x] Source-scan ratchet asserting exactly ONE seal-routing body
  - Files: crates/app/tests/seal_routing_convergence_guard.rs
  - Tests: test_dhan_call_site_uses_route_seal, test_groww_call_site_uses_route_seal, test_no_inline_seal_routing_outside_seal_routing_module

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan D1 seal | dropped (drop_d1=true), no counter touched |
| 2 | Dhan M1 seal, mpsc OK | `BufferedSeal{feed=Dhan}` sent + seals_emitted++ + heartbeat emit |
| 3 | Dhan seal, mpsc full | unlabeled `tv_seal_mpsc_dropped_total`++ + heartbeat drop |
| 4 | Groww D1 seal | routed (drop_d1=false) |
| 5 | Groww M1 seal, mpsc OK | `BufferedSeal{feed=Groww}` sent + `record_candle(Groww)` |
| 6 | Groww seal, mpsc full | `tv_seal_mpsc_dropped_total{feed=groww}`++ |
