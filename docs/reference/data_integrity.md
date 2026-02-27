# Data Integrity — Zero Data Loss

> Extracted from CLAUDE.md. Reference when implementing persistence, orders, or reconciliation.

## Idempotency Rules
Every write operation must be idempotent — running it twice produces the same result.

**QuestDB writes:** Use designated timestamp + security_id as natural dedup key. NEVER rely on insert order.

**Instrument persistence:** Call persist_instrument_snapshot() AT MOST ONCE per calendar day (IST). Caller guards against double invocation. expiry_date is STRING (YYYY-MM-DD), NOT TIMESTAMP. Futures: option_type = '' (empty), strike_price = 0.0 (NOT NULL).

**Valkey writes:** SET with explicit keys (naturally idempotent). Counters: atomic INCR, never GET+SET.

**OMS transitions:** statig state machine rejects invalid transitions at type level. Every transition logged. Append-only in QuestDB.

**Order submission:** Generate idempotency key BEFORE sending. Store in Valkey BEFORE submission. On retry, check Valkey first. CRITICAL: duplicate orders = double financial exposure.

## Deduplication
- **Ticks:** Deduplicate by (security_id, exchange_timestamp, sequence_number). Bounded ring buffer for O(1) lookup. Log duplicates at WARN.
- **Instrument snapshots:** Deduplicate at caller level (scheduler once per IST day). Guard: check last_persisted_date. Recovery: SELECT DISTINCT always correct.
- **Orders:** Deduplicate by idempotency key. On API timeout: CHECK status before retrying. Assume order WENT THROUGH until confirmed otherwise.

## Reconciliation
**End-of-day (15:35 IST):** Fetch all Dhan orders → compare with OMS → flag mismatches (orders in Dhan not OMS = CRITICAL alert, fill quantity mismatch = CRITICAL). Store report in QuestDB.

**Position reconciliation:** Run after every fill. Mismatch = halt trading + alert immediately.

## Data Retention
| Store | Retention |
|-------|-----------|
| QuestDB (hot) | 90 days ticks, all orders forever |
| S3 (cold) | 5-year minimum |
| Loki (hot) | 30 days |
| Loki (cold/S3) | 5 years (SEBI) |
| EBS backups | 30-day snapshots |
