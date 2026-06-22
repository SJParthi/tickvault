# Implementation Plan: Live-Feed Health SP5 — wire Dhan per-tick freshness

**Status:** APPROVED
**Date:** 2026-06-22
**Approved by:** Parthiban (standing approval — "go ahead dude as per the plan", serial SP sequence)

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all **15 rows** of the
100% guarantee matrix and all **7 rows** of the resilience demand matrix apply to
every item in this plan. SP5-specific bindings:
- **Code/testing coverage:** classify reorder + registry transition tests + the
  source-scan wiring guard (`sp5_dhan_feed_health_wiring_guard.rs`); core 2149 /
  app 640 / feed_health 19 green.
- **Performance / O(1):** 3 relaxed atomics per tick, zero-alloc, lock-free
  (hot-path-reviewer CLEAR); `feed_health = None` keeps the Dhan hot path
  byte-identical.
- **Zero ticks lost / WS resilience:** additive only — no new tick-drop path, no
  change to ring→spill→DLQ or `SubscribeRxGuard`; the C1 fix removes a false-RED,
  never suppresses a real fault.
- **Uniqueness/dedup:** per-feed atomic-array isolation (I-P1-11 per-feed slot);
  no QuestDB key touched.
- **Monitoring/logging/alerting/audit/review:** the verdict surfaces on
  `GET /api/feeds/health`; adversarial 3-agent review run BEFORE impl (hot-path
  CLEAR, security 1 MEDIUM applied, hostile CRITICAL C1 fixed).

**Honest 100% claim:** 100% inside the tested envelope, with ratcheted regression
coverage (`sp5_dhan_feed_health_wiring_guard.rs` + the classify/registry tests);
`feed_health = None` keeps the Dhan hot path byte-identical, and beyond the
envelope an un-wired feed honestly reports `unknown` (never a false green or red).

## Design

SP1–SP4 built the truthful per-feed health engine + registry + `GET /api/feeds/health`,
and wired the **Groww** lane. Dhan currently reports `unknown` (its record-sites
aren't wired). SP5 wires **Dhan**'s two signals into the SAME shared
`FeedHealthRegistry` — **additively, O(1), without changing any existing behaviour**:

1. **Per-tick freshness** — in `run_tick_processor` (core), at the EXISTING
   no-tick-watchdog heartbeat block (`tick_processor.rs` ~line 1005), also call
   `feed_health.record_tick(Feed::Dhan, ist_nanos)` where
   `ist_nanos = received_at_nanos + IST_UTC_OFFSET_NANOS`. The offset is REQUIRED:
   `received_at_nanos` is UTC nanos (`current_received_at_nanos`), but the
   endpoint's age math uses IST nanos (`Utc::now()+IST`) — same convention as the
   SP4 Groww Q2 fix. Recording UTC would make every Dhan tick read ~5.5h in the
   future → age saturates to 0 → permanent false-fresh. The registry param is a
   NEW `Option<Arc<FeedHealthRegistry>>` added after `tick_heartbeat`; `None`
   keeps every existing test/caller unchanged.

2. **Connected state** — in `spawn_pool_watchdog_task` (app, `main.rs` ~6109), at
   the EXISTING `health.set_websocket_connections(active)` site (~line 6146), also
   call `feed_health.set_connected(Feed::Dhan, active > 0)`. The watchdog already
   computes `active` (count of `Connected` pool sockets) every 5s. New param is a
   NEW `Option<Arc<FeedHealthRegistry>>`; `None` is a no-op.

3. **Boot wiring** — thread `Some(Arc::clone(&feed_health))` (the existing binding,
   `main.rs:250`) into BOTH `run_tick_processor` calls (fast-boot ~1577, slow-boot
   ~3609) and BOTH `spawn_pool_watchdog_task` calls (~1643, ~3686).

**Why this honours the architecture lock:** only feed-specific tick FETCH stays
feed-specific; the health verdict/registry/endpoint are the ONE common engine.
SP5 adds Dhan's two record-sites to that common engine — no parallel logic.

## Edge Cases

- `feed_health = None` (unit tests, any caller not passing it): both new calls are
  skipped via `if let Some(ref fh)` — byte-identical to today's behaviour.
- Dhan UTC→IST clock convention: covered by the `+ IST_UTC_OFFSET_NANOS` add
  (ratcheted by an assertion test on the recorded value's plausibility window).
- `active == 0` (all sockets down): `set_connected(Dhan, false)` → verdict `Down`
  (correct, not false-OK). `active > 0` but ticks silent >30s in market hours →
  still `Down` via the existing last-tick-age gate (set_connected alone never
  forces OK).
- Groww unaffected: per-feed array isolation (I-P1-11 per-feed slot) — recording
  Dhan never touches Groww's slot.
- Fast-boot path: passes `Some(feed_health)` too, so a recovery boot still reports
  Dhan truthfully.

## Failure Modes

- Registry write fails: impossible — Relaxed atomic stores never error; no I/O.
- Hot-path cost: `record_tick` = `feed.index()` (match) + 2 `fetch_add`/`store` +
  1 `mark_instrumented` store = 3 relaxed atomics, zero-alloc, O(1). Sits next to
  the existing heartbeat store (same class). No new allocation, no lock, no syscall.
- Wrong clock (UTC vs IST): mitigated by the explicit offset + a value-window test.
- Scope/compile: `Feed` + `FeedHealthRegistry` live in `tickvault_common` which
  `core` and `app` already depend on.

## Test Plan

- `crates/core` — new unit test: `run_tick_processor` with a `Some(registry)` records
  a Dhan tick whose recorded ts is in the plausible IST window and bumps
  `ticks_total`; with `None` the registry is untouched. (Reuse the existing
  test-frame harness + `TEST_RECEIVED_AT_OVERRIDE_NANOS`.)
- `crates/core` — unit test: recorded value == `received_at + IST_UTC_OFFSET_NANOS`
  (pins the clock convention so a future edit can't regress to UTC).
- `crates/app` — source-scan wiring guard (extends the existing health-counter
  guard pattern): `spawn_pool_watchdog_task` calls `set_connected(Feed::Dhan, …)`
  next to `set_websocket_connections`, and both `run_tick_processor` calls pass
  `Some(feed_health)`.
- `cargo test -p tickvault-core --lib`, `-p tickvault-app --lib` green.
- banned-pattern + pub-fn-test-guard (118 stable) + plan-gate PASS.
- Adversarial 3-agent review (hot-path + security + hostile) on the DESIGN before
  code AND on the diff after.

## Rollback

- Pure additive: every new param defaults to `None`/no-op. Revert = drop the 2
  record-site lines + restore the 4 call sites to their prior arg lists + remove
  the param. No data migration, no schema change, no behaviour coupling.
- Dhan hot path is unchanged when `feed_health = None`; with `Some`, the only added
  work is 3 relaxed atomics — removing them restores byte-identical hot path.

## Observability

- The wired signals surface directly on `GET /api/feeds/health`: Dhan flips from
  `unknown` → `ok`/`down` (connected + last-tick age) and `ticks_total` climbs.
- No new Prometheus counter needed (the registry IS the observability surface read
  by the endpoint); the existing `tv_websocket_connections` gauge is untouched.
- Honest envelope (corrected after the after-impl hostile review, HIGH): SP5 makes
  Dhan's light truthful for the **connect + freshness** dimensions. The **drops**
  dimension is NOT yet wired for Dhan (`record_drops(Dhan)` belongs at the storage
  terminal-loss site `ws_frame_spill::append`; `record_candle(Dhan)` at the Dhan
  seal site — both storage changes needing their own review). **Known gap → SP5.1:**
  until then, a Dhan feed that is connected + fresh but dropping ticks under extreme
  backpressure (QuestDB down + ring + spill + DLQ all full) would read `ok`, not
  `degraded`. That catastrophic scenario is independently alarmed by
  AGGREGATOR-DROP-01 / WS-SPILL-02 / `tv_ticks_lost_total` / QuestDB-disconnect
  Criticals, so the operator is never blind to the loss — but the feed light should
  still flip `degraded`, which SP5.1 wires. The gap is pinned explicitly by
  `sp5_dhan_feed_health_wiring_guard::test_sp5_1_drops_dimension_pending_is_documented`
  so the dead `drops>0` branch for Dhan is never silently forgotten. SP5 remains a
  strict improvement over the prior `unknown` (no connect/freshness false-OK or
  false-RED; the drops dimension is explicitly pending, not falsely claimed green).

## Adversarial 3-agent review (BEFORE impl)

- **hot-path-reviewer: CLEAR** — `record_tick` is 3 relaxed atomics on fixed arrays, O(1), zero-alloc, lock-free; the `if let Some` branch mirrors the existing heartbeat; no banned pattern. IST-offset add is the correct receive-time semantic.
- **hostile (general-purpose): CRITICAL C1 (fixed below)** — the pool watchdog runs unconditionally from boot, so SP5's `set_connected(Dhan, false)` fires PRE-MARKET (active=0 before the pool connects); `classify()` checks `!connected → Down` BEFORE the market-open gate → Dhan shows a false `Down` every morning 08:30→connect. Also HIGH H1 (same root: `Unknown → Down` skips the healthy window). Cleared the clock bug (#1 — Dhan & Groww both record `received_at(UTC)+IST`, consistent with the endpoint), false-OK (#2), double-record (#4), scope (#6). MEDIUM M1 (record fires before the persist-window gate — acceptable for a *liveness* signal, symmetric with the existing heartbeat; documented, no code change).
- **security-reviewer:** (pending — incorporated before push)

### C1/H1 FIX (in `classify`, `crates/common/src/feed_health.rs`)
Restructure so the **market-open gate precedes the connected check**. Outside market
hours a disconnected/silent feed is EXPECTED (the WS pool sleeps-until-open by design)
→ NOT a fault. Order becomes: Disabled → Degraded(lane) → Unknown(instrumented) →
Degraded(drops, any time) → **if !market_open return Ok "market closed — idle is normal"** →
[market hours] !connected → Down → last-tick-age None/stale → Down → Ok "live". This
keeps every existing test green (they use `market_open: true`) and adds
`test_disconnected_outside_market_hours_is_not_down`.

## Plan Items

- [ ] **C1/H1 fix:** reorder `classify()` so market-open gate precedes the connected check — pre-market disconnected Dhan is `Ok "idle"`, not a false `Down`
  - Files: crates/common/src/feed_health.rs
  - Tests: test_disconnected_outside_market_hours_is_not_down
- [ ] Add `feed_health: Option<Arc<FeedHealthRegistry>>` param to `run_tick_processor`; record Dhan tick at the heartbeat block with IST offset
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_run_tick_processor_records_dhan_feed_health, test_dhan_feed_health_uses_ist_clock
- [ ] Add `feed_health: Option<Arc<FeedHealthRegistry>>` param to `spawn_pool_watchdog_task`; `set_connected(Dhan, active>0)` at the counter site
  - Files: crates/app/src/main.rs
  - Tests: pool_watchdog_sets_dhan_connected (source-scan wiring guard)
- [ ] Thread `Some(feed_health)` into both tick_processor + both watchdog call sites
  - Files: crates/app/src/main.rs
  - Tests: run_tick_processor_passes_feed_health (source-scan wiring guard)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan connected + recent tick, market open | verdict `ok`, ticks_total climbing |
| 2 | Dhan pool all down | `set_connected(false)` → verdict `down` |
| 3 | Dhan connected but silent >30s in market hours | verdict `down` (last-tick-age gate) |
| 4 | `feed_health = None` (unit test / legacy caller) | byte-identical to today, no record |
| 5 | Recorded ts | `== received_at + IST_UTC_OFFSET_NANOS` (IST, not UTC) |
| 6 | Groww slot | untouched by Dhan recording (per-feed isolation) |
