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

### QuestDB Tables (3)

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
    expiry_date         TIMESTAMP,    -- contract expiry date
    strike_price        DOUBLE,       -- strike price (0.0 for futures)
    option_type         SYMBOL,       -- "CE", "PE", or NULL for futures
    lot_size            INT,          -- contract lot size
    tick_size           DOUBLE,       -- minimum price movement
    symbol_name         SYMBOL,       -- full symbol (e.g., "NIFTY-Mar2026-18000-CE")
    display_name        STRING        -- human-readable display name
) TIMESTAMP(snapshot_date) PARTITION BY MONTH;
```

**Volume:** ~150K rows/day = ~55M rows/year. Partitioned by month for efficient querying. QuestDB handles this easily — it's designed for exactly this kind of time-series data.

### Persistence Strategy

```
1. build_fno_universe() completes successfully (existing Block 01)
2. Immediately after: persist_instrument_snapshot(universe, questdb_client)
3. Write order: metadata → underlyings → derivative_contracts
4. Use ILP (InfluxDB Line Protocol) for high-speed ingestion
5. All writes are idempotent — same (snapshot_date, security_id) = same row
6. If QuestDB write fails: WARN log + continue (universe is in-memory, system still functional)
7. QuestDB write failure does NOT block trading — it's observability data, not critical path
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
