---
paths:
  - "crates/core/**/*.rs"
  - "crates/trading/**/*.rs"
---

# Hot Path Rules — Zero Allocation Enforcement

These rules apply to all performance-critical crates. Violations are bugs.
When `crates/websocket/` and `crates/oms/` are created, add them to the paths above.

## Mandatory
- Zero heap allocation: no Box, Vec::new(), String::new(), format!(), .collect() on hot path
- Use zerocopy for zero-copy parsing, arrayvec for bounded stack collections
- papaya for concurrent hot-path lookups (NOT DashMap)
- rtrb for SPSC wait-free channels, crossbeam for MPMC
- enum_dispatch instead of dyn Trait (eliminates vtable indirection)
- No .clone() unless explicitly approved by Parthiban
- No .expect() or .unwrap() — use ? with proper error types
- No blocking I/O (std::fs, std::net, thread::sleep) — use async equivalents

## Performance Targets
- Tick parse: <10ns
- Signal processing: <10μs
- OMS state transition: <100ns
- API round-trip: 5-50ms

## Patterns
```rust
// WRONG — heap allocation
let msg = format!("tick {}", id);
let data: Vec<u8> = raw.to_vec();
let items: Vec<_> = source.iter().collect();

// RIGHT — stack/zerocopy
let msg: ArrayString<64> = ArrayString::from_str("tick").unwrap();
let data: &[u8] = raw;  // borrow, don't copy
// pre-allocate or use ArrayVec for bounded collections
```

## Bounded Everything
- All HashMap/Vec MUST have bounded capacity (with_capacity or ArrayVec)
- All channels MUST have bounded capacity (never unbounded)
- >5% benchmark regression = build FAILS

## Exemptions
- Use `// O(1) EXEMPT: <reason>` comment to justify legitimate O(n) operations (e.g., startup initialization)
- All exemptions reviewed during code review
