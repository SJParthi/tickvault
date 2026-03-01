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

## Safety
- SEV-1/SEV-2: halt trading FIRST, diagnose second

## Deep Reference
Read `docs/reference/data_integrity.md` ONLY when implementing persistence or reconciliation.
