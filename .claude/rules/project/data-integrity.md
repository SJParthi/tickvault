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
- **QuestDB `ticks` DEDUP UPSERT KEY = `(ts, security_id, segment, capture_seq, feed)`**
  (`DEDUP_KEY_TICKS` in `crates/storage/src/tick_persistence.rs`) — TICK-SEQ-01.
  - `ts` = `exchange_timestamp × 1e9` (Dhan LTT, SECOND-granular).
  - `segment` — mandatory (I-P1-11; `security_id` reused across segments).
  - `feed` — broker source label (`'dhan'`/`'groww'`/…), added 2026-06-19
    (operator "same tables + feed column"). Part of the key so a Dhan tick and a
    Groww tick for the SAME `(ts, security_id, segment, capture_seq)` are BOTH
    kept (distinct feeds = distinct observations, never a duplicate). Replay-
    stable (constant per writer — the Dhan `TickPersistenceWriter` stamps
    `TICK_FEED_DHAN='dhan'`), so it does NOT break the capture_seq replay-
    idempotency guarantee. Pinned by `test_dedup_key_ticks_exact_format` +
    `test_tick_row_stamps_feed_dhan_symbol`.
  - `capture_seq` — the sub-second tiebreaker AND the replay-idempotency key. It
    is a strictly-monotonic, **replay-stable** sequence stamped ONCE at the WS
    read instant (`ws_frame_spill::next_frame_seq` = `max(prev+1, wall_nanos)`,
    1 frame = 1 tick) and carried **unchanged** through RAM → ring → spill → DLQ
    → WAL → DB:
    - two DISTINCT arrivals get DISTINCT `capture_seq` → BOTH kept (no loss),
      **even when every value field is byte-identical** — this is the fix for the
      live index loss `NIFTY 23,146.45 → 23,146.75 → 23,146.45` (volume=0 indices
      have no per-tick variation, so content alone cannot distinguish them);
    - a true duplicate / WAL-replay / reconnect re-send reuses the **SAME**
      `capture_seq` (read back from the `TVW2` WAL record, NOT re-stamped) → same
      key → collapsed (idempotent, replay-safe).
  - **`payload_hash` is NOT in the key** (TICK-SEQ-01 PR-2b): it COLLAPSED
    same-value same-second index ticks (real data loss). It remains a stored
    **content-integrity** column (and `tick_payload_hash` a deterministic
    fingerprint function), just no longer the dedup tiebreaker.
  - **`received_at` is NOT in the key**: re-stamped at processing time, so on
    replay it differs → would create duplicate rows. Stored column only.
  - **Key-flip migration is in-place + fail-safe**: `ensure_tick_table_dedup_keys`
    re-runs `DEDUP ENABLE UPSERT KEYS(...)` with the new key set; the stale-schema
    DROP-recovery path **NEVER drops a POPULATED `ticks` table** (SEBI retention)
    — `ticks_table_is_populated` gates it (`tv_ticks_dedup_drop_blocked_total`).
- `capture_seq` MUST stay strictly-monotonic + WAL-persisted (replay-stable);
  a per-process counter that resets on restart would break cross-restart
  idempotency — it is seeded from wall-clock nanos.
- Regression-pinned by `chaos_index_same_value_burst_preserved.rs` (the
  `45→75→45` test + replay-twice idempotency) + `test_dedup_key_is_capture_seq_after_flip`.
- Bounded ring buffer for O(1) lookup; log duplicates at WARN.

## Per-Feed Identity in DEDUP keys (operator 2026-06-23)

> **Full architecture reference:** `docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md`
> is the permanent single source of truth for O(1) per-feed uniqueness / dedup /
> mapping / latency (verified 2026-06-24; drift-guarded by
> `crates/storage/tests/o1_per_feed_doc_guard.rs`).

Every table holding genuinely PER-FEED data carries `feed` (`dhan`/`groww`, from
`tickvault_common::feed::Feed`) IN its DEDUP UPSERT key, so a Dhan row and a
Groww row for the same logical key are BOTH kept (distinct feeds = distinct
observations), never collapsed. The four feed-keyed market-data tables:

| Table | DEDUP key |
|-------|-----------|
| `ticks` | `security_id, segment, capture_seq, feed` (+ designated `ts`) |
| `candles_*` (21) | `ts, security_id, segment, feed` |
| `prev_day_ohlcv` | `ts, security_id, segment, feed` |
| `ws_event_audit` | `ts, trading_date_ist, feed, ws_type, connection_index, event_kind` |

Pinned by `dedup_segment_meta_guard::per_feed_market_data_dedup_keys_must_include_feed`
(removing `feed` from any of these fails the build). Existing tables self-heal
via `ALTER … ADD COLUMN IF NOT EXISTS feed` + `DEDUP ENABLE UPSERT KEYS(…)` at
boot — never a populated-table DROP (SEBI).

**`feed` is a LABEL, NOT a key, where the row is not per-feed:**
`cross_verify_1m_audit` (Dhan-only; Groww has its own table),
`tick_conservation_audit` (single combined cross-feed reconciliation), and the
shared instrument-master / universe tables (one universe both feeds watch —
keying by feed would duplicate the universe, breaking I-P1-11's single-universe
model).

## Position Reconciliation
- After every fill: mismatch = halt trading + alert. End-of-day (immediately after WebSocket disconnect at 15:30 IST): full Dhan vs OMS reconciliation.
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

## WebSocket Timestamp Rule — NEVER ADD +5:30 TO ts

**THIS IS THE SINGLE MOST CRITICAL DATA INTEGRITY RULE.**

Dhan WebSocket sends `exchange_timestamp` (LTT) as **IST epoch seconds**.
The designated QuestDB timestamp `ts` stores this value **DIRECTLY** — multiply
by 1,000,000,000 for nanos. **NEVER add +19800 (IST offset) to ts.**

- `ts` = `exchange_timestamp * 1_000_000_000` (IST epoch, NO offset)
- `received_at` = `Utc::now() + IST_UTC_OFFSET_NANOS` (UTC→IST conversion, offset OK)
- `exchange_timestamp` LONG column = raw IST epoch seconds verbatim

**ONLY historical REST API timestamps (UTC epoch) get +19800.** WebSocket never.

Adding +5:30 to WebSocket timestamps corrupts:
- Candle aggregation (wrong time buckets)
- Market hour detection (trades appear outside 09:15-15:30)
- P&L calculations (wrong trade sequence)
- SEBI audit trail (timestamps off by 5.5 hours)

**Mechanical enforcement:**
- `test_critical_ws_timestamp_no_ist_offset_on_ts` — verifies output
- `test_critical_source_no_ist_offset_in_ts_computation` — scans source code
- Pre-commit hook: `banned-pattern-scanner.sh` blocks IST offset patterns in ts computation

**If any test with "CRITICAL" in its name fails on tick timestamps: REVERT IMMEDIATELY.**

## Greeks Pipeline Timestamp Rule — ALWAYS ADD +5:30 TO ts

**THIS IS A CRITICAL DATA INTEGRITY RULE FOR GREEKS TABLES.**

Greeks pipeline uses `Utc::now()` (system clock UTC) for timestamps.
This is the same source as `received_at` in tick persistence.
**ALWAYS add IST_UTC_OFFSET_NANOS** to convert UTC → IST for QuestDB display.

Applies to ALL 4 tables:
- `option_greeks`
- `dhan_option_chain_raw`
- `greeks_verification`
- `pcr_snapshots`

Pattern:
```rust
let now_nanos = Utc::now()
    .timestamp_nanos_opt()
    .unwrap_or(0)
    .saturating_add(IST_UTC_OFFSET_NANOS);
```

Today's date for time-to-expiry MUST use IST timezone:
```rust
let today = (Utc::now() + TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64)).date_naive();
```

**Why this differs from WebSocket:** WebSocket `exchange_timestamp` is already IST.
`Utc::now()` is UTC and needs conversion. Different sources, different rules.

**Mechanical enforcement:**
- `test_critical_greeks_timestamp_includes_ist_offset` — scans source code for IST_UTC_OFFSET_NANOS
- `test_critical_greeks_today_uses_ist` — scans source code for IST_UTC_OFFSET_SECONDS_I64

**If any test with "CRITICAL" in its name fails on greeks timestamps: REVERT IMMEDIATELY.**

## Safety
- SEV-1/SEV-2: halt trading FIRST, diagnose second

## Deep Reference
Read `docs/standards/data-integrity.md` ONLY when implementing persistence or reconciliation.
