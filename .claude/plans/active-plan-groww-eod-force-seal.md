# Implementation Plan: IST-midnight/EOD force-seal for the Groww aggregator (parity with Dhan)

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — "fix everything" directive, this session
**Crate scope:** `crates/app` only (`groww_bridge.rs` + `main.rs`)
**Cross-reference:** `.claude/rules/project/per-wave-guarantee-matrix.md` (the full
15-row 100% guarantee matrix + 7-row resilience matrix below cross-reference it;
honest proof rows only).

## Design

**The verified bug (on origin/main `74c368af`):** the Dhan candle engine has an
IST-midnight force-seal task (`crates/app/src/main.rs:3818-3887`, the only
production `force_seal_all` call site, routed `feed: Feed::Dhan`). At IST 00:00
each trading day it calls `aggregator.force_seal_all(...)` so the LAST open bucket
of every one of the 21 timeframes is sealed before day-N state can fuse into
day-(N+1)'s first bar.

The Groww feed holds its OWN separate `MultiTfAggregator` instance
(`crates/app/src/groww_bridge.rs:534`, private field of `GrowwBridgeState`). It
seals ONLY per-tick inside `drain_new_data` → `consume_tick` → `route_seal(...
feed: Feed::Groww ...)` (`groww_bridge.rs:701-725`). There is NO IST-midnight /
EOD force-seal for the Groww aggregator. Consequence: the last open bucket of
each of the 21 Groww timeframes each day is never sealed — it is silently
overwritten by the next day's first tick (the same data-loss the Dhan task
exists to prevent).

**The fix (mirror the Dhan task exactly, Groww-tagged):**

1. **Share the Groww aggregator with a boundary task.** `MultiTfAggregator`
   derives `Clone` and its only state is `inner: Arc<HashMap<...>>` (papaya
   concurrent map) with every method (`consume_tick`, `pre_populate`,
   `seed_cumulative`, `force_seal_all`) taking `&self`. A `.clone()` therefore
   shares the SAME underlying map. So: build the aggregator ONCE in
   `run_groww_bridge`, pass it into `GrowwBridgeState::new`, and `.clone()` it for
   a spawned IST-midnight boundary task. The per-tick path (`drain_new_data`) is
   **unchanged** — it still folds + per-tick-seals through the same instance.

2. **Spawn an IST-midnight force-seal task for the Groww aggregator** that mirrors
   `main.rs:3818-3887` byte-for-byte in structure: loop → sleep
   `secs_until_next_ist_midnight()` → skip on non-trading-day
   (`trading_calendar.is_trading_day_today()`) → resolve `global_seal_sender()` →
   `agg.force_seal_all(|sid, seg, tf, state| route_seal(SealRouteParams{ feed:
   Feed::Groww, drop_d1: false, prev_day_cache: None, heartbeat: None,
   feed_health_on_m1: None }, ...))`. `route_seal` already supports
   `feed: Feed::Groww` (`seal_routing.rs:144-187`). The cell's `force_seal`
   (`aggregator_cell.rs:629-637`) RE-ARMS `armed_for_day_open` and CLEARS
   `last_sealed` internally — exactly as for the Dhan path, so no closure-level
   re-arm is needed (mirrors Dhan, which also delegates re-arm to the cell).

3. **15:30 post-close seal:** the Dhan task does NOT do a separate 15:30 seal — it
   is IST-midnight ONLY. To mirror Dhan exactly (task requirement: "mirror whatever
   Dhan does"), the Groww task is also IST-midnight ONLY. No 15:30 task is added.

4. **Gate on `feeds.groww_enabled` + empty-aggregator no-op.** The task reads the
   shared `feed_runtime.is_enabled(Feed::Groww)` flag each boundary; if Groww is
   disabled it skips. `force_seal_all` over an empty papaya map iterates zero
   entries (cheap no-op) so an empty aggregator is naturally a no-op.

5. **Reuse the SAME helpers as Dhan** — `TradingCalendar` (passed as a new
   `run_groww_bridge` param, `Arc`-shared from `main.rs`), `market_hours::
   secs_until_next_ist_midnight()`. No hardcoded offsets. No `.unwrap()/.expect()/
   println!` in prod.

**Why a SEPARATE Groww task (not reuse the Dhan one):** the two feeds hold
SEPARATE aggregator instances (operator lock `groww-second-feed-scope-2026-06-19.md`
§3: "The Groww aggregator is a SEPARATE `MultiTfAggregator` INSTANCE"). The Dhan
task seals the Dhan instance; the Groww instance needs its own force-seal pass.
Both route through the SAME shared `global_seal_sender` chain, distinguished by the
`feed` tag — the established pattern.

## Edge Cases

| # | Edge case | Behaviour |
|---|---|---|
| 1 | Groww disabled at the IST-midnight boundary | Task checks `feed_runtime.is_enabled(Feed::Groww)` → skips the seal pass (no-op) |
| 2 | Aggregator empty (no Groww ticks ever seen today) | `force_seal_all` iterates 0 entries → 0 seals, no-op |
| 3 | Non-trading-day midnight (weekend/holiday) | `trading_calendar.is_trading_day_today()` is false → skip (mirrors Dhan) |
| 4 | `global_seal_sender()` not installed yet | Task logs a `warn!` and skips that boundary (mirrors Dhan exactly) |
| 5 | D1 timeframe on the Groww force-seal | `drop_d1: false` → D1 IS routed for Groww (matches the Groww per-tick policy; Groww is NOT subject to the Dhan 1d-historical-only rule) |
| 6 | Seal mpsc full at boundary | `route_seal` returns `DroppedFull` → counted via the existing `tv_seal_mpsc_dropped_total{feed=groww}`; ring→spill→DLQ is the durable absorber |
| 7 | Boundary fires mid-tick (race with `drain_new_data`) | papaya map + per-cell mutex make `force_seal` and `consume_tick` safe concurrently; `force_seal` clears `last_sealed` so no cross-day amend |

## Failure Modes

- **Seal-writer down (mpsc full):** `route_seal` → `DroppedFull`, counter only; the
  downstream ring→spill→DLQ chain (the existing zero-tick-loss absorber) catches
  the payload. No panic.
- **QuestDB down at boundary:** irrelevant to this task — it only routes seals into
  the in-process mpsc; persistence is the seal-writer's concern (unchanged).
- **`global_seal_sender` uninstalled:** `warn!` + skip (mirrors Dhan); the next
  boundary re-checks.
- **Task panics:** a panic in the spawned task would stop future Groww force-seals
  but NOT affect the per-tick path or Dhan. Mitigation: the task body has no
  `unwrap/expect/index-panic` paths (matches the Dhan task, which is also a bare
  `tokio::spawn` with no supervisor — parity, not a regression).

## Test Plan

- `crates/app` unit tests (mirror the seal-routing tests in `seal_routing.rs`):
  - `test_groww_force_seal_routes_with_feed_groww` — drive `route_seal` with the
    EXACT params the Groww boundary closure uses (`feed: Feed::Groww`,
    `drop_d1: false`, others `None`) and assert the `BufferedSeal` reaching the
    channel carries `feed == Feed::Groww` for a non-D1 AND a D1 TF (Groww routes
    D1). This pins the closure's routing contract — the load-bearing behaviour.
  - `test_run_groww_bridge_spawns_ist_midnight_force_seal` — source-scan ratchet
    (the `run_groww_bridge` I/O driver is TEST-EXEMPT) pinning that the bridge
    spawns an IST-midnight force-seal task that calls `force_seal_all` routed with
    `feed: Feed::Groww` + `secs_until_next_ist_midnight` + the trading-day gate —
    mirrors the existing `test_run_groww_bridge_routes_seals_through_shared_writer`
    source-scan pattern. A future refactor cannot silently drop the EOD seal.
- `cargo test -p tickvault-app` — paste the `test result: ok. N passed` line.
- `cargo clippy -p tickvault-app -- -D warnings` (if fast) + `cargo fmt`.

## Rollback

Single-commit PR, confined to `crates/app` (`groww_bridge.rs` + `main.rs`). Revert
the commit to restore the exact prior behaviour: Groww per-tick sealing only, no
EOD force-seal. No schema change, no config change, no new dependency, no Dhan-path
change — so the revert is a clean `git revert <sha>` with zero migration. Disabling
Groww (`feeds.groww_enabled = false`) also fully neutralises the new task (it skips).

## Observability

This change adds NO new ErrorCode / counter / Telegram event — it routes existing
Groww seals through the SAME `global_seal_sender` chain that already carries the
per-tick Groww seals (same `tv_seal_mpsc_dropped_total{feed=groww}` drop counter,
same `BufferedSeal{feed=Groww}` audit trail, same downstream
`candles_*`/`feed=groww` rows). A `tracing::info!("Groww IST-midnight force-seal
complete", sealed, dropped)` line (mirrors the Dhan task's completion log) provides
the boundary-completion signal. The existing aggregator/candle observability
(7-layer) covers the sealed rows; no new layer is required because no new failure
mode is introduced (the seal path is the existing one).

## Per-Item Guarantee Matrix (cross-reference)

This single-item, single-crate, behaviour-additive PR carries the per-wave
guarantee contract by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`. The 15-row 100% guarantee
matrix and 7-row resilience matrix there apply; the honest proof for THIS item is:

| Demand | Honest proof for this item |
|---|---|
| 100% code coverage | New behaviour is the seal-routing closure (covered by `test_groww_force_seal_routes_with_feed_groww`) + a source-scan ratchet on the I/O-driver spawn; `run_groww_bridge` itself stays TEST-EXEMPT (existing convention) |
| 100% audit coverage | Reuses the existing `BufferedSeal{feed=Groww}` → `candles_*` audit trail; no new audit table needed (no new event type) |
| 100% testing coverage | unit (routing) + source-scan ratchet (wiring); workspace CI runs the full battery |
| 100% performance | Cold path (once/day per boundary); no hot-path allocation added; `force_seal_all` is `O(N×21)` cold per the existing doc-comment |
| 100% monitoring/logging/alerting | Reuses the existing seal-drop counter + adds a completion `info!`; no new failure mode → no new alert |
| 100% security | No new external input, no secret, no ILP injection surface; the closure is pure routing |
| 100% bugs fixing | This IS the bug fix; adversarial 3-agent review on the diff before merge |
| 100% scenarios/functionalities | Edge-case + failure-mode tables above; every new path has a test or a TEST-EXEMPT ratchet |
| 100% extreme check | The source-scan ratchet fails the build if a future refactor drops the EOD seal |

**Resilience (7-row):** Zero ticks lost — this fix REDUCES candle loss (seals the
last-bucket-of-day that was previously dropped); it introduces NO new tick-drop
path (seals route through the existing ring→spill→DLQ absorber). WS/QuestDB/O(1)/
uniqueness/real-time rows are unchanged (this is a cold-path candle-seal task that
touches neither the WS read loop nor the persist DEDUP keys).

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the Groww
aggregator now force-seals every open bucket across all 21 timeframes at IST
midnight on trading days (mirroring the Dhan task), so the last-bucket-of-day is no
longer silently overwritten; the seal routes through the SAME shared seal-writer
chain (ring→spill→DLQ, `TICK_BUFFER_CAPACITY`, ratcheted by
`zero_tick_loss_alert_guard.rs`) tagged `feed=groww`; the routing contract is pinned
by `test_groww_force_seal_routes_with_feed_groww` and the wiring by a source-scan
ratchet. Beyond the envelope (mpsc full at the boundary), the existing
ring→spill→DLQ absorber catches every seal as recoverable data. This does NOT claim
"Groww never loses a candle" — only that the IST-midnight EOD-seal gap (the verified
bug) is closed with the same bounded guarantee Dhan already has.

## Plan Items

- [x] Item 1 — Share the Groww aggregator + spawn IST-midnight force-seal task
  - Files: crates/app/src/groww_bridge.rs, crates/app/src/main.rs
  - Tests: test_groww_force_seal_routes_with_feed_groww,
    test_run_groww_bridge_spawns_ist_midnight_force_seal

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | IST midnight, trading day, Groww enabled, aggregator non-empty | Every open bucket across 21 TFs sealed + routed `feed=Groww` |
| 2 | IST midnight, non-trading day | Skipped (no seal pass) |
| 3 | IST midnight, Groww disabled | Skipped |
| 4 | IST midnight, aggregator empty | No-op (0 seals) |
| 5 | D1 timeframe at boundary | Routed (Groww `drop_d1=false`) |
