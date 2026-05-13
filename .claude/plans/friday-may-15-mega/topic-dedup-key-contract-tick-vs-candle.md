# DEDUP Key Contract — Ticks vs Candles (Worst-Case Sweep)

**Status:** LOCKED 2026-05-13
**Authority:** I-P1-11 (`security-id-uniqueness.md`) + `STORAGE-GAP-01`/`STORAGE-GAP-02` + Phase 0 Item 30
**Scope:** Every QuestDB write path in the system. Pins the exact composite-key contract so future code/PRs cannot regress.

---

## TL;DR (auto-driver one-liner)

> Sir, every row in our database needs a fingerprint. If two rows have the same fingerprint, the newer one wins (overwrites). For ticks the fingerprint is "which stock + which exchange + what time + which order". For candles it's "which stock + which exchange + what time + what bar size". This document lists every weird case that could break that fingerprint and proves we handle them.

---

## The two contracts (REVISED 2026-05-13 late-evening — 6 TFs, no 1s, no matviews)

| Table family | DEDUP UPSERT KEYS | Purpose |
|---|---|---|
| `ticks` (one table) | `(ts, security_id, exchange_segment, sequence_number)` | every WebSocket tick stored exactly once |
| **6 candle base tables**: `candles_1m`, `candles_3m`, `candles_5m`, `candles_15m`, `candles_1h`, `candles_1d` | `(ts, security_id, segment)` per table | every sealed bar stored exactly once; table name IS the timeframe |
| `historical_candles` (cold path) | `(ts, security_id, exchange_segment, timeframe)` | Dhan REST snapshots — single table holds multiple TFs, hence `timeframe` IS in the key |
| `previous_close` | `(trading_date_ist, security_id, exchange_segment)` | end-of-day close per SID |

**Removed from Phase 0:**
- `candles_1s` base table — DELETED. Aggregator computes 1m directly from ticks in RAM.
- All `candles_*` materialized views — DELETED. Each TF is its own native base table written by the in-RAM aggregator.

**Why no `timeframe` column on the 6 live candle tables:** each TF lives in its own table; the table name IS the timeframe. Including a redundant `timeframe` column would be dead weight. (Contrast with `historical_candles`, which is a single table holding all 6 TFs — there `timeframe` must be in the key.)

---

## Why each component is in the key

### Ticks — `(security_id, exchange_segment, ts, sequence_number)`

| Component | Why it's needed | What breaks without it |
|---|---|---|
| `security_id` | Identifies the instrument | Different instruments collapse onto one row |
| `exchange_segment` | I-P1-11: same SID can exist on IDX_I + NSE_EQ (FINNIFTY=27 lives on both) | Cross-segment data corruption (the 2026-04-17 prod bug) |
| `ts` | Identifies when the tick happened | Earlier ticks overwritten by later ones — silent history loss |
| `sequence_number` | Disambiguates **same-microsecond** ticks for the same SID | Burst-quote ticks collapse onto one row — silent data loss |

### Candles — `(ts, security_id, segment)` per table (6 tables, no timeframe column)

| Component | Why it's needed | What breaks without it |
|---|---|---|
| `ts` | Identifies the bar's start instant (1m: minute; 3m/5m/15m: bucket start; 1h: hour; 1d: midnight IST) | Adjacent bars collapse |
| `security_id` | Identifies the instrument | Cross-stock collision |
| `segment` | I-P1-11 rule: same SID can exist on IDX_I (e.g. FINNIFTY=27) AND NSE_EQ (=27) | Cross-segment collision (the 2026-04-17 prod bug) |

**No `timeframe` column** because each TF is its own base table. `candles_1m` holds only 1m bars, `candles_3m` only 3m, etc. The table name IS the timeframe. Adding a redundant column would waste storage and create a fake-uniqueness invariant.

### Historical candles (cold path) — `(ts, security_id, exchange_segment, timeframe)`

Different from live candles: `historical_candles` is a SINGLE table holding all 6 TFs from Dhan REST. Therefore `timeframe` MUST be in the key — otherwise the same SID's 1m bar and 5m bar at the same `ts` would collide.

---

## Item 30 specifically (`pct_from_prev_close`)

`pct_from_prev_close` is a **new attribute on the existing row**, NOT a new identity dimension. The candle DEDUP key stays `(security_id, exchange_segment, ts, timeframe)`. Live seal and cross-verify-overwrite (item 28) write to the same row; the second write replaces the percentage.

Ratchet test: `test_candle_dedup_key_unchanged_security_segment_ts_timeframe` fails the build if anyone adds `pct_from_prev_close` to a DEDUP KEYS clause.

---

## 28 Worst-Case / Extreme Scenarios — covered

### Tick path

| # | Scenario | Outcome under our key | Coverage |
|---|---|---|---|
| T1 | Two ticks at the **same microsecond** for the same SID (burst quotes during open) | `sequence_number` differs → 2 rows preserved | Dhan provides sequence in packet; parser preserves it |
| T2 | FINNIFTY id=27 IDX_I tick AND a different NSE_EQ id=27 tick at the same ts | `exchange_segment` differs → 2 rows preserved | I-P1-11 composite key |
| T3 | Replay after restart — rescue ring re-emits last 5M ticks | All keys identical to first attempt → DEDUP collapses to existing rows | Idempotent re-ingestion |
| T4 | Disconnect + reconnect — Dhan resends buffered ticks | Identical keys → DEDUP collapses | No duplicates |
| T5 | Clock skew — exchange_timestamp goes backward (rare) | `sequence_number` still monotonic per Dhan → unique row | Sequence is authoritative |
| T6 | `sequence_number = 0` from Dhan (legacy/missing) | All zero-seq ticks at same ts collide → ❌ silent loss | **Guard:** `parse_*` rejects zero-seq with `error!` — captured under `STORAGE-GAP-01` |
| T7 | u32 sequence wrap (theoretical 4.3B ticks in one day) | Wraps to 0 → potential collision with day's start | At 100K ticks/sec it takes 11.9h; market is 6.25h → **safe** but pinned by `test_sequence_no_wrap_within_trading_day` |
| T8 | IST midnight crossing during after-hours quotes | `ts` advances → different rows | No special handling needed |
| T9 | Mid-day SID re-mapping (Dhan renumbers an instrument) | Old SID rows preserved; new SID rows go to new key | I-P1-11 holds; no merge |
| T10 | Two WebSocket connections accidentally subscribe same SID | Pool distribution prevents this; if it ever happens, sequence numbers from Dhan are global per SID so DEDUP catches the dup |
| T11 | Tick arrives with `ts` in the future (Dhan bug) | Stored under future ts; cross-verify flags it | `data-integrity-guard` catches |
| T12 | Tick arrives with `ts < 1980` (uninitialized field) | Stored but cross-verify rejects | Data-integrity check |
| T13 | Same SID, same ts, same seq, **different price** (Dhan bug) | Second write overwrites first — undetectable single-row data loss | **Mitigation:** ILP write is append-only at the wire; DEDUP only collapses on flush. Logs both via `tracing` so post-hoc forensics possible |
| T14 | Spill replay after QuestDB outage — same payload re-written | All keys identical → DEDUP collapses | Bounded zero-loss envelope |

### Candle path

| # | Scenario | Outcome under our key | Coverage |
|---|---|---|---|
| C1 | Live aggregator seals 09:15 1m bar at 09:16:00 + cross-verify overwrite at 18:00 | Same key → second write replaces (correct values from Dhan REST) | Item 28 |
| C2 | Late tick arrives at 09:16:01 for the 09:15 minute | `AGGREGATOR-LATE-01` discards — no candle write at all | Wave 6 aggregator hardening |
| C3 | Boundary catch-up seal — OS preempted aggregator past minute boundary | `BOUNDARY-01` writes missed minutes with same key the live seal would have used | Idempotent |
| C4 | Same minute sealed twice (race between live + replay) | Same key → DEDUP collapses | Lock-free seal + DEDUP |
| C5 | First trading day of a new listing — no `prev_close` in cache | `pct_from_prev_close` written as NULL (or skipped per item 30 edge-case policy); row identity unchanged | Item 30 |
| C6 | Daily candle (`candles_1d`) — `ts = IST midnight` | One row per SID per day; same-day re-fetch overwrites | Self-referential prev_close from yesterday's `_1d.close` |
| C7 | Cross-day SID collision — security_id 27 IDX_I on Mon AND NSE_EQ on Tue | Different `ts` AND different segment → no collision | Defense in depth |
| C8 | Holiday / weekend (no market data) | No candles written → no rows → no collisions | Trading calendar gate |
| C9 | Daylight savings transition | India doesn't observe DST; `ts` always IST nanos | Safe by construction |
| C10 | Time zone bug — UTC ns written instead of IST ns | Wrong row identity, but DEDUP still collapses dupes within the wrong zone | `data-integrity-guard` rejects out-of-IST-range candles |
| C11 | Re-fetch of historical day after operator runs cross-verify replay | Same key → overwrites; `pct_from_prev_close` recomputes from same prev_close → idempotent | Item 28 + Item 30 |
| C12 | `ALTER ADD COLUMN` rerun on existing table | `IF NOT EXISTS` makes it idempotent | Schema self-heal |
| C13 | `pct_from_prev_close` written with `prev_close < 1e-9` | Skipped per item 30 policy → NULL; not zero | Edge case policy locked |
| C14 | Two aggregator instances running (dual-instance bug) | Both write same key → DEDUP collapses, but values may differ → undetectable | **`RESILIENCE-01` boot-time lock catches this** |

---

## What the contract does NOT promise

- **Same-key write with different values** is silently lost (the newer wins). Mitigation: every flush is also logged via `tracing` to `data/logs/app.*` so post-mortem forensics exist.
- **Pre-DEDUP burst** (ticks/candles in the same ILP batch with identical keys) — QuestDB applies DEDUP at commit, so within-batch dupes collapse correctly.
- **Schema drift** — if a future PR adds a column to the DEDUP key without `ALTER ADD COLUMN IF NOT EXISTS`, boot fails. **`dedup_segment_meta_guard.rs`** scans every DEDUP constant and fails the build on regression.

---

## Mechanical ratchets (build fails on regression)

| Test | What it pins |
|---|---|
| `dedup_segment_meta_guard.rs::every_dedup_key_includes_segment` | Every DEDUP key carries `exchange_segment` |
| `test_tick_dedup_key_is_sid_segment_ts_sequence` | Tick key composition |
| `test_candle_dedup_key_unchanged_security_segment_ts_timeframe` | Item 30's no-op guarantee on candle key |
| `test_previous_close_dedup_key_is_sid_segment_date` | Previous-close key |
| `test_sequence_no_wrap_within_trading_day` | u32 sequence safety |
| `test_alter_add_column_uses_if_not_exists` | Schema self-heal idempotency |
| `error_code_rule_file_crossref.rs` | Every error code referenced here has a runbook |

---

## Cross-references

- `.claude/rules/project/security-id-uniqueness.md` — I-P1-11 authoritative rule
- `.claude/rules/project/wave-6-error-codes.md` — `AGGREGATOR-LATE-01`, `BOUNDARY-01`
- `.claude/rules/project/wave-2-error-codes.md` — `STORAGE-GAP-01` through `STORAGE-GAP-04`
- `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` — Item 28 (cross-verify overwrite) + Item 30 (pct column)
- `.claude/plans/friday-may-15-mega/topic-extreme-worst-case-sweep.md` — system-wide sweep

---

## Auto-driver explanation

> Sir, imagine your shop has a register book. Each row is a sale. We want NO duplicates and NO lost sales.
>
> - **For ticks** (each price change): we write down "stock + exchange + exact time + serial number". If the same exact thing comes twice (network repeated it), we ignore the second copy — same handwriting, same row. If two customers buy at the same second, the serial number tells them apart.
>
> - **For candles** (15-minute summary): we write "stock + exchange + which minute + which size bar (1-min, 5-min, etc)". When Dhan corrects a number 8 hours later, we find the same row and pencil in the correction. The percentage column (item 30) is just a NEW pencil mark on the SAME row — we don't make a new row for it.
>
> Sir, the 28 weird situations above (network blip, restart, midnight rollover, Dhan correcting a number, two computers writing at once) — every one of them lands on the same row we already wrote, or on a properly different row. Never silent loss. Never silent duplicate. The fingerprint design guarantees it.
