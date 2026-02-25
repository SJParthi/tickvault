# BLOCK 01 — MASTER INSTRUMENT DOWNLOAD & F&O UNIVERSE BUILDER

## Status: ✅ COMPLETE & RUNNING
## Last Verified: 2026-02-24 (live run against real Dhan CSV)

---

## OBJECTIVE

Download Dhan's instrument master CSV daily, parse it, run a 6-pass mapping algorithm, build the complete F&O universe with all lookup maps, generate WebSocket subscription lists, and validate everything. This is the foundation — every downstream block consumes Block 01's output.

---

## LIVE OUTPUT (2026-02-24)

```
CSV downloaded:      276,018 rows (detailed CSV, ~40 MB)
Parsed:              160,245 relevant rows (NSE I/E/D + BSE I/D)
Pipeline time:       ~3 seconds total

Pass 1: Index lookup    → 119 NSE + 75 BSE = 194 indices
Pass 2: Equity lookup   → 2,442 NSE_EQ stocks
Pass 3: F&O underlyings → 215 unique (5 NSE idx + 3 BSE idx + 207 stocks)
Pass 4: Price ID linking → All 215 linked successfully
Pass 5: Derivatives     → 150,949 contracts (1,254 FUT + 149,695 OPT)
Pass 6: Subscriptions   → 14,577 / 25,000 WebSocket slots

Validation: ✅ ALL PASSED
  - NIFTY     → IDX_I:13,  FNO id: 26000, 4,031 contracts
  - RELIANCE  → NSE_EQ:2885, lot: 500, 828 contracts
  - SENSEX    → IDX_I:51,  FNO id: 1, 3,943 contracts
  - Option chains: 1,288 unique (underlying × expiry)
  - Expiry calendar: 215 underlyings, NIFTY has 18 active expiry dates
```

---

## DATA SOURCE

**Primary (no auth required):**
`GET https://images.dhan.co/api-data/api-scrip-master-detailed.csv`
~40 MB, ~276,000 rows across all exchanges/segments. Updated daily by Dhan.

**Fallback:**
`GET https://images.dhan.co/api-data/api-scrip-master.csv`
Smaller file, fewer columns, same security IDs.

**Download strategy:** Try detailed CSV first (3 retries, exponential backoff 2s/4s/8s, 120s timeout). If fails, try compact CSV. If both fail, use previous day's cached file with WARN alert. If no cache exists, system cannot start. Sanity check: file must have > 100,000 rows.

Must download fresh every day at 08:45 IST.

---

## CSV COLUMNS USED

EXCH_ID, SEGMENT, SECURITY_ID, INSTRUMENT, UNDERLYING_SECURITY_ID, UNDERLYING_SYMBOL, SYMBOL_NAME, DISPLAY_NAME, SERIES, LOT_SIZE, SM_EXPIRY_DATE, STRIKE_PRICE, OPTION_TYPE, TICK_SIZE, EXPIRY_FLAG.

Parser auto-detects column indices from headers (supports both detailed and compact CSV formats).

**Filtering:** Only rows where (NSE, I), (NSE, E), (NSE, D), (BSE, I), or (BSE, D). Everything else (MCX, currency, etc.) is skipped.

---

## 6-PASS MAPPING ALGORITHM

### Pass 1: Build Index Lookup
Scan all rows where SEGMENT = "I" (both NSE and BSE). Build a map of symbol → (security_id, IDX_I). Note: both NSE and BSE indices map to the IDX_I exchange segment, not separate segments. Result: 194 entries (119 NSE + 75 BSE).

### Pass 2: Build Equity Lookup
Scan rows where EXCH_ID = "NSE", SEGMENT = "E", SERIES = "EQ". Build a map of symbol → security_id. Result: 2,442 entries.

### Pass 3: Find F&O Underlyings
Scan all FUTIDX and FUTSTK rows from SEGMENT = "D". Deduplicate by UNDERLYING_SYMBOL. Skip anything containing "TEST". Classify each: FUTIDX+NSE = NSE Index, FUTIDX+BSE = BSE Index, FUTSTK = Stock. Record the UNDERLYING_SECURITY_ID, derivative segment (NSE_FNO or BSE_FNO), and lot size. Result: 215 unique underlyings (5 NSE indices + 3 BSE indices + 207 stocks).

### Pass 4: Link Underlyings to Live Price IDs
For each underlying found in Pass 3, find the security ID needed to subscribe for its live price feed:

**Indices** → Look up in Pass 1's index map. If the FNO symbol differs from the index row symbol, apply aliases. Only 2 aliases exist: NIFTYNXT50 → "NIFTY NEXT 50", SENSEX50 → "SNSX50". The resulting ID is an IDX_I security ID (e.g., NIFTY=13, BANKNIFTY=25, SENSEX=51).

**Stocks** → Look up in Pass 2's equity map. Direct match by symbol name. Fallback: use UNDERLYING_SECURITY_ID directly (guaranteed equal to the NSE_EQ SECURITY_ID for stocks). The resulting ID is an NSE_EQ security ID (e.g., RELIANCE=2885).

Result: All 215 linked successfully.

### Pass 5: Scan All Derivatives
Scan ALL SEGMENT = "D" rows (~150K+). For each row with a known underlying, valid expiry (≥ today), and not a TEST instrument, create a derivative contract record keyed by its own SECURITY_ID. Also build: option chains grouped by (underlying, expiry) sorted by strike price, expiry calendars per underlying, and a global security_id → instrument info map covering indices, equities, AND derivatives (used later for WebSocket binary response decoding).

Result: 150,949 contracts, 1,288 option chains, 215 expiry calendars.

### Pass 6: Build WebSocket Subscription Plan
Decide what to subscribe on the fixed WebSocket connections. Each entry specifies a security_id, exchange_segment, and feed mode.

**What SecurityId gets sent for each category:**

| Category | SecurityId Sent | ExchangeSegment | Feed Mode | Count |
|----------|----------------|-----------------|-----------|-------|
| F&O Index prices (8) | Underlying's IDX_I price ID (e.g., NIFTY=13) | IDX_I | Ticker | 8 |
| Display indices (23) | Hardcoded IDX_I IDs (e.g., VIX=21) | IDX_I | Ticker | 23 |
| F&O Stock prices (207) | Underlying's NSE_EQ price ID (e.g., RELIANCE=2885) | NSE_EQ | Quote | 207 |
| Index full chains (5 indices) | Each contract's OWN security ID | NSE_FNO / BSE_FNO | Full | 13,109 |
| Stock futures (207 stocks) | Each contract's OWN security ID | NSE_FNO | Quote | 1,230 |
| **TOTAL** | | | | **14,577** |

Capacity: 14,577 / 25,000 = 58.3% used. Remaining 10,423 slots reserved for active trade positions (dynamically subscribed, locked until exit).

**Full chain indices:** NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, SENSEX. Other indices (BANKEX, SENSEX50, NIFTYNXT50) have price feeds but derivative chains are subscribed dynamically when needed.

**Stock options:** Not in fixed pool (~140K+ contracts). Subscribed dynamically per-trade.

Dedup logic: If a display index overlaps with an F&O index, it's skipped (no double-subscribe).

Validation: Total must be ≤ 25,000 or system bails.

---

## DISPLAY INDICES (23, all IDX_I, Ticker mode)

Dhan-marked indices subscribed for market dashboard and sentiment:

| Index Name | IDX_I Security ID | Category |
|-----------|-------------------|----------|
| INDIA VIX | 21 | Volatility |
| NIFTY 100 | 17 | Broad Market |
| NIFTY 200 | 18 | Broad Market |
| NIFTY 500 | 19 | Broad Market |
| NIFTYMCAP50 | 20 | Mid Cap |
| NIFTY MIDCAP 150 | 1 | Mid Cap |
| NIFTY SMALLCAP 50 | 22 | Small Cap |
| NIFTY SMALLCAP 100 | 5 | Small Cap |
| NIFTY SMALLCAP 250 | 3 | Small Cap |
| NIFTY AUTO | 14 | Sectoral |
| NIFTY PVT BANK | 15 | Sectoral |
| NIFTY FMCG | 28 | Sectoral |
| NIFTY ENERGY | 42 | Sectoral |
| NIFTYINFRA | 43 | Sectoral |
| NIFTYIT | 29 | Sectoral |
| NIFTY MEDIA | 30 | Sectoral |
| NIFTY METAL | 31 | Sectoral |
| NIFTY MNC | 44 | Sectoral |
| NIFTY PHARMA | 32 | Sectoral |
| NIFTY PSU BANK | 33 | Sectoral |
| NIFTY REALTY | 34 | Sectoral |
| NIFTY SERV SECTOR | 46 | Sectoral |
| NIFTY CONSUMPTION | 40 | Thematic |

Combined with 8 F&O indices = 31 total index subscriptions on IDX_I.

F&O Indices: NIFTY(13), BANKNIFTY(25), FINNIFTY(27), MIDCPNIFTY(442), NIFTYNXT50(38), SENSEX(51), BANKEX(69), SENSEX50(83).

---

## KEY MAPPING DISCOVERY

**Stock Security IDs:** UNDERLYING_SECURITY_ID in FNO derivative rows = SECURITY_ID in NSE_EQ equity rows. Direct numeric match. No mapping table needed.

**Index Security IDs:** Indices use "phantom" IDs — the UNDERLYING_SECURITY_ID in FNO rows (e.g., NIFTY=26000) does NOT match the IDX_I SECURITY_ID (NIFTY=13). Must match by symbol name through the index lookup from Pass 1.

**Index aliases:** Only 2 cases where the FNO UNDERLYING_SYMBOL differs from the IDX_I symbol: NIFTYNXT50 → "NIFTY NEXT 50", SENSEX50 → "SNSX50". All other indices match directly.

**BSE indices use IDX_I:** Both NSE and BSE index rows map to the same IDX_I exchange segment. There is no separate "BSE_I" segment.

---

## VALIDATION

Must-exist checks with exact price IDs:
NIFTY → IDX_I:13, BANKNIFTY → IDX_I:25, FINNIFTY → IDX_I:27, MIDCPNIFTY → IDX_I:442, NIFTYNXT50 → IDX_I:38, SENSEX → IDX_I:51, BANKEX → IDX_I:69, SENSEX50 → IDX_I:83, RELIANCE → NSE_EQ:2885.

Count checks: F&O stocks in 150–300 range (currently 207). Total subscriptions ≤ 25,000 (currently 14,577). Each F&O index has contract_count > 0. Known stocks present: RELIANCE, HDFCBANK, INFY, TCS.

Any validation failure = system refuses to start.

---

## CORPORATE ACTION RESILIENCE

If security IDs change (demerger, symbol change, etc.): daily CSV rebuild at 08:45 IST discovers new mappings automatically. Old security IDs vanish from CSV, new ones appear. System self-heals completely — zero manual intervention needed.

---

## OUTPUT

Block 01 produces a single FnoUniverse object containing:
- 215 underlyings with their price IDs, derivative segments, and lot sizes
- 150,949 derivative contracts keyed by their individual security IDs
- A global security_id → instrument info map (covers indices, equities, AND derivatives)
- 1,288 option chains grouped by (underlying, expiry), sorted by strike
- 215 expiry calendars (sorted expiry dates per underlying)
- A subscription plan of 14,577 entries ready to send to WebSocket

---

## BLOCK 01.1 — INSTRUMENT PERSISTENCE TO QUESTDB

### Why Persist

The FnoUniverse is rebuilt in-memory daily from the CSV. But after 90+ days, when reviewing historical tick data, charts, or trade audit trails, we need to know exactly what each security_id referred to on any given day. Security IDs can change due to corporate actions (demergers, symbol changes, new listings). Without persisted instrument snapshots, historical data becomes undecodable.

**Use cases requiring persisted instruments:**
- Decode security_id from historical tick data stored in QuestDB
- Verify what lot sizes, strike prices, and expiry dates existed on a past date
- Track instrument universe changes over time (new listings, delistings, lot size changes)
- Audit trail: confirm what the system knew about instruments when a trade was placed
- Dashboard: show daily build health metrics over time

### QuestDB Tables (4)

#### Table 1: `instrument_build_metadata`

One row per daily build. Tracks build health and universe statistics.

```sql
CREATE TABLE instrument_build_metadata (
    build_date         TIMESTAMP,    -- IST date of the build (designated timestamp)
    csv_source         SYMBOL,       -- "primary" or "fallback" or "cache"
    csv_row_count      INT,          -- total rows in raw CSV
    parsed_row_count   INT,          -- rows after segment filtering
    index_count        INT,          -- Pass 1 result
    equity_count       INT,          -- Pass 2 result
    underlying_count   INT,          -- Pass 3 result
    derivative_count   INT,          -- Pass 5 result
    option_chain_count INT,          -- Pass 5 result
    build_duration_ms  INT,          -- total build time in milliseconds
    build_timestamp    TIMESTAMP     -- exact IST time build completed
) TIMESTAMP(build_date) PARTITION BY MONTH;
```

**Volume:** 1 row/day = 365 rows/year. Negligible storage.

#### Table 2: `fno_underlyings`

Daily snapshot of all F&O underlyings with their linked price IDs.

```sql
CREATE TABLE fno_underlyings (
    snapshot_date           TIMESTAMP,    -- IST date (designated timestamp)
    underlying_symbol       SYMBOL,       -- e.g., "NIFTY", "RELIANCE"
    underlying_security_id  LONG,         -- FNO phantom ID (e.g., NIFTY=26000)
    price_feed_security_id  LONG,         -- live price ID (e.g., NIFTY=13)
    price_feed_segment      SYMBOL,       -- "IDX_I" or "NSE_EQ"
    derivative_segment      SYMBOL,       -- "NSE_FNO" or "BSE_FNO"
    kind                    SYMBOL,       -- "NseIndex", "BseIndex", "Stock"
    lot_size                INT,          -- contract lot size
    contract_count          INT           -- total derivative contracts
) TIMESTAMP(snapshot_date) PARTITION BY MONTH;
```

**Volume:** ~215 rows/day = ~78K rows/year. Negligible storage.

#### Table 3: `derivative_contracts`

Daily snapshot of all active derivative contracts. Needed to decode any security_id from historical tick data.

```sql
CREATE TABLE derivative_contracts (
    snapshot_date       TIMESTAMP,    -- IST date (designated timestamp)
    security_id         LONG,         -- this contract's own security ID
    underlying_symbol   SYMBOL,       -- e.g., "NIFTY", "RELIANCE"
    instrument_kind     SYMBOL,       -- "FutureIndex", "FutureStock", "OptionIndex", "OptionStock"
    exchange_segment    SYMBOL,       -- "NSE_FNO" or "BSE_FNO"
    expiry_date         VARCHAR,      -- contract expiry as YYYY-MM-DD string (NOT timestamp)
    strike_price        DOUBLE,       -- strike price (0.0 for futures)
    option_type         SYMBOL,       -- "CE", "PE", or "" (empty string) for futures
    lot_size            INT,          -- contract lot size
    tick_size           DOUBLE,       -- minimum price movement
    symbol_name         SYMBOL,       -- full symbol (e.g., "NIFTY-Mar2026-18000-CE")
    display_name        STRING        -- human-readable display name
) TIMESTAMP(snapshot_date) PARTITION BY MONTH;
```

**Volume:** ~150K rows/day = ~55M rows/year. Partitioned by month for efficient querying. QuestDB handles this easily — it's designed for exactly this kind of time-series data.

#### Table 4: `subscribed_indices`

Daily snapshot of all 31 subscribed indices (8 F&O + 23 Display). These are the indices that get WebSocket subscriptions for market data.

```sql
CREATE TABLE subscribed_indices (
    snapshot_date   TIMESTAMP,    -- IST date (designated timestamp)
    symbol          SYMBOL,       -- e.g., "NIFTY", "INDIA VIX", "NIFTY AUTO"
    exchange        SYMBOL,       -- "NSE" or "BSE"
    category        SYMBOL,       -- "FnoUnderlying" or "DisplayIndex"
    subcategory     SYMBOL,       -- "Fno", "Volatility", "BroadMarket", "MidCap", "SmallCap", "Sectoral", "Thematic"
    security_id     LONG          -- IDX_I security ID for WebSocket subscription
) TIMESTAMP(snapshot_date) PARTITION BY MONTH;
```

**Composition:**
- **8 F&O indices** (category=FnoUnderlying, subcategory=Fno): NIFTY(13), BANKNIFTY(25), FINNIFTY(27), MIDCPNIFTY(442), NIFTYNXT50(38), SENSEX(51), BANKEX(69), SENSEX50(83)
- **23 Display indices** (category=DisplayIndex): INDIA VIX(21), NIFTY 100(17), NIFTY AUTO(14), etc. — see `DISPLAY_INDEX_ENTRIES` in constants.rs

**Volume:** 31 rows/day = ~11K rows/year. Negligible storage.

### Operational Timeline — Instrument Build

```
8:30 AM IST — Server starts. All Docker containers must be healthy.
              Health check: QuestDB, Valkey, all infra services reporting UP.
              ONLY when all services healthy → proceed to instrument build.

8:30 AM IST — CSV download begins immediately after health check passes.
  + seconds    Download + parse + 5-pass build + validation + persistence.
              Expected: 3-10 seconds total on MacBook, ~1-3 seconds on c7i.2xlarge.

8:31 AM IST — System READY with instruments loaded.
              WebSocket subscriptions can be prepared.
              Pre-market data collection starts at 9:00 AM.

FAILURE SCENARIO:
  If CSV download fails (primary + fallback + cache ALL fail):
    → INSTANT Telegram alert to Parthiban
    → System CANNOT proceed — instruments are required for everything
    → No silent failure. No waiting. Alert the moment all retries are exhausted.
    → Retry window: backon exponential backoff 2s → 8s, 3 retries per URL
    → Total worst-case time: ~30 seconds before alert fires

  If CSV downloaded but build/validation fails:
    → INSTANT Telegram alert with build error details
    → System halts instrument-dependent operations
    → Previous day's cache may be usable as fallback (degraded mode)

BY 8:45 AM IST — System MUST be fully operational with instruments loaded.
                 If not ready by 8:45 AM → SEV-1 alert.
                 Live trading system needs instruments before 9:00 AM pre-market.
```

### Incremental Persistence Strategy

```
Daily snapshots are ADDITIVE — they never delete historical data.

Day 1: CSV has 150,949 contracts → 150,949 rows inserted with snapshot_date = Day 1
Day 2: CSV has 151,200 contracts → 151,200 rows inserted with snapshot_date = Day 2
        (Day 1 data untouched — new partition)

What "incremental" means:
  - New contracts (new listings, new expiries) → automatically appear in today's snapshot
  - Changed contracts (lot size changes) → today's snapshot has new values, old snapshot preserved
  - Expired contracts → not in today's CSV, so not in today's snapshot (but historical snapshots preserved)
  - NOTHING is ever deleted from QuestDB — full audit trail

What the caller must ensure:
  - Call persist_instrument_snapshot() exactly ONCE per IST calendar day
  - Scheduler guards against double invocation (check last_persisted_date)
  - If called twice → duplicate rows (non-fatal, recoverable with SELECT DISTINCT)
```

### Persistence Strategy

```
1. build_fno_universe() completes successfully (existing Block 01)
2. Immediately after: persist_instrument_snapshot(universe, questdb_config)
3. Write order: metadata → underlyings → derivative_contracts → subscribed_indices
4. Use ILP (InfluxDB Line Protocol) for high-speed ingestion via questdb-rs 6.1.0
5. Derivative contracts (~150K rows) batched: flush every ILP_FLUSH_BATCH_SIZE (10,000) rows
6. If QuestDB write fails: WARN log + continue (non-critical observability data)
7. QuestDB write failure does NOT block trading — universe is in-memory, system functional
```

### Deduplication & Idempotency

```
Current state:
  - ILP auto-created tables have NO dedup keys
  - Calling persist_instrument_snapshot() twice on the same IST day = duplicate rows
  - The CALLER must ensure single invocation per day (scheduler responsibility)

Why this is acceptable:
  - Instrument persistence is observability data, not trading-critical
  - Duplicate rows don't corrupt data — queries with DISTINCT still return correct results
  - Daily scheduler (when built) will guard: check last_persisted_date before calling
  - Worst case: duplicate rows waste ~5MB storage — trivial

Future hardening (when scheduler is built):
  - Option A: Caller guard — check last_persisted_date in QuestDB via PG wire before writing
  - Option B: QuestDB DEDUP UPSERT KEYS(snapshot_date, security_id) via PG wire DDL
  - Option C: Both — belt and suspenders

Recovery from accidental duplicates:
  SELECT DISTINCT snapshot_date, security_id, ... FROM derivative_contracts
  WHERE snapshot_date = '2026-02-25'
  -- Always returns correct data regardless of duplicates
```

### Edge Cases & Known Behaviors

```
expiry_date:
  - Stored as VARCHAR in YYYY-MM-DD format, NOT as TIMESTAMP
  - Prevents timezone-related date shift (e.g., 2026-03-27 becoming 2026-03-26T18:30:00Z)
  - NaiveDate::to_string() produces clean YYYY-MM-DD — no .format() needed

Futures contracts:
  - option_type = "" (empty string), NOT NULL — ILP SYMBOL columns don't support NULL
  - strike_price = 0.0 — meaningful zero, not missing data
  - Downstream queries: WHERE option_type != '' filters to options only

Type safety:
  - u32 fields (security_id, lot_size) → i64::from() — infallible, zero risk
  - usize fields (counts, durations) → i64::try_from().expect() — panics if >i64::MAX (impossible)
  - f64 fields (strike_price, tick_size) → column_f64() — direct, no cast needed

IST timestamp:
  - Designated timestamp = IST midnight (00:00:00 IST) stored as UTC
  - QuestDB displays as UTC: 2026-02-24T18:30:00Z = 2026-02-25 00:00:00 IST
  - Grafana/downstream must set timezone to Asia/Kolkata for correct display

Partial write failure:
  - If flush fails mid-batch (e.g., batch 8 of 15), rows 1-7 are committed
  - Result: partial snapshot in QuestDB — metadata and underlyings present, some contracts missing
  - This is acceptable: non-critical data, next day's full snapshot covers the gap
  - Logged as WARN with table name and error details

Corporate actions (demergers, symbol changes):
  - Daily snapshot captures the instrument state AS OF THAT DAY
  - Historical queries always join on snapshot_date to get the correct mapping
  - security_id changes between days are visible via diff queries across snapshot_dates
```

### Retention

```
Hot data (QuestDB):   90 days — matches tick data retention
Cold data (S3):       5 years — SEBI audit compliance
Cleanup:              Automated via QuestDB partition drop (monthly)
```

### Implementation Files

| # | File | Purpose |
|---|------|---------|
| 1 | `crates/storage/src/instrument_persistence.rs` | QuestDB ILP writer for all 3 tables |

Dependencies: `questdb-rs` (ILP client from Tech Stack Bible).

### Queries for Historical Lookups

```sql
-- What was security_id 12345 on 2026-03-15?
SELECT * FROM derivative_contracts
WHERE snapshot_date = '2026-03-15' AND security_id = 12345;

-- How many contracts did NIFTY have over the last 30 days?
SELECT snapshot_date, contract_count FROM fno_underlyings
WHERE underlying_symbol = 'NIFTY'
AND snapshot_date > dateadd('d', -30, now());

-- Build health over the last week
SELECT * FROM instrument_build_metadata
WHERE build_date > dateadd('d', -7, now())
ORDER BY build_date;

-- When did RELIANCE lot size change?
SELECT snapshot_date, lot_size FROM fno_underlyings
WHERE underlying_symbol = 'RELIANCE'
ORDER BY snapshot_date;
```
