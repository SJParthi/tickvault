# Instrument Dhan Dependency Map — Every External Contract

> **Purpose:** Catalog EVERY point where our code depends on Dhan's external
> behavior. For each dependency: what we assume, where the assumption lives
> in code, what Dhan change would break it, will it fail LOUD or SILENT,
> and how to future-proof it.
>
> **Goal:** Even if Dhan changes everything, we NEVER struggle. We either
> adapt automatically or fail loudly with a Telegram CRITICAL alert telling
> us exactly what changed.
>
> **Last updated:** 2026-03-11

---

## DEPENDENCY CATEGORIES

| Category | Count | Risk if Dhan Changes |
|----------|-------|---------------------|
| A. CSV URLs | 2 | Download fails → LOUD |
| B. CSV Column Names | 15 | Parse fails → LOUD |
| C. CSV Data Values | 6 | Build fails or wrong data → MIXED |
| D. Security ID Contracts | 8 | Wrong mapping → LOUD (validation) |
| E. Index Aliases | 2 | Index not linked → WARN |
| F. Binary Protocol | 10 | Parse fails → LOUD |
| G. Exchange Segment Codes | 8 | Wrong segment → SILENT risk |
| H. WebSocket Protocol | 4 | Connection fails → LOUD |
| I. Subscription Limits | 3 | Over-subscription → LOUD |
| J. REST API Contracts | 3 | API calls fail → LOUD |

---

## A. CSV URLS (2 dependencies)

| # | Dependency | Value | File | Line | Dhan Change | Failure Mode |
|---|-----------|-------|------|------|-------------|--------------|
| A1 | Primary CSV URL | `https://images.dhan.co/api-data/api-scrip-master-detailed.csv` | `config/base.toml` | 96 | URL moves | **LOUD** — download fails, tries fallback |
| A2 | Fallback CSV URL | `https://images.dhan.co/api-data/api-scrip-master.csv` | `config/base.toml` | 97 | URL moves | **LOUD** — both fail, tries cache, then halts |

**Current resilience:** 3 retries per URL + fallback URL + cached CSV + cached rkyv.
**Future-proof:** URLs are in config, not hardcoded. Change config = fixed. No code change needed.

---

## B. CSV COLUMN NAMES (15 dependencies)

| # | Column Name | Constant | File |
|---|------------|----------|------|
| B1 | `EXCH_ID` | `CSV_COLUMN_EXCH_ID` | `constants.rs:610` |
| B2 | `SEGMENT` | `CSV_COLUMN_SEGMENT` | `constants.rs:611` |
| B3 | `SECURITY_ID` | `CSV_COLUMN_SECURITY_ID` | `constants.rs:612` |
| B4 | `INSTRUMENT` | `CSV_COLUMN_INSTRUMENT` | `constants.rs:613` |
| B5 | `UNDERLYING_SECURITY_ID` | `CSV_COLUMN_UNDERLYING_SECURITY_ID` | `constants.rs:614` |
| B6 | `UNDERLYING_SYMBOL` | `CSV_COLUMN_UNDERLYING_SYMBOL` | `constants.rs:615` |
| B7 | `SYMBOL_NAME` | `CSV_COLUMN_SYMBOL_NAME` | `constants.rs:616` |
| B8 | `DISPLAY_NAME` | `CSV_COLUMN_DISPLAY_NAME` | `constants.rs:617` |
| B9 | `SERIES` | `CSV_COLUMN_SERIES` | `constants.rs:618` |
| B10 | `LOT_SIZE` | `CSV_COLUMN_LOT_SIZE` | `constants.rs:619` |
| B11 | `SM_EXPIRY_DATE` | `CSV_COLUMN_EXPIRY_DATE` | `constants.rs:620` |
| B12 | `STRIKE_PRICE` | `CSV_COLUMN_STRIKE_PRICE` | `constants.rs:621` |
| B13 | `OPTION_TYPE` | `CSV_COLUMN_OPTION_TYPE` | `constants.rs:622` |
| B14 | `TICK_SIZE` | `CSV_COLUMN_TICK_SIZE` | `constants.rs:623` |
| B15 | `EXPIRY_FLAG` | `CSV_COLUMN_EXPIRY_FLAG` | `constants.rs:624` |

**Dhan change:** Column renamed (e.g., `EXCH_ID` → `EXCHANGE`)
**Failure mode:** **LOUD** — `detect_column_indices()` fails → `parse_instrument_csv()`
returns Err → system halts with error naming the exact missing column.

**Current resilience:**
- Auto-detect column positions from header row (no hardcoded indices)
- Extra columns ignored (Dhan can add columns freely)
- BOM handling (UTF-8 BOM stripped)
- Trailing comma tolerance
- Whitespace in header names handled

**Future-proof:** Column names are constants. If Dhan renames a column:
1. System fails LOUD with exact column name in error
2. Fix: update ONE constant in `constants.rs`
3. No logic change needed — auto-detection handles position shifts

**Test:** `test_column_rename_detection_fails_loudly` — verifies exact error
message when `EXCH_ID` is renamed to `EXCHANGE`.

---

## C. CSV DATA VALUES (6 dependencies)

| # | Assumption | Value | Where Used | Dhan Change | Failure Mode |
|---|-----------|-------|-----------|-------------|--------------|
| C1 | Exchange IDs | `"NSE"`, `"BSE"` | `csv_parser.rs:should_include_row()` | New exchange added (e.g., "MCX") | **SILENT** — new exchange rows filtered out. OK by design — we only want NSE/BSE. |
| C2 | Segment codes | `'I'`, `'E'`, `'D'` | `csv_parser.rs:should_include_row()` | New segment code (e.g., 'C' for currency) | **SILENT** — new segment filtered out. OK for now, problem if we want currency later. |
| C3 | Instrument types | `"FUTIDX"`, `"FUTSTK"`, `"OPTIDX"`, `"OPTSTK"` | `universe_builder.rs:pass3/pass5` | New type (e.g., `"FUTCUR"`) | **SILENT** — unknown types skipped. OK unless it's a type we need. |
| C4 | Option types | `"CE"`, `"PE"` | `csv_parser.rs:parse_row()` | New option type | **LOUD** — parse fails for that row, logged as error |
| C5 | Expiry date format | `"YYYY-MM-DD"` | `csv_parser.rs:parse_expiry_date()` | Format changes (e.g., `"DD-MM-YYYY"`) | **LOUD** — NaiveDate parse fails → `None` → contract filtered as no-expiry |
| C6 | Lot size as float | `"75.0"` (parsed as f64 → u32) | `csv_parser.rs:parse_lot_size()` | Changes to integer `"75"` | **SAFE** — `parse::<f64>()` handles both `"75"` and `"75.0"` |

**Future-proof for C1-C3:** These are **intentional filters**. We ONLY want
NSE/BSE, segments I/E/D, and known instrument types. If Dhan adds new exchanges
or segments, our filter correctly ignores them. When we WANT to support new
segments (e.g., currency), we add to the filter — one-line change.

---

## D. SECURITY ID CONTRACTS (8 must-exist indices + 1 equity + 4 stocks)

### D1. Must-Exist Index Security IDs

| Symbol | Expected IDX_I ID | Constant | Risk |
|--------|------------------|----------|------|
| NIFTY | 13 | `VALIDATION_MUST_EXIST_INDICES[0]` | If Dhan changes NIFTY from 13 → new ID |
| BANKNIFTY | 25 | `VALIDATION_MUST_EXIST_INDICES[1]` | Same |
| FINNIFTY | 27 | `VALIDATION_MUST_EXIST_INDICES[2]` | Same |
| MIDCPNIFTY | 442 | `VALIDATION_MUST_EXIST_INDICES[3]` | Same |
| NIFTYNXT50 | 38 | `VALIDATION_MUST_EXIST_INDICES[4]` | Same |
| SENSEX | 51 | `VALIDATION_MUST_EXIST_INDICES[5]` | Same |
| BANKEX | 69 | `VALIDATION_MUST_EXIST_INDICES[6]` | Same |
| SENSEX50 | 83 | `VALIDATION_MUST_EXIST_INDICES[7]` | Same |

**File:** `constants.rs:736-745`
**Dhan change:** NIFTY security_id changes from 13 to something else.
**Failure mode:** **LOUD** — validation Check 1 fails → system halts with
`"NIFTY: expected IDX_I security_id 13, got <new_id>"`.

**Fix:** Update the constant. ONE number change.
**Likelihood:** Extremely low. These IDs have been stable for years.
Dhan would break every client if they changed index IDs.

### D2. Must-Exist Equity Security ID

| Symbol | Expected NSE_EQ ID | Constant |
|--------|-------------------|----------|
| RELIANCE | 2885 | `VALIDATION_MUST_EXIST_EQUITIES` |

**File:** `constants.rs:749`
**Failure mode:** **LOUD** — Check 2 fails → system halts.

### D3. Must-Exist F&O Stocks

| Symbol | Constant |
|--------|----------|
| RELIANCE | `VALIDATION_MUST_EXIST_FNO_STOCKS[0]` |
| HDFCBANK | `VALIDATION_MUST_EXIST_FNO_STOCKS[1]` |
| INFY | `VALIDATION_MUST_EXIST_FNO_STOCKS[2]` |
| TCS | `VALIDATION_MUST_EXIST_FNO_STOCKS[3]` |

**File:** `constants.rs:752`
**Dhan change:** Stock removed from F&O segment (e.g., SEBI removes TCS).
**Failure mode:** **LOUD** — Check 3 fails → system halts.
**Fix:** Remove TCS from the constant. But this is a GOOD failure — we WANT
to know if a major stock disappears from F&O.

---

## E. INDEX ALIASES (2 dependencies)

| # | FNO Symbol | IDX_I Symbol | Constant |
|---|-----------|-------------|----------|
| E1 | `NIFTYNXT50` | `NIFTY NEXT 50` | `INDEX_SYMBOL_ALIASES[0]` |
| E2 | `SENSEX50` | `SNSX50` | `INDEX_SYMBOL_ALIASES[1]` |

**File:** `constants.rs:727-728`
**Dhan change:** Dhan renames the IDX_I symbol (e.g., `"NIFTY NEXT 50"` →
`"NIFTY NXT 50"`).
**Failure mode:** **WARN** — alias lookup fails → index price feed not linked →
validation Check 9 warns → system continues but index derivatives have no
live price feed for underlying.

**Future-proof:** If a new alias appears (e.g., Dhan adds `NIFTY100` index
with FNO symbol `NFTYHUNDRED`), the fix is one line added to
`INDEX_SYMBOL_ALIASES`.

---

## F. BINARY WEBSOCKET PROTOCOL (10 dependencies)

### F1. Packet Sizes

| Code | Type | Size (bytes) | Constant | File |
|------|------|-------------|----------|------|
| 1 | Index Ticker | 16 | `TICKER_PACKET_SIZE` | `constants.rs:14` |
| 2 | Ticker | 16 | `TICKER_PACKET_SIZE` | `constants.rs:14` |
| 3 | Market Depth | 112 | `MARKET_DEPTH_PACKET_SIZE` | `constants.rs:22` |
| 4 | Quote | 50 | `QUOTE_PACKET_SIZE` | `constants.rs:18` |
| 5 | OI Data | 12 | `OI_PACKET_SIZE` | `constants.rs:219` |
| 6 | Previous Close | 16 | `PREVIOUS_CLOSE_PACKET_SIZE` | `constants.rs:30` |
| 7 | Market Status | 8 | `MARKET_STATUS_PACKET_SIZE` | `constants.rs:34` |
| 8 | Full Packet | 162 | `FULL_QUOTE_PACKET_SIZE` | `constants.rs:26` |
| 50 | Disconnect | 10 | `DISCONNECT_PACKET_SIZE` | `constants.rs:38` |

**Dhan change:** Packet size changes (e.g., adds a field to Ticker).
**Failure mode:** **LOUD** — size validation in dispatcher rejects undersized
packets. Logs parse error with exact response code and expected vs actual size.

**Compile-time enforcement:**
```rust
const _: () = assert!(TICKER_PACKET_SIZE == 16, "ticker total = 16");
const _: () = assert!(QUOTE_PACKET_SIZE == 50, "quote total = 50");
// ... (all packet sizes have compile-time assertions)
```

### F2. Binary Header Layout

```
Offset 0: response_code (u8)    — HEADER_OFFSET_RESPONSE_CODE = 0
Offset 1: msg_length (u16 LE)
Offset 3: exchange_segment (u8) — HEADER_OFFSET_EXCHANGE_SEGMENT = 3
Offset 4: security_id (u32 LE)  — HEADER_OFFSET_SECURITY_ID = 4
Total: 8 bytes                  — BINARY_HEADER_SIZE = 8
```

**File:** `constants.rs:215` + `parser/header.rs`
**Dhan change:** Header layout changes (e.g., adds a version byte).
**Failure mode:** **LOUD** — all packets fail to parse → massive parse error
rate → Grafana alert fires within seconds.

### F3. Response Code Mapping

| Code | Meaning | Constant |
|------|---------|----------|
| 1 | Index Ticker | `RESPONSE_CODE_INDEX_TICKER` |
| 2 | Ticker | `RESPONSE_CODE_TICKER` |
| 3 | Market Depth | `RESPONSE_CODE_MARKET_DEPTH` |
| 4 | Quote | `RESPONSE_CODE_QUOTE` |
| 5 | OI | `RESPONSE_CODE_OI` |
| 6 | Previous Close | `RESPONSE_CODE_PREVIOUS_CLOSE` |
| 7 | Market Status | `RESPONSE_CODE_MARKET_STATUS` |
| 8 | Full Packet | `RESPONSE_CODE_FULL` |
| 50 | Disconnect | `RESPONSE_CODE_DISCONNECT` |

**Dhan change:** New response code added (e.g., code 9 for something new).
**Failure mode:** **LOUD** — dispatcher logs `"unknown response_code: 9"` →
packet dropped → parse error counter increments → Grafana alert.

**Future-proof:** Unknown codes are logged and dropped. System continues
processing known codes. Adding support = one match arm in dispatcher.

---

## G. EXCHANGE SEGMENT CODES (8 dependencies)

| Code | Segment | Constant | Used Where |
|------|---------|----------|-----------|
| 0 | IDX_I | `EXCHANGE_SEGMENT_IDX_I` | Parser, subscription |
| 1 | NSE_EQ | `EXCHANGE_SEGMENT_NSE_EQ` | Parser, subscription |
| 2 | NSE_FNO | `EXCHANGE_SEGMENT_NSE_FNO` | Parser, subscription |
| 3 | NSE_CURRENCY | `EXCHANGE_SEGMENT_NSE_CURRENCY` | Parser only |
| 4 | BSE_EQ | `EXCHANGE_SEGMENT_BSE_EQ` | Parser only |
| 5 | MCX_COMM | `EXCHANGE_SEGMENT_MCX_COMM` | Parser only |
| 7 | BSE_CURRENCY | `EXCHANGE_SEGMENT_BSE_CURRENCY` | Parser only |
| 8 | BSE_FNO | `EXCHANGE_SEGMENT_BSE_FNO` | Parser, subscription |

**File:** `constants.rs:84-107`
**Note:** Code 6 is unused/skipped in Dhan's protocol.

**Dhan change:** New segment code (e.g., code 6 = new segment).
**Failure mode:** **PARTIAL SILENT** — `segment_code_to_str()` returns
`"UNKNOWN"` for code 6. Ticks still persisted with segment="UNKNOWN".
No data loss, but no segment identification either.

**Risk:** This is the ONE area where failure is partially silent. The tick
data is saved, but the segment label is wrong.

**Future-proof:** `ExchangeSegment` enum has exhaustive match. Adding a new
segment = one enum variant + one match arm. Compiler catches all missing
handlers.

---

## H. WEBSOCKET PROTOCOL (4 dependencies)

| # | Dependency | Value | File | Dhan Change | Failure Mode |
|---|-----------|-------|------|-------------|--------------|
| H1 | WS URL | `wss://api-feed.dhan.co` | `config/base.toml:92` | URL changes | **LOUD** — connection fails |
| H2 | Order WS URL | `wss://api-order-update.dhan.co` | `config/base.toml:93` | URL changes | **LOUD** — connection fails |
| H3 | Auth via query param | `?token=<jwt>&clientId=<id>&authType=2` | `connection.rs` | Auth method changes | **LOUD** — 807 disconnect |
| H4 | Disconnect codes | 801-814 (12 codes) | `types.rs` | New disconnect code | **SAFE** — unknown codes treated as non-reconnectable |

**Future-proof:** URLs are in config. Disconnect codes have default handling.
Auth method is the riskiest — if Dhan changes WS auth, code change required.

---

## I. SUBSCRIPTION LIMITS (3 dependencies)

| # | Limit | Value | Constant | File |
|---|-------|-------|----------|------|
| I1 | Max instruments per connection | 5,000 | Config: `max_instruments_per_connection` | `base.toml:98` |
| I2 | Max WebSocket connections | 5 | Config: `max_websocket_connections` | `base.toml:99` |
| I3 | Batch size per subscribe message | 100 | `SUBSCRIPTION_BATCH_SIZE` | `constants.rs` |

**Dhan change:** Limit changes (e.g., max 10,000 per connection).
**Failure mode:** **SAFE** — we'd just be under-utilizing capacity. If limit
DECREASES, we'd over-subscribe → Dhan rejects → **LOUD**.

**Future-proof:** All limits are in config/constants. Change one number.

---

## J. REST API CONTRACTS (3 dependencies)

| # | Dependency | Value | File | Dhan Change | Failure Mode |
|---|-----------|-------|------|-------------|--------------|
| J1 | Auth URL | `https://auth.dhan.co` | `config/base.toml:95` | URL changes | **LOUD** — auth fails |
| J2 | API URL | `https://api.dhan.co/v2` | `config/base.toml:94` | URL/version changes | **LOUD** — API fails |
| J3 | Header: `access-token` | Raw JWT in header | `token_manager.rs` | Header name changes | **LOUD** — 401 Unauthorized |

---

## FUTURE-PROOF SCORECARD

### What Happens Automatically (Zero Code Change)

| Dhan Change | Why We're Safe |
|-------------|---------------|
| New columns added to CSV | Auto-detect ignores unknown columns |
| New exchange added (e.g., MCX) | Filter correctly excludes it |
| New F&O stocks added | CSV rebuild discovers them daily |
| Contracts expire | Pass 5 filters `expiry_date < today` |
| Lot sizes change | Delta detector catches and logs |
| New strike prices listed | Pass 5 picks them up |
| Corporate actions (symbol rename) | Daily rebuild self-heals |
| More than 5 WS connections allowed | We just use 5 (under-utilize) |

### What Fails LOUD (Telegram Alert, System Halts)

| Dhan Change | What Happens | Fix Time |
|-------------|-------------|----------|
| CSV URL moves | Download fails → fallback → cache → halt | Config change: 1 min |
| CSV column renamed | Parse fails with exact column name | Constant change: 1 min |
| Index security_id changes | Validation Check 1 fails | Constant change: 1 min |
| Binary packet size changes | Parse rejects undersized packets | Constant change: 5 min |
| WS URL changes | Connection fails | Config change: 1 min |
| API URL changes | Auth fails | Config change: 1 min |
| Major stock removed from F&O | Validation Check 3 fails | Constant change: 1 min |

### What Could Fail SILENT (Needs Monitoring)

| Dhan Change | Risk | Mitigation |
|-------------|------|-----------|
| New segment code (code 6) | Ticks labeled "UNKNOWN" | Monitor `segment="UNKNOWN"` in QuestDB |
| New response code in WS | Packets dropped as unknown | Monitor parse error rate in Grafana |
| New instrument type in CSV | Rows silently filtered | Monitor derivative_count trend |
| Index alias changes | Index not linked to price feed | Check 9 WARN in logs |
| Expiry date format changes | Contracts filtered as no-expiry | Monitor derivative_count drop |

---

## HOW TO HANDLE THE WORST CASE: DHAN CHANGES EVERYTHING

### Scenario: Dhan releases "V3 API" — new CSV format, new binary protocol

**Phase 1 (Immediate — system auto-detects):**
```
1. CSV download succeeds (URL still works)
2. Column detection fails → exact missing column names in error
3. System halts with: "missing required column: EXCH_ID"
4. Telegram CRITICAL alert sent
5. Cached rkyv from yesterday still works → can restart in market hours
```

**Phase 2 (Fix — 30 minutes):**
```
1. Read Dhan's V3 changelog
2. Update column name constants (if renamed)
3. Add new columns to parser (if required)
4. Update binary protocol constants (if sizes changed)
5. cargo test → all 408+ tests validate the fix
6. Deploy
```

**Phase 3 (Verify — automatic):**
```
1. System downloads new CSV → parse succeeds
2. Validation checks pass
3. Instruments subscribed
4. Ticks flowing
5. Grafana shows normal metrics
```

### Why We Never Struggle

| Principle | How It's Enforced |
|-----------|------------------|
| **Fail LOUD, never SILENT** | Every external dependency has a validation check. Missing column = halt. Wrong ID = halt. Bad packet = logged + dropped. |
| **Single constant to fix** | Every Dhan-dependent value is a named constant in `constants.rs` or a config value in `base.toml`. Never scattered across code. |
| **Cached fallback** | Even if Dhan's entire API is down, cached rkyv from yesterday works for the full trading day. |
| **Daily self-healing** | CSV rebuild at 08:45 IST discovers all changes (new contracts, lot sizes, symbols). Zero manual intervention for normal changes. |
| **408+ tests** | Any code change to handle Dhan's new format is validated against 408+ tests before deploy. |
| **Compile-time assertions** | Packet sizes, header sizes, enum exhaustiveness — compiler catches mismatches before runtime. |

---

## MONITORING CHECKLIST (Daily Health)

| Check | Query/Alert | Catches |
|-------|------------|---------|
| Build succeeded today | `instrument_build_metadata WHERE timestamp = today()` | CSV download or parse failure |
| Derivative count normal | `SELECT count() FROM derivative_contracts WHERE timestamp = (SELECT max(timestamp) FROM derivative_contracts)` — between 50K-200K | Truncated CSV, format change, cross-day accumulation (I-P1-08) |
| Underlying count normal | `SELECT count() FROM fno_underlyings WHERE timestamp = (SELECT max(timestamp) FROM fno_underlyings)` — between 150-300 | Major F&O list change, cross-day accumulation (I-P1-08) |
| Parse error rate = 0 | `dlt_tick_parse_errors_total` | Binary protocol change |
| Unknown segments = 0 | `SELECT count() WHERE segment='UNKNOWN'` | New segment code |
| Lifecycle events reasonable | < 2000 events/day | Massive CSV format change |
| All 8 must-exist indices present | Validation Check 1 passes | Index ID change |
| WebSocket connected | `dlt_websocket_connections_active` | WS URL/auth change |

> **CRITICAL (I-P1-08):** ALL Grafana queries on instrument snapshot tables
> (`fno_underlyings`, `derivative_contracts`, `subscribed_indices`) MUST filter
> by date. These tables accumulate rows across days by design. Unfiltered
> `SELECT count()` will show N x daily_count where N = number of days of data.
> Use `WHERE timestamp = (SELECT max(timestamp) FROM table)` for current counts.

---

## CROSS-REFERENCE TO GAPS

| Dependency Area | Related Gap IDs |
|----------------|----------------|
| CSV column names | None — already resilient (auto-detect + LOUD fail) |
| Security IDs | I-P1-03 (reuse), I-P1-04 (reassignment), I-P1-05 (historical) |
| Segment codes | I-P3-01 (duplicated code), I-P1-06 (dedup key) |
| Storage survival | I-P0-04 (EBS), I-P0-05 (S3), I-P0-06 (emergency override) |
| Validation | I-P0-01 (duplicate ID), I-P0-02 (count consistency) |
| Order safety | I-P0-03 (expiry Gate 4) |
| Operational | I-P1-01 (daily refresh), I-P1-07 (Unavailable = CRITICAL) |
| Observability/Grafana | I-P1-08 (cross-day snapshot accumulation — RESOLVED) |
