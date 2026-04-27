# O(1) / Uniqueness / Dedup Proof Matrix

> Every new artifact in the 14-item plan, with explicit proof of all three principles. Pre-push gate fails if any cell lacks proof.

## Matrix

| Artifact | O(1) proof | Uniqueness key | Dedup key | Hot path? | Where |
|---|---|---|---|---|---|
| `previous_close` table | Bench `prev_close_write_le_100ns` (Criterion) | `(security_id, exchange_segment, trading_date_ist)` | `DEDUP UPSERT KEYS(trading_date_ist, security_id, segment)` | Yes — write on first Quote/Full per day per (id,segment) | Item 4 |
| `previous_close_persistence::write` | ILP-batched, async-flushed; sync API call ≤ 200ns | same composite | same | Yes | Item 4 |
| `first_seen_set` papaya | `papaya::HashSet<(u32, ExchangeSegment)>` insert ≤ 50ns p99 | composite (id, segment) | in-memory; idempotent insert | Yes | Item 4 |
| First-seen IST-midnight reset task | `tokio::time::sleep_until(next_ist_midnight())` — once per day | N/A | N/A (clears set) | No (background) | Item 4 |
| `stock_movers` ALL ranks | Bench `compute_snapshot_239_stocks_le_1ms` | `(security_id, exchange_segment, ts)` | `DEDUP UPSERT KEYS(ts, security_id, segment)` | Snapshot every 5s, NOT per-tick | Item 2 |
| `option_movers` 22K | Bench `compute_snapshot_22k_options_le_5ms`; partition by 3 underlying buckets if exceeded | `(security_id, exchange_segment, ts)` | `DEDUP UPSERT KEYS(ts, security_id, segment)` | Snapshot every 5s | Item 3 |
| `option_movers` enrichment lookup | `papaya::HashMap<(u32, ExchangeSegment), InstrumentRecord>::get` ≤ 50ns p99 | composite | N/A | Yes (per-row) | Item 3 |
| `preopen_movers` | Bench `preopen_compute_le_1ms` | `(security_id, exchange_segment, ts, phase='PREOPEN')` | `DEDUP UPSERT KEYS(ts, security_id, segment)` (phase column not in dedup key — same row replaces) | 30s cadence | Item 10 |
| `tick_gap_detector` papaya map | `papaya::HashMap<(u32, ExchangeSegment), Instant>`; lookup + insert ≤ 50ns p99 | composite (id, segment) — I-P1-11 | N/A (in-memory) | Yes — every tick | Item 8 |
| `tick_gap_detector` daily reset | `tokio::time::sleep_until(15:35 IST)` | N/A | N/A | No | Item 8 (G19) |
| `subscribe_rx_guard` reinstall (existing, hardening) | Drop trait O(1); zero allocation | per-connection | N/A | Reconnect path | Item 5 |
| Phase2 emit guard | Drop trait — panics in debug if dropped without firing; `bool` flag check is O(1) | once-per-day per `trading_date_ist` | In-memory bitset reset at IST 00:00 | One-shot event per day | Item 1 (G7) |
| `MovesSnapshot` arc-swap | `arc_swap::Cache::load()` ≤ 50ns p99 — lock-free reads | one-shot per swap | last-write-wins | Yes (5s swap) | Item 0 |
| Hot-path async writer (PrevClose cache) | mpsc enqueue ≤ 200ns p99; consumer in `spawn_blocking` | per-path | N/A | Yes | Item 0 |
| Spill writer async wrapper | mpsc bounded queue capacity 8192; enqueue ≤ 200ns; drop-oldest-on-overflow | per-record | N/A | Yes | Item 0 |
| `phase2_audit` row | ILP write batched | `(trading_date_ist, ts)` | `DEDUP UPSERT KEYS(trading_date_ist, ts)` | One-shot daily | Item 9 |
| `depth_rebalance_audit` | ILP write batched | `(underlying_symbol, ts)` | `DEDUP UPSERT KEYS(underlying_symbol, ts)`; `PARTITION BY DAY` | 60s cadence; not hot | Item 9 |
| `ws_reconnect_audit` | ILP write batched | `(connection_id, ts)` | same | Reconnect path; not hot per-tick | Item 9 |
| `boot_audit` | ILP write batched | `(boot_id ULID, step)` | `DEDUP UPSERT KEYS(boot_id, step)` | Boot only | Item 9 |
| `selftest_audit` | ILP write batched | `(trading_date_ist, check_name)` | same | 09:16 IST one-shot | Item 9 |
| `order_audit` | ILP write batched | `(order_id, ts, leg)` | same | Order path; ≤ 10 orders/sec | Item 9 |
| Token-refresh-on-wake | `arc_swap::Cache::load()` for token read O(1); refresh-if-stale check is O(1) | N/A | N/A | Wake from sleep — once per sleep cycle | Item 5 (G1) |
| Telegram coalescer bucket lookup | `papaya::HashMap<(EventKind, CoalesceKey), CoalesceState>::get` ≤ 50ns p99 | `(EventKind, CoalesceKey)` per window | last-emit-time map | Every event | Item 11 (G15) |
| Telegram global GCRA bucket | `governor::Quota::permit()` O(1) atomic CAS | N/A | N/A | Every event | Item 11 |
| Telegram bounded queue overflow | mpsc capacity 1024; drop-oldest with counter | N/A | N/A | Every event | Item 11 |
| 12 sub-gauges → score | Single Prometheus query evaluation; scrape O(N gauges) where N=12 fixed | N/A | N/A | Every 30s scrape | Item 13 |

## Banned constructs on hot path (ratchet category 7)

| Pattern | Allowed | Rejected on hot path |
|---|---|---|
| `std::fs::*` | tests, init, drain | `crates/core/src/pipeline/`, `crates/storage/src/tick_persistence.rs` hot fns |
| `BufWriter::<File>::new` | drain background tasks | spill writer hot fn (must wrap in async channel) |
| `RwLock::write()` | cold paths | tick path, snapshot swap (use `arc-swap`) |
| `HashSet<u32>`, `HashMap<u32, _>` | single-segment with `// APPROVED:` comment | any cross-segment dedup or lookup |
| `Vec::with_capacity` then `push` in hot loop | non-hot | use `ArrayVec<[T; N]>` instead |
| `.clone()` on hot path | non-hot | use `Arc::clone` or borrow |
| `dyn Trait` on hot path | non-hot | use generic monomorphization |
| `tokio::sync::Mutex` on hot path | non-hot | use `parking_lot::Mutex` (cold) or arc-swap (hot) |

## Composite-key enforcement (I-P1-11)

Every NEW collection in this plan that stores or looks up instruments uses `(security_id, exchange_segment)`:

| Collection | File | Key type |
|---|---|---|
| `first_seen_set` | `crates/core/src/pipeline/first_seen_set.rs` | `papaya::HashSet<(u32, ExchangeSegment)>` |
| `tick_gap_detector` last-tick-seen map | `crates/core/src/pipeline/tick_gap_detector.rs` | `papaya::HashMap<(u32, ExchangeSegment), Instant>` |
| `option_movers` enrichment lookup | uses existing `InstrumentRegistry::get_with_segment(id, segment)` | composite |
| All 6 audit tables (where security_id present) | DEDUP keys include `segment` | composite |

`crates/storage/tests/dedup_segment_meta_guard.rs` extended to cover all 6 new audit tables + `previous_close`.

## DEDUP-key catalogue (every new QuestDB table)

```
previous_close          → DEDUP UPSERT KEYS(trading_date_ist, security_id, segment)
stock_movers (extended) → DEDUP UPSERT KEYS(ts, security_id, segment)
option_movers           → DEDUP UPSERT KEYS(ts, security_id, segment)
phase2_audit            → DEDUP UPSERT KEYS(trading_date_ist, ts)
depth_rebalance_audit   → DEDUP UPSERT KEYS(underlying_symbol, ts)
ws_reconnect_audit      → DEDUP UPSERT KEYS(connection_id, ts)
boot_audit              → DEDUP UPSERT KEYS(boot_id, step)
selftest_audit          → DEDUP UPSERT KEYS(trading_date_ist, check_name)
order_audit             → DEDUP UPSERT KEYS(order_id, ts, leg)
```

Every key includes the disambiguator. `dedup_segment_meta_guard.rs` source-scan ratchets all 9 statements above.

## Bench-budget catalogue

```toml
# quality/benchmark-budgets.toml additions
prev_close_handler_p99_ns = 100
movers_snapshot_swap_p99_ns = 50
spill_write_enqueue_p99_ns = 200
compute_snapshot_239_stocks_p99_ms = 1
compute_snapshot_22k_options_p99_ms = 5
option_mover_enrichment_lookup_p99_ns = 50
tick_gap_lookup_p99_ns = 50
preopen_compute_p99_ms = 1
greeks_lag_with_movers_running_p99_ms = 50
```

`scripts/bench-gate.sh` enforces 5% regression cap across all 9.
