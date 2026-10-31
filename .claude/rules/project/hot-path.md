---
paths:
  - "crates/core/src/websocket/**/*.rs"
  - "crates/core/src/pipeline/**/*.rs"
  - "crates/trading/**/*.rs"
---

# Hot Path Rules — Zero Allocation Enforcement

Violations are bugs. When `crates/oms/` is created, add it to paths above.

## Mandatory (hooks enforce mechanically at commit)
- Zero heap allocation: no Box, Vec::new(), String::new(), format!(), .collect() on hot path
- zerocopy for zero-copy parsing, ArrayVec for bounded stack collections
- papaya for concurrent hot-path lookups (NOT DashMap)
- rtrb for SPSC wait-free channels, crossbeam for MPMC
- enum_dispatch instead of dyn Trait (eliminates vtable indirection)
- No .clone() unless Parthiban approves
- No .expect() or .unwrap() — use ? with proper error types
- No blocking I/O (std::fs, std::net, thread::sleep) — use async equivalents

## Performance Targets
| Operation | Budget |
|-----------|--------|
| Tick parse | <10ns |
| Signal processing | <10us |
| OMS state transition | <100ns |
| API round-trip | 5-50ms |

## Bounded Everything
- All HashMap/Vec: with_capacity or ArrayVec
- All channels: bounded capacity (never unbounded)
- >5% benchmark regression = build FAILS

## Exemptions
- `// O(1) EXEMPT: <reason>` to justify legitimate O(n) (e.g., startup initialization)
- All exemptions reviewed during code review

## 2026-10-30 — our own ILP encoder for tick rows (plan ITEM DT)

Owner decision 2026-10-30 ("Yes, build our own"). `crates/storage/src/tick_ilp.rs`
builds `ticks`, `tick_quotes` and `feed_aux_packets` rows without the
questdb-rs buffer when `[tick_storage] own_ilp_encoder = true` (default OFF).
- Every row layout is written ONCE over the `IlpLine` trait; the library buffer
  and our encoder implement it, so column order and separators cannot drift.
  Never add a second copy of a row layout for one encoder.
- Per row: O(bytes written), no allocation while the buffer has room (DHAT
  gate `dhat_tick_row_append`, both encoders). A price that is a whole number
  of paise (|v| < 1e13) is written from that count; any other `f64` goes
  through a reused library buffer (`#[cold]`), so its text is the library's.
- Budget `tick_row_append` = 1,000 ns per row, median and per-row p99 (the
  bench exits 1 over it). Never widen the fast path without the exhaustive
  f32 check (`test_exact_cents_every_f32_price_below_2_pow_17_matches_the_library`)
  and the random-bits proptest passing.
