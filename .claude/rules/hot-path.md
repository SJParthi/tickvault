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
