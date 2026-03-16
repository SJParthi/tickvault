---
paths:
  - "crates/trading/**/*.rs"
  - "crates/storage/**/*.rs"
  - "crates/core/src/pipeline/**/*.rs"
---

# Data Integrity Rules

## Idempotency
- Every write operation must be idempotent
- Orders: generate idempotency key in Valkey BEFORE submission
- Duplicate orders = double financial exposure — CRITICAL
- QuestDB: designated timestamp + security_id as natural dedup key

## Tick Deduplication
- Dedup by (security_id, exchange_timestamp, sequence_number)
- Bounded ring buffer for O(1) lookup
- Log duplicates at WARN

## Position Reconciliation
- After every fill: mismatch = halt trading + alert. End-of-day 15:35 IST: full Dhan vs OMS reconciliation.
- Both reconciliation types flag mismatches as CRITICAL

## Retention
- 5 years minimum (SEBI). QuestDB hot: 90 days ticks, all orders forever. S3 cold: 5 years.

## Price Precision Preservation
- Dhan WebSocket sends prices as f32. NEVER use `f64::from(f32)` for QuestDB storage
- ALWAYS use `f32_to_f64_clean()` — converts via shortest decimal string to avoid IEEE 754 widening artifacts
- Example: 10.20_f32 → `f64::from()` → 10.19999980926514 (WRONG) vs `f32_to_f64_clean()` → 10.2 (CORRECT)
- Dhan REST API returns f64 natively — no conversion needed for historical candles
- Materialized views use exact aggregations (first/max/min/last/sum) — corruption enters at input only
- Enforced mechanically: `banned-pattern-scanner.sh` blocks `f64::from()` in `crates/storage/`

## Safety
- SEV-1/SEV-2: halt trading FIRST, diagnose second

## Deep Reference
Read `docs/standards/data-integrity.md` ONLY when implementing persistence or reconciliation.
