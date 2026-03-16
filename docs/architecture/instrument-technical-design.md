# Instrument Technical Design — Complete Architecture Reference

> **Purpose:** Single source of truth for ALL instrument-related technical
> decisions, architecture, and enforcement mechanisms. Covers the entire pipeline
> from CSV download through QuestDB persistence, lifecycle tracking, and O(1)
> runtime lookups.
>
> **Last updated:** 2026-03-10

---

## 1. SYSTEM ARCHITECTURE

### 1.1 Data Flow

```
                      OUTSIDE MARKET HOURS
                      ====================

  Dhan CSV URL ──► Download ──► Parse ──► 5-Pass Build ──► Validate
       │                                       │               │
       ▼                                       ▼               ▼
  Local CSV Cache                         FnoUniverse    Reject if invalid
       │                                       │
       │              ┌────────────────────────┼─────────────────┐
       │              ▼                        ▼                 ▼
       │       rkyv Binary Cache      Delta Detection     QuestDB Persistence
       │       (fno-universe.rkyv)    (vs yesterday)      (4 snapshot tables +
       │              │                    │                1 lifecycle table)
       │              │                    ▼
       │              │             instrument_lifecycle
       │              │             (QuestDB events)
       │              ▼
       │       Freshness Marker
       │       (instrument-build-date.txt)
       │
       │       Persistence Marker
       │       (instrument-persist-date.txt)
       ▼

                      MARKET HOURS [08:45, 16:00) IST
                      ===============================

  rkyv Binary Cache ──► MappedUniverse (zero-copy mmap)
       │                      │
       │                      ▼
       │               SubscriptionPlan ──► InstrumentRegistry
       │                                          │
       │                                          ▼
       │                                   O(1) HashMap lookups
       │                                   (tick enrichment)
       │
       ▼ (fallback if rkyv missing/corrupt)
  CSV File Cache ──► Parse ──► Build ──► FnoUniverse ──► SubscriptionPlan
```

### 1.2 Boot Sequence Position

```
CryptoProvider → Config → Observability → Logging → Notification → Auth
→ QuestDB Table Setup (ensure_instrument_tables)    ← DDL + DEDUP keys
→ Universe Build (load_or_build_instruments)         ← this module
→ Subscription Planning (build_subscription_plan)
→ WebSocket → TickProcessor → OMS → API → TokenRenewal → Shutdown
```

### 1.3 Crate Ownership

| Crate | What it owns |
|-------|-------------|
| `common` | Types (`FnoUniverse`, `DerivativeContract`, etc.), `InstrumentRegistry`, constants |
| `core` | CSV download, parsing, 5-pass builder, validation, caching, delta detection, loader |
| `storage` | QuestDB persistence (ILP writer, table DDL, lifecycle writer) |
| `app` | Orchestration in `main.rs`, startup sequencing |
| `api` | HTTP endpoints (`POST /api/instruments/rebuild`) |

### 1.4 Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/instrument/universe_builder.rs` | 5-pass algorithm |
| `crates/core/src/instrument/csv_parser.rs` | CSV → ParsedInstrumentRow |
| `crates/core/src/instrument/csv_downloader.rs` | Download + cache CSV |
| `crates/core/src/instrument/instrument_loader.rs` | Market-hours-aware orchestrator |
| `crates/core/src/instrument/binary_cache.rs` | rkyv zero-copy cache |
| `crates/core/src/instrument/validation.rs` | Post-build validation (11 checks) |
| `crates/core/src/instrument/subscription_planner.rs` | Universe → SubscriptionPlan |
| `crates/core/src/instrument/delta_detector.rs` | Day-over-day change detection |
| `crates/common/src/instrument_types.rs` | All domain types |
| `crates/common/src/instrument_registry.rs` | O(1) tick enrichment registry |
| `crates/storage/src/instrument_persistence.rs` | QuestDB ILP writer (5 tables) |
| `crates/common/src/constants.rs` | All instrument constants |

---

## 2. THE 5-PASS ALGORITHM

### 2.1 Overview

The universe builder makes 5 passes over the parsed CSV rows. Each pass
builds on results from previous passes.

### 2.2 Pass 1: Index Lookup

**Input:** All rows where `segment = 'I'` (both NSE and BSE).
**Output:** `HashMap<String, IndexEntry>` — symbol → (security_id, exchange).
**Count:** ~194 entries (119 NSE + 75 BSE).

Purpose: Build a lookup so Pass 4 can find the IDX_I security_id for any
index by its symbol name.

### 2.3 Pass 2: Equity Lookup

**Input:** Rows where `exchange = NSE`, `segment = 'E'`, `series = "EQ"`.
**Output:** `HashMap<String, SecurityId>` — symbol → security_id.
**Count:** ~2,442 entries.

Purpose: Build a lookup so Pass 4 can find the NSE_EQ security_id for any
stock by its symbol name.

### 2.4 Pass 3: Find F&O Underlyings

**Input:** All FUTIDX and FUTSTK rows from `segment = 'D'`.
**Processing:**
- Deduplicate by `UNDERLYING_SYMBOL` (first occurrence wins)
- Skip anything containing "TEST"
- Classify: FUTIDX+NSE = NseIndex, FUTIDX+BSE = BseIndex, FUTSTK = Stock
- Record: underlying_security_id, derivative segment, lot_size

**Output:** ~214 `UnlinkedUnderlying` objects.

### 2.5 Pass 4: Link to Live Price IDs

**Input:** Pass 3 underlyings + Pass 1/2 lookups.
**Processing:**
- Indices: Look up IDX_I security_id in Pass 1's index map
  - Apply aliases: NIFTYNXT50 → "NIFTY NEXT 50", SENSEX50 → "SNSX50"
- Stocks: Look up NSE_EQ security_id in Pass 2's equity map
  - Fallback: Use UNDERLYING_SECURITY_ID directly

**Output:** ~214 `FnoUnderlying` objects with price_feed_security_id linked.

### 2.6 Pass 5: Scan All Derivatives

**Input:** ALL `segment = 'D'` rows.
**Processing:**
- Filter: known underlying + valid expiry (>= today) + not TEST
- Create `DerivativeContract` keyed by SECURITY_ID
- Build `OptionChain` grouped by (underlying, expiry), sorted by strike
- Build `ExpiryCalendar` per underlying (sorted dates)
- Build `InstrumentInfo` global lookup (indices + equities + derivatives)
- Build `SubscribedIndex` list (8 F&O + 23 display)

**Output:** ~97K contracts (varies 80K–150K+), option chains, calendars.

---

## 3. DEDUPLICATION & UNIQUENESS ENFORCEMENT (7 Layers)

### 3.1 Layer 1: Application Persistence Marker

**File:** `instrument-persist-date.txt` (separate from build freshness marker)
**Location:** Same cache directory as other instrument files.

**Semantics:**
- Contains a single line: `YYYY-MM-DD` (IST date of last successful QuestDB write)
- Checked BEFORE any QuestDB write attempt
- Written ONLY after ALL 4 snapshot tables + lifecycle table successfully flushed
- If QuestDB write fails → marker NOT written → next restart retries

**Why separate from build freshness marker:**
- Build freshness marker = "I downloaded and built today" (prevents re-download)
- Persistence marker = "I wrote to QuestDB today" (prevents duplicate DB writes)
- Build can succeed but persistence can fail (QuestDB down) — must retry persistence
  without re-downloading

### 3.2 Layer 2: DEDUP Verification Gate

**Before writing to QuestDB:**
1. Run `ALTER TABLE ... DEDUP ENABLE UPSERT KEYS(...)` on all 5 tables
2. Verify DEDUP is active by querying `tables()` or running a verification query
3. If verification FAILS → log ERROR → skip QuestDB write entirely
4. Write proceeds ONLY with confirmed DEDUP

**Why this matters:** The previous implementation was "best-effort" — DEDUP
ALTER could fail silently, and writes would proceed without dedup, creating
duplicate rows. This was the root cause of the bug.

### 3.3 Layer 3: QuestDB DEDUP UPSERT KEYS (Database Level)

All 5 instrument tables have DEDUP enabled:

```sql
ALTER TABLE instrument_build_metadata DEDUP ENABLE UPSERT KEYS(timestamp, csv_source)
ALTER TABLE fno_underlyings DEDUP ENABLE UPSERT KEYS(timestamp, underlying_symbol)
ALTER TABLE derivative_contracts DEDUP ENABLE UPSERT KEYS(timestamp, security_id)
ALTER TABLE subscribed_indices DEDUP ENABLE UPSERT KEYS(timestamp, security_id)
ALTER TABLE instrument_lifecycle DEDUP ENABLE UPSERT KEYS(snapshot_date, security_id, event_type)
```

**Semantics:** Same (timestamp, key) → last write wins. No duplicate rows.

### 3.4 Layer 4: Day-Over-Day Delta Detection

**Module:** `crates/core/src/instrument/delta_detector.rs`

**Algorithm:**
```
Input:  yesterday: Option<FnoUniverse>, today: FnoUniverse
Output: Vec<LifecycleEvent>

1. If yesterday is None → return empty (first-ever run, all rows are "initial")
2. For each security_id in yesterday.derivative_contracts:
   - If NOT in today → emit ContractExpired event
   - If in today BUT lot_size differs → emit LotSizeChanged event
3. For each security_id in today.derivative_contracts:
   - If NOT in yesterday → emit ContractAdded event
4. For each symbol in yesterday.underlyings:
   - If NOT in today → emit UnderlyingRemoved event
5. For each symbol in today.underlyings:
   - If NOT in yesterday → emit UnderlyingAdded event
```

**Performance:** O(n) where n = max(yesterday_count, today_count). Runs ONLY
at build time, outside market hours. Never on hot path.

### 3.5 Layer 5: instrument_lifecycle QuestDB Table

```sql
CREATE TABLE IF NOT EXISTS instrument_lifecycle (
    security_id LONG,
    underlying_symbol SYMBOL,
    event_type SYMBOL,
    field_changed SYMBOL,
    old_value STRING,
    new_value STRING,
    snapshot_date TIMESTAMP
) TIMESTAMP(snapshot_date) PARTITION BY MONTH WAL
```

**Event types:** `contract_added`, `contract_expired`, `lot_size_changed`,
`tick_size_changed`, `underlying_added`, `underlying_removed`

**Volume:** Typically 100–500 events per day (weekly expiry cycles).
Spikes to 1000+ around monthly expiry.

**Queries:**
```sql
-- What contracts expired on 2026-03-06?
SELECT * FROM instrument_lifecycle
WHERE event_type = 'contract_expired'
AND snapshot_date = '2026-03-06';

-- When did RELIANCE lot size change?
SELECT * FROM instrument_lifecycle
WHERE underlying_symbol = 'RELIANCE'
AND event_type = 'lot_size_changed';

-- How many contracts added/expired each day?
SELECT snapshot_date, event_type, count()
FROM instrument_lifecycle
GROUP BY snapshot_date, event_type
ORDER BY snapshot_date;
```

### 3.6 Layer 6: Build-Time Validation Enhancements

**Check 10: Duplicate security_id detection**
During Pass 5, before inserting a derivative contract, check if security_id
already exists. If duplicate found → log which two rows conflict → hard error.

**Check 11: Count consistency**
After building derivative_contracts HashMap, verify `.len()` equals the
number of unique security_ids found during parsing. If mismatch → hard error
(indicates silent HashMap overwrite from duplicate IDs).

### 3.7 Layer 7: Pre-Commit Hook Enforcement

**dedup-latency-scanner.sh** additions:
- Scan for `persist_instrument_snapshot` calls that don't check persistence marker
- Scan for raw ILP `Sender::flush()` calls in instrument code without DEDUP verification

---

## 4. QuestDB TABLE SCHEMAS — ALL 5 TABLES

### 4.1 instrument_build_metadata (1 row/day)

| Column | QuestDB Type | Source |
|--------|-------------|--------|
| csv_source | SYMBOL | "primary" or "fallback" or "cache" |
| csv_row_count | LONG | Raw CSV row count |
| parsed_row_count | LONG | After segment filtering |
| index_count | LONG | Pass 1 result |
| equity_count | LONG | Pass 2 result |
| underlying_count | LONG | Pass 3 result |
| derivative_count | LONG | Pass 5 result |
| option_chain_count | LONG | Pass 5 result |
| build_duration_ms | LONG | Total build time in ms |
| build_timestamp | TIMESTAMP | Exact IST time build completed |
| timestamp | TIMESTAMP | **Designated** — midnight IST |

DEDUP KEY: `(timestamp, csv_source)`

### 4.2 fno_underlyings (~214 rows/day)

| Column | QuestDB Type | Source |
|--------|-------------|--------|
| underlying_symbol | SYMBOL | e.g., "NIFTY", "RELIANCE" |
| price_feed_segment | SYMBOL | "IDX_I" or "NSE_EQ" |
| derivative_segment | SYMBOL | "NSE_FNO" or "BSE_FNO" |
| kind | SYMBOL | "NseIndex", "BseIndex", "Stock" |
| underlying_security_id | LONG | FNO phantom ID (e.g., 26000) |
| price_feed_security_id | LONG | Live price ID (e.g., 13) |
| lot_size | LONG | Contract lot size |
| contract_count | LONG | Total derivative count |
| timestamp | TIMESTAMP | **Designated** — midnight IST |

DEDUP KEY: `(timestamp, underlying_symbol)`

### 4.3 derivative_contracts (~80K–150K rows/day)

| Column | QuestDB Type | Source |
|--------|-------------|--------|
| underlying_symbol | SYMBOL | e.g., "NIFTY" |
| instrument_kind | SYMBOL | "FutureIndex", "OptionStock", etc. |
| exchange_segment | SYMBOL | "NSE_FNO" or "BSE_FNO" |
| option_type | SYMBOL | "CE", "PE", or "" (futures) |
| symbol_name | SYMBOL | Full CSV symbol |
| security_id | LONG | This contract's unique ID |
| expiry_date | STRING | "YYYY-MM-DD" (NOT TIMESTAMP) |
| strike_price | DOUBLE | Strike price (0.0 for futures) |
| lot_size | LONG | Contract lot size |
| tick_size | DOUBLE | Minimum price movement |
| display_name | STRING | Human-readable name |
| timestamp | TIMESTAMP | **Designated** — midnight IST |

DEDUP KEY: `(timestamp, security_id)`

**Why expiry_date is STRING, not TIMESTAMP:**
Prevents timezone-related date shift (e.g., 2026-03-27 becoming 2026-03-26T18:30:00Z).
`NaiveDate::to_string()` produces clean "YYYY-MM-DD".

### 4.4 subscribed_indices (31 rows/day)

| Column | QuestDB Type | Source |
|--------|-------------|--------|
| symbol | SYMBOL | e.g., "NIFTY", "INDIA VIX" |
| exchange | SYMBOL | "NSE" or "BSE" |
| category | SYMBOL | "FnoUnderlying" or "DisplayIndex" |
| subcategory | SYMBOL | "Fno", "Volatility", "Sectoral", etc. |
| security_id | LONG | IDX_I security ID |
| timestamp | TIMESTAMP | **Designated** — midnight IST |

DEDUP KEY: `(timestamp, security_id)`

### 4.5 instrument_lifecycle (100–1000+ events/day)

| Column | QuestDB Type | Source |
|--------|-------------|--------|
| security_id | LONG | Contract security ID |
| underlying_symbol | SYMBOL | e.g., "NIFTY" |
| event_type | SYMBOL | "contract_added", "contract_expired", etc. |
| field_changed | SYMBOL | "lot_size", "tick_size", or "" |
| old_value | STRING | Previous value (empty for adds) |
| new_value | STRING | New value (empty for expires) |
| snapshot_date | TIMESTAMP | **Designated** — midnight IST |

DEDUP KEY: `(snapshot_date, security_id, event_type)`

---

## 5. CACHING STRATEGY

### 5.1 Three Cache Layers

| Layer | File | Format | Load Time | When Written |
|-------|------|--------|-----------|-------------|
| rkyv binary | `fno-universe.rkyv` | Zero-copy mmap | sub-0.5ms | After successful build |
| CSV file | `api-scrip-master-detailed.csv` | Raw CSV | ~400ms | On download |
| Freshness marker | `instrument-build-date.txt` | "YYYY-MM-DD" | instant | After successful build |
| Persistence marker | `instrument-persist-date.txt` | "YYYY-MM-DD" | instant | After successful QuestDB write |

### 5.2 rkyv Zero-Copy Cache

The `MappedUniverse` uses memory-mapped I/O to access the archived universe
directly from disk without deserialization. The archived types mirror the
owned types but are repr(C) aligned for zero-copy access.

**Key property:** During market hours, the entire universe is available
via a single mmap syscall. No parsing, no allocation, no computation.

### 5.3 Freshness Marker vs Persistence Marker

| Property | Build Freshness | QuestDB Persistence |
|----------|----------------|-------------------|
| File | `instrument-build-date.txt` | `instrument-persist-date.txt` |
| Written after | Successful CSV download + build | Successful QuestDB write |
| Prevents | Re-downloading same day (manual API) | Re-writing to QuestDB same day |
| Checked by | `try_rebuild_instruments()` | `persist_instrument_snapshot()` |
| Outside market hours | Ignored (always downloads fresh) | Checked (prevents double write) |

---

## 6. IST TIMESTAMP CONVENTION

All instrument tables use "IST-as-UTC" convention for the designated timestamp:

- Today's IST date: 2026-03-10
- Stored as: `2026-03-10T00:00:00.000000Z` (midnight IST stored as UTC)
- QuestDB displays: `2026-03-10T00:00:00.000000Z`
- Grafana must set timezone to `Asia/Kolkata` for correct display

**Why not true UTC?** Because IST midnight is 18:30 UTC previous day, which
would cause date confusion in queries. By storing IST-as-UTC, a simple
`WHERE timestamp = '2026-03-10'` matches the IST date without timezone math.

---

## 7. PERFORMANCE CHARACTERISTICS

### 7.1 Build Time (Cold Path — Outside Market Hours)

| Step | Time | Notes |
|------|------|-------|
| CSV download | 1–5s | Depends on network |
| CSV parsing | ~200ms | ~276K rows parsed |
| 5-pass build | ~2s | HashMap operations |
| Validation | <10ms | 11 checks |
| Delta detection | <50ms | O(n) comparison |
| QuestDB persistence | ~2s | ~150K ILP rows batched |
| rkyv cache write | ~100ms | Serialize + fsync |
| **Total** | **3–10s** | MacBook Pro |
| **Total (prod)** | **1–3s** | AWS c7i.2xlarge |

### 7.2 Load Time (Hot Path — Market Hours)

| Method | Time | Notes |
|--------|------|-------|
| rkyv zero-copy | sub-0.5ms | mmap only, no deserialize |
| CSV fallback | ~400ms | Parse + build (no download) |
| Subscription plan | <1ms | Filter + build registry |

### 7.3 Runtime Lookup (Hot Path — Per Tick)

| Lookup | Time | Method |
|--------|------|--------|
| security_id → instrument | O(1) | HashMap::get |
| symbol → underlying | O(1) | HashMap::get |
| (symbol, expiry) → chain | O(1) | HashMap::get |

**ZERO allocation on hot path.** InstrumentRegistry is `Arc<>`, immutable,
no locks, no contention. Pure HashMap reads.

---

## 8. TYPE SYSTEM GUARANTEES

### 8.1 SecurityId

```rust
pub type SecurityId = u32;
```

Dhan security IDs are always positive 32-bit integers. Stored as `LONG` (i64)
in QuestDB via `i64::from(security_id)` — infallible widening, zero risk.

### 8.2 ExchangeSegment Enum

```rust
pub enum ExchangeSegment {
    IdxI,      // Index segment (both NSE and BSE)
    NseEquity, // NSE equities
    NseFno,    // NSE F&O derivatives
    BseFno,    // BSE F&O derivatives
}
```

Exhaustive match — compiler enforces handling of all variants.

### 8.3 DhanInstrumentKind Enum

```rust
pub enum DhanInstrumentKind {
    FutureIndex,  // FUTIDX
    FutureStock,  // FUTSTK
    OptionIndex,  // OPTIDX
    OptionStock,  // OPTSTK
}
```

Maps 1:1 to Dhan's CSV `INSTRUMENT` column values.

### 8.4 OptionType Enum

```rust
pub enum OptionType { Call, Put }
```

Futures have `option_type: None`. Options have `Some(Call)` or `Some(Put)`.
In QuestDB: `""` for futures, `"CE"` or `"PE"` for options (ILP SYMBOL
columns don't support NULL).

---

## 9. ERROR HANDLING STRATEGY

### 9.1 What Blocks Trading

| Error | Impact | Recovery |
|-------|--------|---------|
| CSV download fails (all retries + cache) | **System halts** | Manual intervention + Telegram alert |
| Universe validation fails | **System halts** | CSV is corrupted — wait for Dhan to fix |
| QuestDB persistence fails | **Non-blocking** | Logged as WARN, trading continues |
| Lifecycle detection fails | **Non-blocking** | Logged as WARN, trading continues |
| rkyv cache write fails | **Non-blocking** | Logged as WARN, CSV fallback next time |

### 9.2 Principle

**Instruments are REQUIRED for trading. QuestDB persistence is observability
data, NOT critical path.** If persistence fails, the universe is still in
memory and the system trades normally. The persistence failure is logged and
will be retried on next restart.

---

## 10. TEST STRATEGY

### 10.1 Unit Tests per Module

| Module | Test Count | Key Scenarios |
|--------|-----------|---------------|
| universe_builder | ~40 | All 5 passes, edge cases, aliases |
| csv_parser | ~20 | Column detection, type parsing, filtering |
| validation | ~25 | All 11 checks, pass and fail paths |
| instrument_loader | ~15 | Market hours, cache, freshness marker |
| binary_cache | ~10 | Write/read/corrupt/missing |
| instrument_persistence | ~20 | ILP serialization, DDL, DEDUP |
| delta_detector | ~12 | Added/expired/modified/no-change/first-run |
| instrument_registry | ~14 | All categories, O(1) lookup |
| subscription_planner | ~17 | Config toggles, filtering, dedup |

### 10.2 Integration Test Scenarios

1. Full pipeline: download CSV → build → validate → persist → delta detect
2. Market hours restart: rkyv cache → subscription plan → ready
3. Double write prevention: two persist calls → only one write
4. DEDUP verification failure → write skipped
5. First-ever run → no delta events
6. Day-over-day with expiry → lifecycle events emitted

### 10.3 Invariant Tests (Compile-Time + Runtime)

- HashMap key uniqueness (structural — HashMap enforces)
- SecurityId type is u32 (compiler enforces)
- Exhaustive enum matching (compiler enforces)
- Validation checks (runtime — hard error on fail)
- Delta detection correctness (unit tests — every change type)

---

## 11. CONFIGURATION

### 11.1 Instrument Config (`config/base.toml`)

```toml
[instrument]
daily_download_time = "08:45:00"
csv_cache_directory = "/data/instruments"
csv_cache_filename = "api-scrip-master-detailed.csv"
csv_download_timeout_secs = 120
build_window_start = "08:45:00"   # Market hours start (no download)
build_window_end = "16:00:00"     # Market hours end
```

### 11.2 Subscription Config (`config/base.toml`)

```toml
[subscription]
feed_mode = "Ticker"
subscribe_index_derivatives = true
subscribe_stock_derivatives = true
subscribe_display_indices = true
subscribe_stock_equities = true
stock_atm_strikes_above = 10
stock_atm_strikes_below = 10
stock_default_atm_fallback_enabled = true
```

### 11.3 QuestDB Config (`config/base.toml`)

```toml
[questdb]
host = "questdb"        # Docker hostname
ilp_port = 9009         # ILP TCP port
http_port = 9000        # HTTP REST API port
pg_port = 8812          # PostgreSQL wire protocol port
```

---

## 12. COMMON QUESTDB QUERIES

```sql
-- What was security_id 12345 on 2026-03-15?
SELECT * FROM derivative_contracts
WHERE timestamp = '2026-03-15' AND security_id = 12345;

-- How many contracts did NIFTY have over the last 30 days?
SELECT timestamp, contract_count FROM fno_underlyings
WHERE underlying_symbol = 'NIFTY'
AND timestamp > dateadd('d', -30, now());

-- Build health over the last week
SELECT * FROM instrument_build_metadata
WHERE timestamp > dateadd('d', -7, now())
ORDER BY timestamp;

-- When did RELIANCE lot size change?
SELECT * FROM instrument_lifecycle
WHERE underlying_symbol = 'RELIANCE'
AND event_type = 'lot_size_changed'
ORDER BY snapshot_date;

-- Contracts that expired yesterday
SELECT security_id, underlying_symbol, old_value
FROM instrument_lifecycle
WHERE event_type = 'contract_expired'
AND snapshot_date = '2026-03-09';

-- New contracts added today
SELECT security_id, underlying_symbol, new_value
FROM instrument_lifecycle
WHERE event_type = 'contract_added'
AND snapshot_date = '2026-03-10';

-- Daily churn (adds vs expires)
SELECT snapshot_date,
       sum(CASE WHEN event_type = 'contract_added' THEN 1 ELSE 0 END) as added,
       sum(CASE WHEN event_type = 'contract_expired' THEN 1 ELSE 0 END) as expired
FROM instrument_lifecycle
GROUP BY snapshot_date
ORDER BY snapshot_date;
```
