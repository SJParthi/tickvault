# Guarantee Statement — tickvault

## 1. Measured Latency Guarantees

All budgets are enforced by CI Stage 5 (Criterion benchmarks). Exceeding the absolute cap **or** regressing >5% from baseline = automatic build failure.

| Operation | Budget | Crate | Benchmark |
|-----------|--------|-------|-----------|
| Tick binary parse | < 10 ns | `core` | `tick_binary_parse` |
| Tick pipeline routing | < 100 ns | `core` | `tick_pipeline_routing` |
| papaya HashMap lookup | < 50 ns | `core` | `papaya_lookup` |
| Full tick processing (parse + filter + persist) | < 10 us | `core` | `full_tick_processing` |
| OMS state transition | < 100 ns | `trading` | `oms_state_transition` |
| Market hour validation | < 50 ns | `common` | `market_hour_validation` |
| Config TOML load | < 10 ms | `common` | `config_toml_load` |

Source: `quality/benchmark-budgets.toml`. Max regression: 5%.

## 2. O(1) Proof Map

Each hot-path operation mapped to its code location with justification.

### Parser Layer

| Operation | File | Why O(1) |
|-----------|------|----------|
| Header parse | `crates/core/src/parser/header.rs` | 4 fixed `from_le_bytes` reads at constant offsets. Length check is one comparison. |
| Ticker parse | `crates/core/src/parser/ticker.rs` | 2 `from_le_bytes` reads (LTP f32, LTT u32). Size check is one comparison. |
| Quote parse | `crates/core/src/parser/quote.rs` | 11 `from_le_bytes` reads at fixed offsets. No loops. |
| Full packet parse | `crates/core/src/parser/full_packet.rs` | Fixed reads + 5 depth levels (constant, not data-dependent). |
| Market depth parse | `crates/core/src/parser/market_depth.rs` | 5 levels x 4 fields = 20 fixed reads. Level count is a compile-time constant. |
| OI parse | `crates/core/src/parser/oi.rs` | 1 `from_le_bytes` read (u32). |
| Previous close parse | `crates/core/src/parser/previous_close.rs` | 2 `from_le_bytes` reads (f32 + u32). |
| Dispatcher | `crates/core/src/parser/dispatcher.rs` | Single `match` on `response_code` (u8). Routes to one parser. No fallthrough iteration. |

### Pipeline Layer

| Operation | File | Why O(1) |
|-----------|------|----------|
| Junk filter | `crates/core/src/pipeline/tick_processor.rs:203-218` | 3 comparisons: `is_finite()`, `<= 0.0`, `< MINIMUM_VALID_EXCHANGE_TIMESTAMP`. |
| Dedup ring | `crates/core/src/pipeline/tick_processor.rs` (`TickDedupRing`) | FNV-1a hash (3 multiplies + XORs) → 1 array index → 1 equality check. |
| Depth validation | `crates/core/src/pipeline/tick_processor.rs` (`depth_prices_are_finite`) | 10 `is_finite()` checks (5 levels x 2 prices). Fixed count. |
| Candle aggregation | `crates/core/src/pipeline/candle_aggregator.rs` | HashMap lookup + 4 f32 min/max comparisons per tick. |
| Top movers update | `crates/core/src/pipeline/top_movers.rs` | HashMap lookup + change_pct computation (2 arithmetic ops). |

### Trading Layer

| Operation | File | Why O(1) |
|-----------|------|----------|
| OMS state transition | `crates/trading/src/oms.rs` | Enum match on current state → new state. No collection iteration. |

## 3. What Cannot Be Guaranteed

These are external to the system and outside our control:

- **Network latency** to Dhan WebSocket servers (exchange-side, ISP-dependent)
- **QuestDB write latency** under backpressure (external service, disk I/O bound)
- **OS scheduling jitter** on non-RT kernels (context switches, interrupts)
- **Docker container scheduling** overhead (cgroup allocation, namespace transitions)
- **Dhan feed delivery timing** (exchange dissemination schedule, tick batching)

## 4. Enforcement

1. **CI Stage 5:** Criterion benchmarks run on every PR. Results compared against `quality/benchmark-budgets.toml` absolute caps.
2. **Regression gate:** >5% regression from baseline = automatic build failure.
3. **Compile-time lints:** `clippy::arithmetic_side_effects` denied workspace-wide, preventing accidental O(n) via unchecked loops on hot path.
4. **Code review rules:** `.claude/rules/` auto-loaded per path, enforcing no `.clone()`/`DashMap`/`dyn` on hot path.
5. **Const assertions:** Packet sizes and byte offsets verified at compile time via `const _: () = assert!(...)` in `constants.rs`.
