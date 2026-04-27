# Wave 2 — Guarantees Analysis (Grounded in tests + file:line)

> **Authority:** Operator question 2026-04-27. Answers "where is zero-tick-loss
> guaranteed, where is it best-effort, and what does Wave 2 close?"
> **Sources:** 4 parallel Explore agents reading the actual codebase.
> **No aspiration. Every claim cites a test or rule file.**

## TL;DR

There is NO mathematical "100%" in distributed systems. There are **mechanical
proofs** — tests in CI that fail the build if an invariant is violated.

`tickvault` already has **strong proofs** for tick-loss avoidance, WebSocket
recovery, QuestDB resilience, and O(1) hot paths. Wave 2 closes the remaining
**give-up paths** (main-feed `return false` post-close, depth 60-attempt cap,
boot race window) and adds a **tick-gap RCA detector** + **6 audit tables**.

The full grounded tables are in `.claude/plans/wave-2-guarantees-analysis.md`.

## Table 1 — Tick-loss guarantees

| Risk surface | Current proof (file:line + test) | Wave 2 addition | Residual risk |
|---|---|---|---|
| WS disconnect mid-tick | SubscribeRxGuard reinstalls receiver — `crates/core/src/websocket/connection.rs:114-147` ; tests `test_subscribe_rx_guard_reinstalls_on_drop`, `_survives_many_cycles` | Item 8 — tick-gap detector flags any 30s silence per `(security_id, segment)` | None added |
| WS gives up post-close (main feed) | **CURRENT GAP** — `return false` at `connection.rs:1285` | **Item 5** — `sleep_until_next_market_open_ist()` + `respawn_dead_connections_loop` ; tests `test_main_feed_post_close_sleeps_until_next_open`, `_skips_weekend_to_monday_0900` | Holiday calendar drift (mitigated by `TradingCalendar`) |
| Depth WS gives up after 60 attempts | **CURRENT CAP** — `DEPTH_RECONNECT_MAX_ATTEMPTS = 60` at `depth_connection.rs:123` | **Item 6** — 60 in-market attempts THEN sleep until 09:00 | Same as above |
| Token expiry mid-sleep | Polls 5s/60s for valid token (`connection.rs:1342`) | **Item 5.4** — `force_renewal_if_stale(14400s)` before reconnect-after-sleep ; test `test_token_force_renewal_on_wake_when_stale` | None |
| QuestDB outage → ring overflow | Ring 600K + disk spill ; chaos `chaos_zero_tick_loss.rs`, `chaos_rescue_ring_overflow.rs` ; test asserts `tv_ticks_dropped_total == 0` | None | Sustained >100K-frame burst with disk full → drops (Wave 1 raised SPILL to 131k) |
| Sequence holes in depth feed | `DepthSequenceTracker` papaya O(1) ; `crates/core/src/pipeline/depth_sequence_tracker.rs` ; DHAT zero-alloc + loom concurrency tests | None | Dhan side-effects (Dhan can drop sequence — exchange-side, not ours) |
| Boot race (WS streams before QuestDB ready) | **CURRENT GAP** — tick processor consumes SPSC immediately | **Item 7** — `wait_for_questdb_ready(60s)` ; escalating logs at +5/+10/+20/+30/+60s ; test `test_tick_processor_waits_for_questdb_ready` | None |
| Tick-gap RCA | None (no per-instrument silence detection) | **Item 8** — composite-key papaya + 60s coalesce + 15:35 IST reset ; bench `bench_tick_gap_lookup_le_50ns` (≤50ns p99) ; DHAT zero-alloc | Coalesce window may delay 1st alert by ≤60s (intentional) |
| Live-feed purity | `live_feed_purity_guard.rs` — 4 tests ban `BackfillWorker`, `synthesize_ticks`, `append_tick` in historical path | None | None |

## Table 2 — WebSocket resilience

| Surface | Current proof | Wave 2 addition |
|---|---|---|
| Pool watchdog (5s tick) | `main.rs:4917 spawn_pool_watchdog_task` writes `tv_websocket_connections_active` ; test `test_pool_watchdog_writes_active_connection_counter_to_health` | **Item 5.3** — `respawn_dead_connections_loop` supervisor (auto-respawns within 5s) |
| Off-hours disconnect | `WebSocketDisconnectedOffHours` (Severity::Low) — `notification/events.rs:335` ; test `test_websocket_disconnected_off_hours_has_low_severity` | None |
| Order-update activity watchdog | 14400s (4h) — `activity_watchdog.rs:75` ; test `test_watchdog_thresholds_match_dhan_spec` | **Item 6** keeps 14400s (regression test `test_order_update_activity_watchdog_remains_14400s`) |
| Resilience SLA alerts | 10 pinned alerts — `resilience_sla_alert_guard.rs:48-62` (WebSocketDisconnected, HighReconnectRate, Backpressure, QuestDbDown, ValkeyDown, etc.) | None |
| Token-aware wake | None (token can expire during 16h overnight sleep) | **Item 5.4** — `TokenManager::force_renewal()` before reconnect-after-sleep |

## Table 3 — QuestDB resilience

| Surface | Current proof | Wave 2 addition |
|---|---|---|
| Schema drift | `ALTER TABLE ADD COLUMN IF NOT EXISTS` self-heal at boot — `instrument_persistence.rs:354-412` | **Item 9** — same pattern in 6 new audit tables |
| DEDUP keys composite | `dedup_segment_meta_guard.rs:84` scans every `DEDUP_KEY_*` constant ; fails build if `security_id` lacks `segment` | **Item 9** — meta-guard extended to 6 audit modules |
| ILP backpressure | Ring 600K + disk spill at `data/spill/ticks-YYYYMMDD.bin` ; chaos `chaos_questdb_docker_pause.rs`, `chaos_rescue_ring_overflow.rs`, `chaos_disk_full.rs` (10 storage chaos tests + 6 core) | **Item 7** — adds `wait_for_questdb_ready` so boot-time WS frames don't bypass ring before it's wired |
| Flush-failure logging | `error_level_meta_guard.rs:65` — 28 phrases must use `error!`, never `warn!` | **Item 8/9** — new flush sites added under same guard |
| Materialized views | 18 views from `candles_1s` — `materialized_views.rs:103-220` ; idempotent `CREATE IF NOT EXISTS` | None |
| S3 archival | Partition manager detaches old partitions | **Item 9.4** — `(table, partition_date)` idempotency-key in `s3_archive_log` (G18) prevents re-archive |

## Table 4 — Cross-cutting invariants

| Property | Current proof | Wave 2 |
|---|---|---|
| O(1) hot path | DHAT zero-alloc tests + Criterion budgets in `quality/benchmark-budgets.toml` (parse ≤10ns, lookup ≤50ns, route ≤100ns) ; 5% regression gate via `scripts/bench-gate.sh` | **Item 8** — adds `bench_tick_gap_lookup_le_50ns` + `dhat_tick_gap_detector` |
| Uniqueness — composite key (I-P1-11) | 7-layer enforcement: planner, CSV-parse, registry, storage DEDUP, banned-pattern hook category 5, FnoUniverse runtime detection, Prometheus visibility | **Item 8** — tick_gap_detector uses `(u32, ExchangeSegment)` papaya key |
| Dedup at storage | `DEDUP UPSERT KEYS` on every table (ticks, candles_*, historical_candles, instrument_*, deep_market_depth) | **Item 9** — DEDUP on all 6 audit tables |
| Automation — Telegram | Alertmanager dedup ; 5 sinks (stdout / app.log / errors.log / errors.jsonl / Telegram+SNS) | **Item 8** — 60s coalescing window for tick-gap (G4 — fixes per-instrument spam) |
| Automation — runbooks | 67 ErrorCode variants × runbook MD ; cross-ref test `error_code_rule_file_crossref.rs` fails build if any variant lacks rule | **C3** — `wave-2-error-codes.md` for the ~10 new variants |
| Auto-triage | 11 auto-fix scripts + `error-rules.yaml` (7 seed rules) + `claude-loop-prompt.md` + MCP server | **C2** — new triage YAML rules per Wave 2 ErrorCode |
| Dashboards | 12 Grafana JSON ; `operator-health.json` pinned by guard test | **Item 9.7** — new `audit-trails.json` (6 panels) |
| Real-time alerts | 66 Prometheus alert rules + 10 SLA alerts pinned | **Item 5/6/7/8/9** add ~5 new alert rules + meta-guard pin |

## Honest residual risks (no test can rule these out)

1. **Disk full + sustained QuestDB outage > N hours** — eventually rescue
   ring evicts oldest. Mitigation: monitoring + S3 archival. Wave 2 doesn't
   change this surface.
2. **Dhan-side packet drops** — if Dhan's exchange-feed gateway drops a
   sequence number, our sequence-hole detector ALERTS but cannot recover
   the tick. Cross-verify with historical candles closes the loop
   (post-market). Wave 2 doesn't change this surface.
3. **Holiday calendar drift** — `TradingCalendar` is config-loaded; if NSE
   adds a surprise holiday and config isn't updated, sleep wakes early.
   Mitigation: `make doctor` + Telegram alert on first connect failure.
4. **Single-AZ AWS** — no HA failover. Within the ₹5,000/mo budget per
   `aws-budget.md`. Acceptable for retail solo trader.
5. **Stream timeout during long Claude responses** — addressed by
   stream-resilience.md (this rule file).

## What Wave 2 does NOT promise

- 100% recovery from arbitrary kernel panics — already best-effort via
  `chaos_sigkill_replay.rs` + WAL replay.
- Survival of GitHub/Dhan/AWS simultaneous outage — out of scope.
- Recovery of ticks already accepted by Dhan but lost in transit — only
  Dhan's gateway can replay (we lobby Dhan support; see
  `docs/dhan-support/`).

## Decision needed

The Wave 2 plan as specified in `.claude/plans/active-plan-wave-2.md`
closes the four highest-value gaps:

| Gap | Closed by |
|---|---|
| G1 — main-feed gives up post-close | Item 5 |
| G14 — boot race | Item 7 |
| G4 — per-instrument tick-gap spam (would happen at scale once Item 8 lands without coalescing) | Item 8 |
| G18 — no audit trail for Phase2/depth-rebalance/WS-reconnect/boot/selftest/orders | Item 9 |

Estimated scope: 5 commits + C11 doc rewrite + PR. ~1,500-2,000 LoC across
~30 files including new modules, schema, tests, benches, docs.

**Operator confirm: proceed?**
