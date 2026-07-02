# Implementation Plan: Live-Feed Health SP5.1 — wire Dhan drops (close the false-OK)

**Status:** VERIFIED
**Date:** 2026-06-22
**Reconciled:** 2026-07-02 — audit verified all 3 items shipped on origin/main; checkboxes were stale.
**Approved by:** Parthiban (standing approval — "resolve issues and merge, then go ahead"; no-false-OK is mandatory)

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all **15 rows** of the
100% guarantee matrix and all **7 rows** of the resilience demand matrix apply.
SP5.1 specifics: the drop record is on the COLD error path (only fires on
catastrophic channel-full / writer-dead), so the per-frame hot path is untouched;
1 relaxed atomic; additive (`feed_health = None` → no-op). **Honest 100% claim:**
100% inside the tested envelope — closes the connected+fresh-but-dropping false-OK
for Dhan; ratcheted by the new storage test + the flipped wiring guard.

## Design

SP5 left a known HIGH: Dhan's `drops>0 → Degraded` branch was dead, so a Dhan feed
connected + fresh but DROPPING ticks under extreme backpressure read `ok`, not
`degraded`. SP5.1 wires the drop signal at the **terminal Dhan loss site** — the
WAL frame spill, the single chokepoint where a Dhan live-feed frame is actually
lost (it emits `tv_ticks_lost_total`).

1. `WsFrameSpill` (`crates/storage/src/ws_frame_spill.rs`) gains an
   `Option<Arc<FeedHealthRegistry>>` field + a `with_feed_health(self, …) -> Self`
   **builder setter** (keeps `new()` signature stable, so the existing prod call
   site + both test constructors are unchanged). In BOTH drop-critical arms of
   `append_with_seq` (the `TrySendError::Full` and `TrySendError::Disconnected`
   arms), AFTER the existing counters, if `ws_type == WsType::LiveFeed` and the
   registry is `Some`, call `feed_health.record_drops(Feed::Dhan, 1)`.
2. `crates/app/src/main.rs` (~line 706) wires the registry in via the setter:
   `Some(Arc::new(spill.with_feed_health(Some(Arc::clone(&feed_health)))))`.

Result: a dropped Dhan live-feed frame → `drops_total > 0` → `classify()` returns
`Degraded` → `GET /api/feeds/health` flips Dhan 🟡. Closes the SP5 HIGH.

## Edge Cases

- `feed_health = None` → both new calls skipped; byte-identical to today (existing
  prod call site keeps working; the two `#[cfg(test)]` constructors are untouched
  because `new()` signature is unchanged).
- `ws_type != WsType::LiveFeed` (OrderUpdate / future depth) → NOT recorded: only
  the Dhan market-data feed maps to `Feed::Dhan`. A dropped order-update frame is
  not a market-data loss.
- The drop arms are the COLD error path (only on channel-full / writer-dead) — the
  hot `Ok(()) → Spilled` path is untouched, so no per-frame hot-path cost.
- Future second live feed via the WAL: today Groww uses its own file-based bridge,
  NOT `WsFrameSpill`, so `WsType::LiveFeed` == Dhan. If a future Groww WAL reuses
  `WsFrameSpill`, the `ws_type → Feed` mapping must extend (noted in a code comment).
- `set_connected`/freshness verdicts unchanged: a drop makes it `Degraded`, which
  is correctly LESS severe than `Down` (the feed is still alive, just lossy).

## Failure Modes

- `record_drops` is a relaxed-atomic `fetch_add` + `mark_instrumented` store —
  cannot fail, no I/O, no alloc, no lock.
- No change to the WAL durability path itself (the drop already happened; we only
  also count it for health). The zero-tick-loss chain semantics are unchanged.
- No new `unwrap`/`expect`/`unsafe`.

## Test Plan

- `crates/storage` unit test: construct `WsFrameSpill` with a dead writer + a
  `FeedHealthRegistry`, `append(WsType::LiveFeed, …)` → `Dropped`; then
  `registry.snapshot(Feed::Dhan, enabled, lane, market_open=true, …)` with
  connected+fresh → `Degraded` and `drops_total >= 1`. Plus: an OrderUpdate drop
  does NOT record a Dhan drop.
- Update `crates/app/tests/sp5_dhan_feed_health_wiring_guard.rs`: flip
  `test_sp5_1_drops_dimension_pending_is_documented` → assert `record_drops(`Feed::Dhan`)`
  is now wired in `ws_frame_spill.rs`; update the module doc.
- `cargo test -p tickvault-storage --lib`, `-p tickvault-common`, `-p tickvault-app` green.
- banned-pattern + pub-fn-test-guard + plan-gate + per-item-guarantee-check PASS.
- Adversarial 3-agent review (hot-path + security + hostile) on the WAL-chain change
  BEFORE impl AND on the diff after.

## Rollback

Pure additive: revert = remove the `feed_health` field + `with_feed_health` setter
+ the 2 `record_drops` calls in the drop arms + the 1 wiring line in main.rs. No
schema, no data, no behaviour coupling; `new()` signature never changed.

## Observability

Dhan drops now surface as `Degraded` on `GET /api/feeds/health` (closing the
false-OK). The existing `tv_ticks_lost_total` + `tv_ws_frame_spill_drop_critical`
counters + the AGGREGATOR-DROP-01 / WS-SPILL-02 Criticals are unchanged — SP5.1
adds the per-feed health signal alongside them, not instead of them.

## Plan Items

- [x] `WsFrameSpill`: add `feed_health` field + `with_feed_health` setter; `record_drops(Feed::Dhan, 1)` in both drop arms for `WsType::LiveFeed` — DONE on main: `with_feed_health` in `crates/storage/src/ws_frame_spill.rs`
  - Files: crates/storage/src/ws_frame_spill.rs
  - Tests: test_live_feed_drop_records_dhan_feed_health, test_order_update_drop_does_not_record_dhan
- [x] Wire `Some(feed_health)` into `WsFrameSpill` at boot — DONE on main: wired in `crates/app/src/main.rs`
  - Files: crates/app/src/main.rs
  - Tests: (covered by the flipped wiring guard)
- [x] Flip the SP5.1 pending guard → drops-now-wired; update module doc — DONE on main: `test_sp5_1_drops_dimension_wired_in_spill` in `crates/app/tests/sp5_dhan_feed_health_wiring_guard.rs`
  - Files: crates/app/tests/sp5_dhan_feed_health_wiring_guard.rs
  - Tests: test_sp5_1_drops_dimension_wired_in_spill

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan connected + fresh + a live-feed frame dropped (market hrs) | verdict `degraded`, drops_total>0 |
| 2 | Dhan connected + fresh + no drops | verdict `ok` (unchanged) |
| 3 | OrderUpdate frame dropped | Dhan drops NOT incremented |
| 4 | `feed_health = None` | byte-identical to today |
| 5 | Drop happens (cold error path) | hot Spilled path untouched, O(1) |
