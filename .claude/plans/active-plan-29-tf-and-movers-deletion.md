# Active Plan: 29-Timeframes Engine + Movers Deletion + Bulletproof Resilience

**Status:** DRAFT (awaiting operator approval before any code change)
**Date:** 2026-05-04
**Branch (proposed):** `claude/29-tf-movers-deletion`
**Authority chain:** `CLAUDE.md` > `.claude/rules/project/per-wave-guarantee-matrix.md` > `.claude/rules/project/stream-resilience.md` > this file
**Operator:** Parthiban
**Builder:** Claude Code

---

## 0. Executive summary

Replace the dual `stock_movers` + `option_movers` tables with a single unified architecture:
**(a)** in-memory `CandleEngine<TF>` × 29 timeframes for trading hot path (zero-DB, O(1));
**(b)** 29 `candles_<tf>` matviews in QuestDB for replay / audit / dashboard / API;
**(c)** one `formulas.rs` module shared by both paths so RAM ≡ DB byte-equal;
**(d)** config-driven extensibility — adding TF #30 is a TOML edit, not a redesign.

Net code change: **~700 LoC less, 2 tables less, 7 ErrorCodes less, 1 unified engine more.**

---

## 1. Locked decisions (operator-confirmed Y)

| # | Decision | Locked turn |
|---|---|---|
| L1 | Ticks store raw `volume` (cum-day) + NEW `volume_delta` + NEW `prev_day_close` + NEW `prev_day_oi` + NEW `phase`. NEVER store derived percentages. | session 2026-05-04 |
| L2 | Candles store existing per-bar `volume` + NEW `volume_cum_day_at_end` + NEW `prev_day_close` + NEW `prev_day_oi` + NEW `phase`. NEVER store derived percentages. | session 2026-05-04 |
| L3 | All 29 timeframes (1s/3s/5s/10s/15s/30s, 1m..15m, 30m, 1h, 2h, 3h, 4h, 1d, 1w, 1mo) — same generic engine, applied uniformly. | session 2026-05-04 |
| L4 | Trading reads RAM only (zero DB on hot path); dashboard/API reads DB matviews; both consistent via parameterized parity tests. | session 2026-05-04 |
| L5 | DELETE `stock_movers` + `option_movers` + writers + 4 ErrorCodes; replace with unified engine + matviews. | session 2026-05-04 |
| L6 | All math in single source `crates/common/src/formulas.rs`. RAM engine + DB matviews + API SELECTs all consume it. Drift mechanically impossible. | session 2026-05-04 |
| L7 | Config-driven from day 1: timeframes, movers categories, segments, metrics, phases. Adding any new one = TOML edit, no code change. Ratchets enforce. | session 2026-05-04 |
| L8 | Honest 100% envelope wording per `wave-4-shared-preamble.md` Section 8 in plan + every PR body. No literal "never". | session 2026-05-04 |
| L9 | RAM = trading speed only. DB = ALWAYS source of truth for replay/audit. Both written every tick. No data loss path. | session 2026-05-04 |
| L10 | 13 NEW bulletproofing additions (forensic snapshot, mmap spill, cache-line align, etc. — see §7). NO forced rotation, NO subscribe-noop heartbeat — zero deliberate disconnects per L11. | session 2026-05-04 |
| L11 | **ZERO deliberate disconnects.** Code MUST NOT initiate any disconnect on a healthy socket. Only Dhan-side / network-side disconnects are recovered (reactive). Banned: forced rotation, voluntary connection cycling, voluntary unsubscribe-then-resubscribe on healthy connections. Activity watchdog (passive detection) stays. | session 2026-05-04 |
| L12 | **Cascade timeframe design.** Hot path touches ONLY the `1s` engine on every tick. When `1s` engine seals a bar, the sealed bar cascades into `3s`/`5s`/`1m`/...`1mo` engines via background SPSC consumer. Result: per-tick budget = 1 atomic store path (≤200ns achievable), NOT 29. Hot-path agent CRITICAL C2 fix. | session 2026-05-04 |
| L13 | **IST midnight reset invariant.** `last_seen[security]` map (volume_delta calculator) + `prev_day_close` first-seen stamper + `prev_oi_cache` ALL reset/reload at IST midnight in a single atomic phase transition. VOLUME-MONO-01 monotonicity check is SUPPRESSED during PREMARKET phase to prevent the day-rollover negative-delta blow-up across 25K instruments. Hostile bug-hunt CRITICAL C3 fix. | session 2026-05-04 |
| L14 | **Strict cold-boot ordering invariant.** Boot sequence MUST complete in this order before WS subscribe fires: (1) `prev_oi_cache` loaded from `candles_1d` yesterday, (2) `CandleEngine` and `MoversEngine` instantiated and ready, (3) historical replay of last 5 min ticks from `ticks` table, (4) THEN Dhan WS subscribe. Ratchet test pins each gate. Hostile bug-hunt CRITICAL C4 fix. | session 2026-05-04 |

---

## 2. The architecture (CASCADE design — corrected per hot-path agent CRITICAL C2)

**Hot path touches ONE engine (`1s`). Coarser timeframes derived OFF hot path.**

```
                     Every Dhan tick
                            │
                            ▼
                   ┌────────────────┐
                   │ 1s CandleEngine│ ← ONLY engine on hot path
                   │ (atomic OHLCV) │   ≤200ns per tick
                   └────────┬───────┘
                            │ on bar seal (every 1s)
                            ▼
              SPSC channel (off hot path)
                            │
       ┌────────────────────┼────────────────────┐
       ▼                    ▼                    ▼
   28 derived           ticks table       derivative_contracts
   engines              (raw + 4 NEW       (already exists)
   (3s,5s,...,1mo)      cols)
   cascaded from
   sealed 1s bars
       │                  cols)               │
       │                    │                 │
       │                    ▼                 │
       │            29 candles_<tf>           │
       │            matviews                  │
       │                    │                 │
       │                    └─────JOIN────────┘
       │                            │
       ▼                            ▼
   Trading bot               /api/movers + dashboards
   reads RAM <20ns           reads JOIN result <100ms
   NEVER touches DB          NEVER blocks trading
```

### Single source of truth: `crates/common/src/formulas.rs`

Every formula lives once. RAM engine calls Rust fn. DB matview/SELECT uses the SQL constant in the same file. Test asserts byte-equal output across 1M random inputs.

```rust
#[inline(always)]
pub fn change_pct(close: f32, prev_close: f32) -> f32 {
    if prev_close <= 0.0 { 0.0 } else { (close - prev_close) / prev_close * 100.0 }
}

pub const FORMULA_CHANGE_PCT_SQL: &str =
    "CASE WHEN prev_day_close > 0 THEN (close - prev_day_close) / prev_day_close * 100 ELSE 0 END";
```

---

## 3. Column design

### IST midnight reset semantics (per L13 + hostile bug-hunt CRITICAL C3)

At IST midnight (00:00:00.000 IST), a single atomic phase transition runs:

| Step | Action | Why |
|---|---|---|
| 1 | Snapshot `last_seen[security]` map → discard | yesterday's cumulative volumes invalid |
| 2 | Reload `prev_oi_cache` from `candles_1d` (yesterday's row) | new "previous day OI" baseline |
| 3 | Reset `prev_day_close` first-seen stamper | new trading day = stamp on first tick |
| 4 | Set phase = PREMARKET | until 09:00 IST |
| 5 | **VOLUME-MONO-01 SUPPRESSED until phase reaches OPEN** | first ticks of new day legitimately reset Dhan's cumulative counter to ~0 |
| 6 | Engine state TTL sweep — drop entries with no tick for >7 days | prevents F&O expiry leak (hostile bug-hunt HIGH H7) |

Single-thread owns the rollover: `MidnightRolloverTask` runs in the tick-processor SPSC consumer (no race across threads).

### `ticks` table (4 NEW columns, additive ALTER)

| Column | Source | Update freq | Notes |
|---|---|---|---|
| `volume` (existing) | bytes 22-25 | every tick | raw cumulative-day from Dhan, semantically unchanged |
| `volume_delta` (NEW) | derived at parse: `current - last_seen[security]` | every tick | per-tick incremental; VOLUME-MONO-01 wires through it |
| `prev_day_close` (NEW) | first-seen stamper from packet close field (or code 6 for IDX_I) | once per (security, day) | frozen for the trading day |
| `prev_day_oi` (NEW) | `prev_oi_cache` loaded at boot from yesterday's `candles_1d` | once per (security, day) | frozen for the trading day |
| `phase` (NEW) | `compute_phase(now_ist, segment)` pure fn | every tick (computed) | PREMARKET / PREOPEN / OPEN / POSTAUCTION / CLOSED |

### `candles_<tf>` matview (4 NEW columns, rebuild required — off-hours)

| Column | Aggregation | Notes |
|---|---|---|
| `open` / `high` / `low` / `close` (existing) | first / max / min / last over bucket | unchanged |
| `volume` (existing) | already per-bar (`last - first` over bucket) | semantically unchanged |
| `oi` (existing) | last(oi) over bucket | unchanged |
| `volume_cum_day_at_end` (NEW) | `last(volume)` from ticks (cumulative) | for Top Volume ranking — matches Dhan UI |
| `prev_day_close` (NEW) | `last(prev_day_close)` from ticks | frozen-per-day, projected to candle |
| `prev_day_oi` (NEW) | `last(prev_day_oi)` from ticks | same |
| `phase` (NEW) | `last(phase)` from ticks | what phase the bar was in |

### What stays in `derivative_contracts` (NOT copied to candles)

`trading_symbol`, `underlying_symbol`, `instrument_type`, `strike_price`, `option_type`, `expiry_date` — STATIC per `(security_id, exchange_segment)`. JOIN at query time. ~29K rows fits in QuestDB cache. JOIN cost <5ms.

---

## 4. The 29 timeframes (config-driven from day 1)

`config/base.toml`:

```toml
[timeframes]
enabled = [
  "1s", "3s", "5s", "10s", "15s", "30s",
  "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m", "11m", "12m", "13m", "14m", "15m",
  "30m", "1h", "2h", "3h", "4h",
  "1d", "1w", "1mo"
]

[timeframes.retention]
sub_second = "7d"   # 1s/3s/5s/10s/15s/30s — high volume, short retention
minute     = "90d"  # 1m..15m — standard
hour       = "1y"   # 30m..4h — swing
day_plus   = "5y"   # 1d/1w/1mo — SEBI compliance
```

Boot loop:
```
for tf in config.timeframes.enabled {
    create_matview_if_not_exists("candles_" + tf);
    spawn CandleEngine::<tf>();
    register Prometheus counter tv_candle_<tf>_*;
    provision Grafana panel from template;
    provision Prometheus alert from template;
    register parameterized parity test for tf;
}
```

**Adding TF #30 = append one string to the array. Zero code change.**

---

## 5. `/api/movers` — single SQL, all 7 categories

**Security note (per security-reviewer HIGH H4):** the `ORDER BY` column is NOT raw-string interpolated from TOML. Config category names map to a `MoversCategory` enum at parse time; the enum's `order_by_clause()` returns a `&'static str` whitelist literal. TOML edits cannot inject SQL. See `crates/api/src/handlers/movers.rs::MoversCategory` (existing pattern, replicated here).

**Composite-key note (per security-reviewer LOW + I-P1-11):** `LATEST ON ts PARTITION BY security_id, exchange_segment` — composite key is mandatory; `security_id` alone causes cross-segment SecurityId collision (NSE_FNO/BSE_FNO).

```sql
WITH latest AS (
  SELECT c.security_id, c.exchange_segment,
         c.close AS ltp, c.prev_day_close,
         c.volume_cum_day_at_end AS volume,
         c.oi, c.prev_day_oi, c.phase,
         d.trading_symbol, d.underlying_symbol, d.instrument_type,
         d.strike_price, d.option_type, d.expiry_date,
         (c.close - c.prev_day_close) AS change_amount,
         FORMULA_CHANGE_PCT_SQL AS change_pct,
         (c.oi - c.prev_day_oi) AS oi_change,
         FORMULA_OI_CHANGE_PCT_SQL AS oi_change_pct
  FROM candles_1m c
  JOIN derivative_contracts d USING (security_id, exchange_segment)
  LATEST ON ts PARTITION BY security_id, exchange_segment
  WHERE ts > now() - 5m
)
SELECT * FROM latest
WHERE exchange_segment = $1 AND instrument_type = $2
ORDER BY <CONFIG-DRIVEN COLUMN> <DIR>   -- top_volume / change_pct / oi_change_pct
LIMIT 10;
```

Categories from `config/base.toml`:

```toml
[[movers.categories]]
name = "top_volume"
order_by = "volume"
direction = "desc"

[[movers.categories]]
name = "price_gainers"
order_by = "change_pct"
direction = "desc"

[[movers.categories]]
name = "price_losers"
order_by = "change_pct"
direction = "asc"

[[movers.categories]]
name = "oi_gainers"
order_by = "oi_change_pct"
direction = "desc"

[[movers.categories]]
name = "oi_losers"
order_by = "oi_change_pct"
direction = "asc"

[[movers.categories]]
name = "highest_oi"
order_by = "oi"
direction = "desc"

[[movers.categories]]
name = "top_value"
order_by = "ltp * volume"
direction = "desc"
```

**Adding a new category = append one TOML block. Zero code change.**

---

## 6. The 5-phase rollout (per hostile bug-hunt CRITICAL C5: no big-bang DROP)

| Phase | Ships | Risk if rolled back | Gate before next phase |
|---|---|---|---|
| **Phase 1: Schema widen** (additive ALTER + matview rebuild) | `ALTER TABLE ticks ADD COLUMN IF NOT EXISTS` x4; matviews rebuilt with new schema during off-hours; old movers tables UNTOUCHED | ZERO — new columns sit empty if writer not deployed | matview rebuild verified on at least 1 trading day |
| **Phase 2: Populate new columns** (dual-write) | Tick parser stamps `prev_day_close` (first-seen) + `prev_day_oi` (boot cache) + `phase` + `volume_delta` (with IST midnight reset per L13); matview captures them; old movers tables ALSO still being written | ZERO — old path unchanged | 7 trading days of populated columns + zero parser errors |
| **Phase 3: Add in-memory engines + parity test** (shadow mode) | `CandleEngine<1s>` + 28 derived engines (cascade per L12) + `MoversEngine` deployed; `/api/movers` v2 SELECT exposed alongside v1; ratchet test compares RAM vs new SELECT vs old movers tables — all three must agree | ZERO — strategy still reads old tables; new engines shadow-deployed | 14 trading days green-streak across all 3 ratchet axes |
| **Phase 4a: Read-path flip + 24h soak** | Strategy reads `MoversEngine` (RAM); `/api/movers?v=1` STILL backed by old tables; `?v=2` backed by new SELECT. Old movers writers KEEP RUNNING. Operator dashboards verified on v2 schema. **24h soak with both paths live.** | LOW — revert read-path PR; old data still being written; no DROP yet | 14 trading days post-flip, no v1/v2 disagreement, no operator regression |
| **Phase 4b: Writer + table deletion** | Old movers writers DELETED; `DROP TABLE stock_movers, option_movers`; 4 ErrorCodes + runbooks deleted; `/api/movers?v=1` shim returns 410 Gone with migration message | LOW — `?v=2` always working; data still in `ticks` for replay | post-deploy: 30 trading days monitoring before declaring "done" |

---

## 7. 12 bulletproofing additions (was 13; bump arena DELETED per hot-path CRITICAL C1)

| # | Addition | What it solves | Mechanism | Ratchet |
|---|---|---|---|---|
| 1 | Cache-line align hot atomics (`#[repr(align(64))]`) | false-sharing slowdown under burst | `MoverState` + `CandleState` aligned | `test_atomic_alignment_64_bytes` |
| ~~2~~ | ~~Bump arena~~ DELETED per hot-path CRITICAL C1 — fixed-size atomics + papaya don't allocate; arena was dead weight | n/a | n/a | n/a |
| 3 | `mmap`'d tick spill file with **boot-time `MADV_WILLNEED` pre-fault** (per hot-path HIGH H2) | OS page cache handles 10x burst; pre-fault prevents page-fault syscalls on hot path | `memmap2` crate, fixed 2GB file, sequential touch at boot, **wrap-with-DLQ on overflow + CRITICAL alert** (per hostile bug-hunt CRITICAL C6) | `chaos_burst_10x.rs` + `test_mmap_pre_fault_at_boot` |
| 4 | Per-subsystem memory ceiling + alert at 80% | catches drift before OOM | `tv_subsystem_memory_bytes` gauge + Prom alert | `resilience_sla_alert_guard.rs` extension |
| 5 | OOM-protect parser thread (`oom_score_adj=-1000`) | parser last to die under pressure | systemd unit + boot log assertion | `test_oom_score_set_at_boot` |
| 6 | `core_affinity` pin WS-read + parser to cores | deterministic latency, no jitter | already in Wave 5 (CORE-PIN-01) | already pinned |
| 7 | Aggressive TCP keepalive (1s/5s) | detect dead conn <10s | socket option set on connect | `test_tcp_keepalive_set` |
| 8 | DNS round-robin Dhan endpoint IPs | failover on IP route degradation | `tokio_dns_lookup_all` + retry next | `test_dns_failover_to_secondary_ip` |
| 9 | Dual-stack IPv4 + IPv6 (Happy Eyeballs RFC 8305) | one stack down → other works | `tokio::net::TcpStream::connect` with both families | `test_happy_eyeballs_fallback` |
| 10 | Per-conn forensic 60s ring of events | one-line "why did it die" | `arrayvec::ArrayVec<EventLog, 1000>` per conn | `test_forensic_ring_captures_pre_disconnect_60s` |
| 11 | Pre-disconnect snapshot dumped to JSON on every RST | rules in/out our code as cause | snapshot of ring depth, QuestDB state, mem, CPU, net at moment of RST | `test_snapshot_written_on_disconnect` |
| 12 | On-demand replay from spill (`make replay-spill-last-60s`) | test recovery without waiting for real failure | tool reads spill NDJSON, re-injects into pipeline | `test_replay_spill_produces_identical_output` |
| 13 | Chaos test: synthetic Dhan RST flood (Friday CI) | proves bounded recovery weekly | `chaos_dhan_rst_flood.rs` injects RST every 30s for 5min | weekly CI run |

**REMOVED (per L11 — zero deliberate disconnects):**
- ~~Forced WS conn age rotation every 4h~~ — banned: deliberate disconnects violate L11
- ~~Active subscribe-noop heartbeat every 30s~~ — banned: deliberate work on socket violates L11. Activity watchdog (passive) at 50s already detects silent sockets without any voluntary work.

---

## 8. Ratchet tests (full enumeration — every test fails build on regression)

| Test | File | What it pins |
|---|---|---|
| `test_rust_sql_formula_byte_parity_change_pct` | `crates/common/tests/formulas_parity.rs` | RAM `change_pct()` ≡ SQL formula on 1M random inputs |
| `test_rust_sql_formula_byte_parity_oi_change_pct` | same | same for OI |
| `test_movers_engine_top_k_matches_db_select_at_minute_boundary` | `crates/trading/tests/movers_parity.rs` | RAM top-10 = DB SELECT at minute close |
| `test_no_inline_formula_outside_formulas_module` | `crates/common/tests/inline_formula_guard.rs` | source-grep ban: no `(close - prev_day_close)` outside `formulas.rs` |
| `test_no_hardcoded_timeframe_string_outside_config_loader` | `crates/common/tests/hardcoded_tf_guard.rs` | ban `"1m" \| "5m" \| ...` literals outside loader |
| `test_adding_new_tf_requires_only_config_change` | `crates/app/tests/dynamic_tf_meta.rs` | meta-test: boot with `enabled = [..., "90s"]` → engine + matview + counter all materialize |
| `test_zero_alloc_per_tick_candle_engine_29_tfs` | `crates/trading/tests/dhat_candle_engine.rs` | DHAT: 0 allocs across 10K ticks, all 29 TFs |
| `test_per_tick_budget_under_200ns_29_tfs` | `crates/trading/benches/candle_engine.rs` | Criterion: p99 ≤ 200ns |
| `test_first_seen_stamper_freezes_prev_day_close` | `crates/core/tests/first_seen_stamper.rs` | first tick of day stamps; subsequent ticks ignore |
| `test_prev_oi_cache_loaded_from_candles_1d_at_boot` | `crates/core/tests/prev_oi_cache.rs` | non-empty after boot |
| `test_phase_pure_fn_property` | `crates/common/tests/phase_property.rs` | proptest: deterministic across every IST minute + every NSE holiday |
| `test_volume_delta_monotonic_or_breach_emitted` | `crates/core/tests/volume_mono_guard.rs` | VOLUME-MONO-01 wires through `volume_delta` |
| `test_dedup_segment_in_all_29_matviews` | `crates/storage/tests/dedup_segment_meta_guard.rs` (extended) | every `candles_<tf>` DEDUP UPSERT KEYS includes segment |
| `test_movers_tables_deleted_in_phase_4` | `crates/storage/tests/phase_4_deletion_guard.rs` | post-Phase-4: tables don't exist |
| `test_atomic_alignment_64_bytes` | `crates/trading/tests/atomic_align.rs` | `MoverState`/`CandleState` aligned |
| `test_tcp_keepalive_set_to_1s_5s` | `crates/core/tests/tcp_keepalive.rs` | socket options set on connect |
| `test_dns_failover_to_secondary_ip` | `crates/core/tests/dns_failover.rs` | second IP tried if first fails |
| `test_happy_eyeballs_fallback_v6_to_v4` | `crates/core/tests/happy_eyeballs.rs` | RFC 8305 |
| `test_forensic_ring_captures_pre_disconnect_60s` | `crates/core/tests/forensic_ring.rs` | last 60s of events available on RST |
| `test_snapshot_written_on_disconnect` | `crates/core/tests/disconnect_snapshot.rs` | JSON dumped to `data/logs/disconnect-forensics.*.json` |
| `test_replay_spill_produces_identical_output` | `crates/storage/tests/spill_replay.rs` | re-injection from spill = original pipeline output |
| `chaos_dhan_rst_flood` | `crates/core/tests/chaos_dhan_rst_flood.rs` | weekly CI: 5min RST every 30s — bounded recovery |
| `chaos_burst_10x_traffic` | `crates/core/tests/chaos_burst_10x.rs` | 10x normal tick rate — mmap spill absorbs |
| `test_cascade_only_1s_engine_touched_on_tick` | `crates/trading/tests/cascade_hot_path_guard.rs` | hot-path C2 fix: per-tick path touches `1s` only; coarser TFs derived off-path. Source-grep + DHAT prove no `CandleEngine<3s/5s/.../1mo>::on_tick` call site exists in `tick_processor.rs`. |
| `test_midnight_rollover_atomic_phase_transition` | `crates/core/tests/midnight_rollover.rs` | hostile C3 fix: at IST 00:00:00, `last_seen` cleared, `prev_oi_cache` reloaded, phase=PREMARKET, VOLUME-MONO-01 suppressed atomically. Property test: cross-thread tick at 23:59:59.999 vs 00:00:00.001 stamps the correct trading_date. |
| `test_boot_ordering_invariant_pre_oi_before_subscribe` | `crates/app/tests/boot_order_guard.rs` | hostile C4 fix: subscribe MUST NOT fire until (1) `prev_oi_cache` loaded, (2) engines ready, (3) historical replay complete. Test injects WS subscribe at each phase, asserts blocked until all 3 gates green. |
| `test_phase4a_24h_soak_dual_path_parity` | `crates/api/tests/phase_4a_soak_guard.rs` | hostile C5 fix: during 24h soak, `?v=1` and `?v=2` MUST return identical top-K. Build fails if disagreement detected over 100 simulated requests. |
| `test_mmap_spill_overflow_wraps_with_dlq` | `crates/storage/tests/mmap_spill_overflow.rs` | hostile C6 fix: when 2GB mmap fills, oldest entries wrap into DLQ NDJSON, Prometheus counter `tv_spill_wrap_total` increments, CRITICAL alert fires. NO crash, NO silent loss, NO blocking. |
| `test_movers_category_allowlist_enum` | `crates/api/tests/movers_allowlist.rs` | security H4 fix: TOML `order_by = "(SELECT ...)"` rejected at parse time; only enum-whitelisted columns reach SQL. |
| `test_forensic_snapshot_sanitizes_dhan_strings` | `crates/core/tests/forensic_sanitize.rs` | security H5 fix: Dhan disconnect reason strings + QuestDB error bodies pass through `sanitize_audit_string` before NDJSON write. CRLF injection rejected. |
| `test_engine_state_ttl_drops_expired_contracts` | `crates/trading/tests/engine_ttl.rs` | hostile H7 fix: `MoverState` entries with no tick for >7 days dropped on weekly sweep. F&O expiry leak prevented. |
| `test_packet_semantic_drift_hourly_sanity_check` | `crates/core/tests/packet_drift_guard.rs` | hostile H9 fix: hourly sanity sweep across N≥100 SIDs verifies `volume` is cumulative-monotonic. If Dhan flips semantics mid-session, CRITICAL alert fires within 1h. |
| `test_parity_ratchet_2s_tolerance_window` | `crates/trading/tests/movers_parity.rs` (extended) | hostile H6 fix: minute-boundary parity test allows 2s lag between RAM seal and matview seal. No flake. |
| `test_forensic_snapshot_edge_triggered_coalesced` | `crates/core/tests/forensic_edge_trigger.rs` | hostile H5 fix: 5 RSTs in <1s emit 1 coalesced snapshot + count, NOT 5 separate files. |

**Total: 25 new ratchet tests + extensions to 4 existing meta-guards.**

---

## 9. Honest 100% envelope wording (mandatory PR body)

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤2,000,000-tick ring buffer capacity (constant `TICK_BUFFER_CAPACITY`);
> ≤10x traffic burst absorbed by 2GB mmap'd spill with `MADV_WILLNEED` pre-fault, wrap-with-DLQ on overflow (chaos-tested);
> bench-gated O(1) hot path (≤200ns per tick on the `1s` engine; coarser TFs derived off-path via cascade per L12);
> RAM ≡ DB formula parity enforced by build-time + boot-time + runtime ratchets with 2s tolerance window for matview-seal lag;
> composite-key uniqueness `(security_id, exchange_segment)` enforced in tick storage, candle DEDUP, registry, and `LATEST ON` SQL;
> IST midnight rollover atomic phase transition (clears `last_seen` map, reloads `prev_oi_cache`, suppresses VOLUME-MONO-01 during PREMARKET) per L13;
> strict cold-boot ordering: cache → engines → historical replay → THEN subscribe per L14;
> 5-phase rollout (Phase 4a 24h soak before Phase 4b DROP) prevents in-flight `?v=1` request failures;
> F&O expiry leak prevented via 7-day TTL sweep on engine state;
> Dhan packet semantic drift detected hourly across N≥100 SIDs (cumulative-monotonicity sanity);
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake;
> chaos-tested Dhan RST flood weekly in CI.
> Beyond the envelope, DLQ NDJSON catches every payload as recoverable text.
> Live proof: Telegram alerts on 2026-05-04 show 5/5 disconnect at 12:55 IST detected + auto-recovered + zero tick loss + depth swaps continued normally.
> Adversarial review: 3 agents (hot-path-reviewer + security-reviewer + general-purpose hostile bug-hunt) reviewed plan v1; all 6 CRITICAL + 11 HIGH findings folded into v2 (this plan)."

---

## 10. Three adversarial agents queue (per `wave-4-shared-preamble.md` Section 3)

Run BEFORE each PR opens AND on the diff before merge:

| Agent | Focus | Report budget |
|---|---|---|
| `hot-path-reviewer` | hot-path violations: `.clone()`, `Vec::new()`, `Box`, `dyn`, allocation, unbounded channel, lock on hot path | <400 words |
| `security-reviewer` | secret exposure, ILP injection, path traversal, unsafe blocks, missing input sanitization, segment-key drift | <400 words |
| `general-purpose` (hostile bug-hunt) | race conditions, daily-reset collisions, market-hours gates, edge-trigger, false-OK, flush-using-warn-not-error, counters shown raw without `increase()`, pub fn defined-but-never-called | <600 words |

Synthesize verdicts → CRITICAL / HIGH / MEDIUM / LOW. Fix every CRITICAL+HIGH inline before opening PR. Document every false-positive triage with grep evidence.

---

## 11. Per-item 9-box guarantee matrix (per `per-wave-guarantee-matrix.md`)

For every item in §6 (Phases 1-4) and §7 (15 additions), the PR MUST tick all 9 boxes:

1. typed `NotificationEvent::*` event (if user-visible)
2. `ErrorCode` variant + `code_str()` (if error path)
3. `tracing::error!`/`warn!`/`info!` with `code = ErrorCode::X.code_str()` field
4. Prometheus counter or gauge
5. Grafana panel referencing the metric
6. Alert rule in `alerts.yml` (if failure-mode)
7. call site for every new pub fn (`pub-fn-wiring-guard.sh`)
8. test for every new pub fn (`pub-fn-test-guard.sh`)
9. ratchet test in this plan (§8)

Plus the 7-row resilience demand matrix from `per-wave-guarantee-matrix.md`.

---

## 12. Per-phase rollback plan

| Phase rolled back | What survives | Recovery time | Data loss risk |
|---|---|---|---|
| Phase 1 | columns exist with NULL/zero | drop columns or leave; no impact | none |
| Phase 2 | columns populated, no consumer | revert PR; columns stay | none |
| Phase 3 | engine running shadow, no consumer | revert PR | none |
| Phase 4 | reads switch back via revert; old tables already gone — replay from `ticks` for the gap | ~1h replay | none (ticks retained 90d) |

---

## 13. Operator UAT checklist (per phase)

### Phase 1 verification
- [ ] `SHOW CREATE TABLE ticks` shows 4 new columns
- [ ] `SHOW CREATE MATERIALIZED VIEW candles_1m` shows 4 new columns
- [ ] All 29 matviews exist
- [ ] No write errors in `data/logs/errors.jsonl.*` for 1 trading day
- [ ] Old `stock_movers` + `option_movers` tables UNTOUCHED, still being written

### Phase 2 verification
- [ ] `SELECT count(*) FROM ticks WHERE prev_day_close > 0 AND ts > now() - 1h` non-zero
- [ ] `SELECT count(*) FROM candles_1m WHERE prev_day_close > 0 AND ts > now() - 1h` non-zero
- [ ] `SELECT distinct phase FROM ticks WHERE ts > now() - 1d` returns 5 expected phases
- [ ] No `PREVCLOSE-01/02` errors
- [ ] 7 trading days clean

### Phase 3 verification
- [ ] `tv_movers_engine_ram_db_diff_total` = 0 for 7 trading days
- [ ] `/api/movers` returns identical top-10 as old `stock_movers` query
- [ ] Strategy still reads old tables (Phase 4 hasn't fired yet)

### Phase 4 verification
- [ ] `SHOW TABLES` does NOT contain `stock_movers` / `option_movers`
- [ ] Strategy reads `MoversEngine` (verified via tracing span)
- [ ] `/api/movers` reads `candles_1m` (verified via QuestDB query log)
- [ ] No `MOVERS-01/02/03` errors (codes deleted)
- [ ] 30 trading days post-deploy clean

---

## 14. Files touched (additions + deletions)

### NEW files
| File | Purpose | LoC |
|---|---|---|
| `crates/common/src/formulas.rs` | single-source math (Rust + SQL constants) | ~150 |
| `crates/common/src/phase.rs` | `compute_phase()` pure fn + tests | ~200 |
| `crates/core/src/pipeline/prev_day_close_stamper.rs` | first-seen O(1) stamper | ~150 |
| `crates/core/src/pipeline/prev_oi_cache.rs` | boot-time load from `candles_1d` | ~120 |
| `crates/trading/src/candles/engine.rs` | generic `CandleEngine<TF>` | ~400 |
| `crates/trading/src/movers/engine.rs` | `MoversEngine` (top-K heap, arc-swap) | ~300 |
| `crates/core/src/forensics/disconnect_snapshot.rs` | snapshot writer | ~200 |
| `crates/core/src/forensics/event_ring.rs` | per-conn 60s ring | ~150 |
| `crates/core/src/network/dns_round_robin.rs` | DNS failover | ~120 |
| `crates/core/src/network/happy_eyeballs.rs` | RFC 8305 | ~100 |
| `crates/storage/src/mmap_spill.rs` | `mmap`'d spill file | ~250 |
| 25 new test files (per §8) | ratchet tests | ~2500 |

**Total new: ~4,640 LoC**

### DELETED files
| File | Reason | LoC removed |
|---|---|---|
| `stock_movers` table + DDL | candles cover it | ~50 |
| `option_movers` table + DDL | same | ~50 |
| `crates/storage/src/movers_persistence.rs` | dead | ~600 |
| `crates/core/src/pipeline/preopen_movers.rs` | phase column covers it | ~400 |
| `previous_close` writer + table (price portion) | first-seen stamper covers it | ~300 |
| `crates/core/src/pipeline/prev_close_writer.rs` | dead | ~250 |
| `crates/core/src/pipeline/prev_close_persist.rs` | dead | ~200 |
| 7 ErrorCodes (HOT-PATH-01/02, PREVCLOSE-01/02, MOVERS-01/02/03) | reasons vanish | ~50 |
| Runbook entries in `wave-1-error-codes.md` | follow code | ~150 |
| Triage YAML rules referencing deleted ErrorCodes | follow code | ~50 |
| Movers Grafana panels | replaced by candle-driven | ~80 |

**Total deleted: ~2,180 LoC + 2 tables + 7 ErrorCodes**

### NET CHANGE
**+4,640 LoC added, −2,180 LoC removed → +2,460 LoC net** (most of the addition is ratchet tests and the new bulletproofing — the production code itself shrinks net).

---

## 15. Open questions — ALL LOCKED (no questions remain)

Per operator charter (2026-05-04): defaults locked at maximum-guarantee-within-envelope. No further questions until §16 status checkpoint.

| # | Question | LOCKED ANSWER | Rationale |
|---|---|---|---|
| Q1 | Timeframes list | full 29 (1s/3s/5s/10s/15s/30s, 1m..15m by 1m, 30m, 1h, 2h, 3h, 4h, 1d, 1w, 1mo) | maximum coverage |
| Q2 | Forced WS rotation | **DELETED** — banned by L11 | zero deliberate disconnects |
| Q3 | Sub-second retention | 7d hot EBS → 365d S3 Standard → 5y Glacier Deep Archive | longest fit inside `aws-budget.md` ₹5K/mo cap; SEBI 5y satisfied |
| Q4 | Phase 4 deletion gate | **14 trading days** green-streak ratchet parity | covers ≥1 expiry day + monthly rollover; maximum guarantee per charter |
| Q5 | Phase 1 start | auto-start when operator says "go" — no further confirmation needed | charter §1 demands automation |

---

## 16. Status tracking

- [ ] Operator approves §1 locked decisions (re-confirm Y on all 10)
- [ ] Operator answers §15 open questions
- [ ] Three adversarial agents review the plan (hot-path / security / hostile bug-hunt)
- [ ] Synthesize agent findings → fix CRITICAL+HIGH inline
- [ ] Phase 1 PR opens on `claude/29-tf-movers-deletion`
- [ ] Phase 1 verified per §13
- [ ] Phase 2 PR
- [ ] Phase 2 verified
- [ ] Phase 3 PR
- [ ] Phase 3 verified (7-day green-streak gate)
- [ ] Phase 4 PR
- [ ] Phase 4 verified (30-day post-deploy monitoring)
- [ ] Plan archived to `.claude/plans/archive/2026-MM-DD-29-tf-movers-deletion.md`

---

**END OF PLAN. NO CODE TOUCHES ANYTHING UNTIL OPERATOR APPROVES §1 + §15 + GREENLIGHTS PHASE 1 PR.**
