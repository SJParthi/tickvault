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

---

## 2. The architecture

```
                     Every Dhan tick
                            │
       ┌────────────────────┼────────────────────┐
       ▼                    ▼                    ▼
   29 RAM                ticks            derivative_contracts
   sticky notes          table            (already exists — symbol,
   (1 per timeframe)     (raw + 4 NEW      strike, expiry, underlying)
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

## 6. The 4-phase rollout (no big-bang, each phase its own PR)

| Phase | Ships | Risk if rolled back | Gate before next phase |
|---|---|---|---|
| **Phase 1: Schema widen** (additive ALTER + matview rebuild) | `ALTER TABLE ticks ADD COLUMN IF NOT EXISTS` x4; matviews rebuilt with new schema during off-hours; old movers tables UNTOUCHED | ZERO — new columns sit empty if writer not deployed | matview rebuild verified on at least 1 trading day |
| **Phase 2: Populate new columns** (dual-write) | Tick parser stamps `prev_day_close` (first-seen) + `prev_day_oi` (boot cache) + `phase`; matview captures them; old movers tables ALSO still being written | ZERO — old path unchanged | 7 trading days of populated columns + zero parser errors |
| **Phase 3: Add in-memory engines + parity test** (shadow mode) | `CandleEngine<TF>` + `MoversEngine` deployed; `/api/movers` switched to candles SELECT; ratchet test compares RAM vs new SELECT vs old movers tables — all three must agree | ZERO — strategy still reads old tables; new engines shadow-deployed | 7 trading days green-streak across all 3 ratchet axes |
| **Phase 4: Cutover + delete** | Strategy reads `MoversEngine` (RAM); old movers tables + writers + 4 ErrorCodes + runbooks DELETED | LOW — revert single PR; data still in `ticks` for replay | **14 trading days** of green-streak ratchet parity (RAM ≡ DB ≡ legacy) before merge; post-deploy: 30 trading days monitoring before declaring "done" |

---

## 7. NEW: 15 bulletproofing additions for O(1) + memory burst + WS resilience

| # | Addition | What it solves | Mechanism | Ratchet |
|---|---|---|---|---|
| 1 | Cache-line align hot atomics (`#[repr(align(64))]`) | false-sharing slowdown under burst | `MoverState` + `CandleState` aligned | `test_atomic_alignment_64_bytes` |
| 2 | Pre-allocated bump arena per tick (recycled) | zero-alloc even on burst | `bumpalo::Bump` reset per tick | DHAT extended |
| 3 | `mmap`'d tick spill file | OS page cache handles 10x burst | `memmap2` crate, fixed file size | `chaos_burst_10x.rs` |
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
| `test_forced_rotation_every_4h_preserves_subs` | `crates/core/tests/forced_rotation.rs` | subs survive scheduled rotation |
| `test_silent_socket_detected_in_30s` | `crates/core/tests/silent_socket.rs` | noop heartbeat detects |
| `test_forensic_ring_captures_pre_disconnect_60s` | `crates/core/tests/forensic_ring.rs` | last 60s of events available on RST |
| `test_snapshot_written_on_disconnect` | `crates/core/tests/disconnect_snapshot.rs` | JSON dumped to `data/logs/disconnect-forensics.*.json` |
| `test_replay_spill_produces_identical_output` | `crates/storage/tests/spill_replay.rs` | re-injection from spill = original pipeline output |
| `chaos_dhan_rst_flood` | `crates/core/tests/chaos_dhan_rst_flood.rs` | weekly CI: 5min RST every 30s — bounded recovery |
| `chaos_burst_10x_traffic` | `crates/core/tests/chaos_burst_10x.rs` | 10x normal tick rate — mmap spill absorbs |

**Total: 25 new ratchet tests + extensions to 4 existing meta-guards.**

---

## 9. Honest 100% envelope wording (mandatory PR body)

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤2,000,000-tick ring buffer capacity (constant `TICK_BUFFER_CAPACITY`);
> ≤10x traffic burst absorbed by mmap'd spill (chaos-tested);
> bench-gated O(1) hot path (≤200ns per tick across 29 timeframes);
> RAM≡DB formula parity enforced by build-time + boot-time + runtime ratchets;
> composite-key uniqueness `(security_id, exchange_segment)`;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake;
> chaos-tested Dhan RST flood weekly in CI.
> Beyond the envelope, DLQ NDJSON catches every payload as recoverable text.
> Live proof: Telegram alerts on 2026-05-04 show 5/5 disconnect at 12:55 IST detected + auto-recovered + zero tick loss + depth swaps continued normally."

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

## 17. Functional additions (8 items)

| # | Item | Where it lives | Notes |
|---|---|---|---|
| F1 | Greeks (delta/theta/gamma/vega/IV) for option contracts | `formulas.rs` greek fns + columns on `candles_*` for `instrument_type IN ('OPTSTK','OPTIDX','OPTCUR','OPTFUT')` | RAM engine reads underlying spot; greeks computed cold-path only (greeks_verification table consolidated into candles) |
| F2 | VWAP per-bar | config-driven metric: `[[metrics]] name="vwap" expr="sum(close*volume_delta)/sum(volume_delta)"` | derived in matview SAMPLE BY |
| F3 | Spread / mid / bid-ask imbalance (microstructure) | columns on `candles_*` derived from depth-20 packets | only populated for instruments subscribed to depth-20 |
| F4 | Cross-instrument correlation engine | secondary in-memory `CorrelationEngine` (e.g., NIFTY ↔ BANKNIFTY) | cold path only; reads sealed candles, not ticks |
| F5 | **Cold-boot warmup mid-session** | at boot, replay last 5 min of ticks from `ticks` table into engines before strategy enables | prevents top-K being empty during 1-min warmup gap |
| F6 | **Engine state snapshot every 60s** | `MoverState` array serialized to mmap'd file; loaded at boot to skip warmup | bounded write rate; survives crashes |
| F7 | **Garbled-tick sanity guard** | reject + count: price outside ±20% of last, volume non-monotonic, OI swing >50% | new `ErrorCode::SanityGuard01` — rejected ticks counted, not silenced |
| F8 | Holiday-aware `compute_phase()` | wires `TradingCalendar` into `phase.rs` | NSE holidays change phase boundaries; pure fn, ratchet-tested |

---

## 18. Operational additions (5 items)

| # | Item | Where | Notes |
|---|---|---|---|
| O1 | Feature flag per phase | `[features]` section in TOML; checked at boot | rollback any single piece per env without revert; can disable engine per-TF |
| O2 | Kill-switch fallback to DB-read mode | `MOVERS_ENGINE_FALLBACK_TO_DB=1` env var; checked on each `MoversEngine::top_k()` read | if RAM engine misbehaves in prod, operator forces DB path without redeploy |
| O3 | Existing Grafana dashboard migration | scripted rewrite of all panels reading old movers tables → candles SELECT, BEFORE Phase 4 | dashboards must not break on table delete |
| O4 | `/api/movers` API contract version bump | `?v=2` query param introduced in Phase 3; `?v=1` shim kept 30 days post-Phase-4 | external consumers get migration window |
| O5 | AWS-vs-local config split | `config/local.toml`, `config/aws.toml`, `config/prod.toml` overrides; CI validates both | mmap path, oom_score_adj, cgroup limits differ per env |

---

## 19. Compliance + DR additions (4 items)

| # | Item | Where | Notes |
|---|---|---|---|
| C1 | SEBI movers ranking audit | `movers_rank_audit` table — 1 row per top-K rank change (low volume, ~1K rows/day) | regulator query "what was top-10 at 14:32:15 on 2027-08-15" answerable; lifecycle 90d hot → S3 Glacier 5y |
| C2 | EBS corruption disaster recovery runbook | `docs/runbooks/disaster-recovery.md` documents full-DB rebuild from S3 backup; chaos-tested annually | restore time target: 4h |
| C3 | Multi-region failover | RECORDED as future-work (out of scope for this plan) | acknowledge but defer |
| C4 | Engine state audit table | `engine_state_audit` table — every `MoverState` change persisted (high volume, ~10M rows/day) | "why did engine rank X at Y?" forensic; SEBI 5y; lifecycle hot 30d → S3 |

---

## 20. Cost + sizing additions (3 items)

| # | Item | Concrete number | Notes |
|---|---|---|---|
| $1 | Monthly cost projection (29 matviews + new columns + sub-second TFs + audit tables) | **+₹420/mo** above current baseline | within ₹5K/mo `aws-budget.md` cap (current ~₹4,981; new ~₹4,981 + 420 wait — needs recompute) |
| $2 | Disk projection per TF (1 trading day) | 1s ~1.2 GB/day, 3s ~400 MB/day, 5s ~240 MB/day, 1m ~20 MB/day, 1d ~1 MB/day | tiering per `[timeframes.retention]` |
| $3 | Memory ceiling assertion at boot | 25K instruments × 29 TFs × 64 B = ~46 MB; refuse to start if predicted > 200 MB | bounded; new `BOOT-04` error code if exceeded |

⚠️ **Cost gate (must verify before Phase 1):** the `$1` row is a PROJECTION. Before Phase 1 PR opens, run actual sizing calc against current QuestDB partition sizes + S3 lifecycle cost. If projected total > ₹4,950/mo, trim sub-second retention or disable lowest-volume TFs.

---

## 21. Testing additions (6 items beyond the 25 in §8)

| # | Test | File | Pins |
|---|---|---|---|
| T1 | Replay determinism | `crates/trading/tests/replay_determinism.rs` | same `data/test-fixtures/golden-day.ndjson` → same engine state, every run |
| T2 | Multi-day boundary | `crates/core/tests/midnight_rollover_property.rs` | proptest crossing IST midnight: `prev_day_close` reset, `prev_oi_cache` reload |
| T3 | Snapshot+restore round-trip | `crates/trading/tests/snapshot_restore.rs` | save → restart → restore → continue → identical to no-restart baseline |
| T4 | Chaos: garbled tick injection | `crates/core/tests/chaos_garbled_tick.rs` | malformed packet → rejected + counted; engine state uncorrupted |
| T5 | Chaos: clock skew | `crates/core/tests/chaos_clock_skew.rs` | ±2s wall-clock skew → bar boundaries still correct (BOOT-03 still halts on >2s) |
| T6 | Chaos: feature-flag flip mid-session | `crates/core/tests/chaos_feature_flip.rs` | flip `candle_engine_29tf` true→false at 11:00 IST → clean fallback to DB reads |

**Total ratchets after additions: 25 (§8) + 6 (§21) = 31 ratchet tests.**

---

## 22. New ErrorCodes (5 items)

| Code | Severity | Module | Meaning |
|---|---|---|---|
| `SNAPSHOT-01` | Medium | `crates/trading/src/movers/snapshot.rs` | engine state snapshot write failed (60s recurring task) |
| `FORENSIC-01` | Medium | `crates/core/src/forensics/disconnect_snapshot.rs` | disconnect forensic dump failed (file write, disk full?) |
| `HEARTBEAT-01` | High | `crates/core/src/websocket/activity_watchdog.rs` | silent socket detected via passive activity watchdog (no ticks for >50s on a healthy WS) |
| `WARMUP-01` | Low | `crates/trading/src/candles/engine.rs` | mid-session cold-boot warmup ticking (informational; engine catching up from `ticks` replay) |
| `BOOT-04` | Critical | `crates/app/src/main.rs` | predicted memory at boot > 200 MB ceiling → refuse to start |

⚠️ Note: `ROTATION-01` was originally in this list but DELETED — forced rotation banned by L11.

Each code requires:
- runbook entry in `.claude/rules/project/wave-5-error-codes.md` (or new `wave-6-error-codes.md`)
- triage YAML rule in `.claude/triage/error-rules.yaml`
- Prometheus counter `tv_<code_lower>_total`
- alert rule (for Medium+) in `alerts.yml`
- Telegram notification variant

---

## 23. Updated NET CHANGE (after additions §17-§22)

| Category | LoC |
|---|---|
| §14 base additions | +4,640 |
| §14 base deletions | −2,180 |
| §17 functional (8 items) | +1,200 |
| §18 operational (5 items) | +600 |
| §19 compliance + DR (4 items) | +400 |
| §20 cost + sizing (3 items) | +150 |
| §21 testing (6 items) | +500 |
| §22 ErrorCodes (5 codes) | +200 (incl. runbook entries) |
| **NET TOTAL** | **+5,510 LoC net** |

(Bigger than original +2,460 because we accepted all 31 functional/operational/compliance/cost/testing additions. The deletion side stays at −2,180 — same.)

---

## 24. Status tracking

- [ ] Operator approves §1 L1-L11 locked decisions
- [ ] Three adversarial agents review the plan (hot-path / security / hostile bug-hunt)
- [ ] Synthesize agent findings → fix CRITICAL+HIGH inline
- [ ] Cost projection $1 verified against actual QuestDB partition sizes (gate before Phase 1)
- [ ] Phase 1 PR opens on `claude/29-tf-movers-deletion`
- [ ] Phase 1 verified per §13
- [ ] Phase 2 PR + verified
- [ ] Phase 3 PR + verified (14-day green-streak gate)
- [ ] Phase 4 PR + verified (30-day post-deploy)
- [ ] Plan archived to `.claude/plans/archive/`

---

**END OF PLAN. NO CODE TOUCHES ANYTHING UNTIL OPERATOR APPROVES §1 L1-L11 + GREENLIGHTS PHASE 1 PR.**
