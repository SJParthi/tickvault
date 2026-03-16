# Instrument Complete Reference — Never Struggle Again

> **Purpose:** Single definitive quick-reference for EVERY instrument detail.
> Type fields, function signatures, config keys, data flow, file locations,
> constants, QuestDB schemas, test coverage — all in one place.
>
> **Cross-references:**
> - Architecture: `instrument-technical-design.md`
> - Guarantees: `instrument-guarantee.md`
> - Dhan Dependencies: `instrument-dhan-dependency-map.md`
> - Gap Enforcement: `instrument-gaps-consolidated.md`
>
> **Last updated:** 2026-03-15

---

## TABLE OF CONTENTS

1. [File Map](#1-file-map)
2. [Complete Type Reference](#2-complete-type-reference)
3. [5-Pass Build Algorithm](#3-5-pass-build-algorithm)
4. [CSV Download & Caching](#4-csv-download--caching)
5. [Binary Cache (rkyv)](#5-binary-cache-rkyv)
6. [Subscription Planner](#6-subscription-planner)
7. [Delta Detection](#7-delta-detection)
8. [Validation](#8-validation)
9. [QuestDB Persistence](#9-questdb-persistence)
10. [Configuration Reference](#10-configuration-reference)
11. [Constants Reference](#11-constants-reference)
12. [Data Flow End-to-End](#12-data-flow-end-to-end)
13. [Test Coverage Map](#13-test-coverage-map)
14. [Troubleshooting Quick Reference](#14-troubleshooting-quick-reference)

---

## 1. FILE MAP

### crates/common/src/ (Types & Registry)

| File | What It Contains |
|------|-----------------|
| `types.rs` | `SecurityId` (u32), `Exchange`, `ExchangeSegment`, `FeedMode`, `InstrumentType`, `OptionType` |
| `instrument_types.rs` | `FnoUniverse`, `FnoUnderlying`, `DerivativeContract`, `OptionChain`, `OptionChainEntry`, `OptionChainKey`, `ExpiryCalendar`, `InstrumentInfo`, `SubscribedIndex`, `UniverseBuildMetadata`, all rkyv helpers, classification enums |
| `instrument_registry.rs` | `InstrumentRegistry`, `SubscribedInstrument`, `SubscriptionCategory`, builder helpers |
| `constants.rs` | CSV columns, CSV values, index lists, validation thresholds, QuestDB table names |
| `config.rs` | `InstrumentConfig`, `SubscriptionConfig` (deserialized from TOML) |

### crates/core/src/instrument/ (Pipeline)

| File | What It Does |
|------|-------------|
| `mod.rs` | Re-exports: `MappedUniverse`, `load_or_build_instruments`, `build_fno_universe`, `build_subscription_plan` |
| `csv_downloader.rs` | HTTP download with retry + fallback + cache. `download_instrument_csv()`, `load_cached_csv()` |
| `csv_parser.rs` | Column-index-based CSV parsing. `parse_instrument_csv()` → `Vec<ParsedInstrumentRow>` |
| `universe_builder.rs` | 5-pass algorithm. `build_fno_universe()`, `build_fno_universe_from_csv()` → `FnoUniverse` |
| `validation.rs` | Post-build validation. `validate_fno_universe()` — 5 checks, any failure = hard error |
| `instrument_loader.rs` | Market-hours-aware orchestrator. `load_or_build_instruments()`, `try_rebuild_instruments()` |
| `binary_cache.rs` | rkyv serialize/deserialize. `write_binary_cache()`, `read_binary_cache()`, `MappedUniverse` |
| `subscription_planner.rs` | Universe → filtered list. `build_subscription_plan()`, `build_subscription_plan_from_archived()` |
| `delta_detector.rs` | Day-over-day diff. `detect_universe_delta()` → `Vec<LifecycleEvent>` |
| `daily_scheduler.rs` | Background tokio task for daily refresh. `DailyRefreshConfig`, `compute_next_trigger_time()` |
| `s3_backup.rs` | S3 backup/restore for disaster recovery. `S3BackupConfig`, `backup_to_s3()`, `restore_from_s3()` |
| `diagnostic.rs` | End-to-end health check. `run_instrument_diagnostic()` → `DiagnosticReport` |

### crates/storage/src/

| File | What It Does |
|------|-------------|
| `instrument_persistence.rs` | QuestDB ILP writer. `persist_instrument_snapshot()`, `persist_lifecycle_events()`, DDL for 5 tables |

---

## 2. COMPLETE TYPE REFERENCE

### 2.1 Primitive Types (types.rs)

```rust
pub type SecurityId = u32;  // Unique Dhan instrument ID. NOT stable across days for derivatives.
```

#### Exchange
```rust
pub enum Exchange {
    NationalStockExchange,  // NSE
    BombayStockExchange,    // BSE
}
// Methods: as_str() → "NSE" | "BSE", Display
```

#### ExchangeSegment
```rust
pub enum ExchangeSegment {
    IdxI,          // 0 — Index segment (used for index value feeds)
    NseEquity,     // 1 — NSE Equity (stock price feeds)
    NseFno,        // 2 — NSE F&O (all NSE derivatives)
    NseCurrency,   // 3 — NSE Currency (not used)
    BseEquity,     // 4 — BSE Equity (not used in subscriptions)
    McxComm,       // 5 — MCX Commodity (not used)
    // GAP: No enum 6
    BseCurrency,   // 7 — BSE Currency (not used)
    BseFno,        // 8 — BSE F&O (SENSEX, BANKEX, SENSEX50 only)
}
// Methods: as_str() → "IDX_I" | "NSE_EQ" | ..., binary_code() → u8, Display
// CRITICAL: from_byte() must return None for unknown values including 6
```

#### FeedMode
```rust
pub enum FeedMode {
    Ticker,  // 16 bytes: header + LTP + LTT
    Quote,   // 50 bytes: price + volume + OHLC
    Full,    // 162 bytes: quote + OI + 5-level depth
}
```

#### InstrumentType
```rust
pub enum InstrumentType {
    Index,   // Maps to Dhan: "INDEX"
    Equity,  // Maps to Dhan: "EQUITY"
    Future,  // Maps to Dhan: "FUTIDX" (index) or "FUTSTK" (stock)
    Option,  // Maps to Dhan: "OPTIDX" (index) or "OPTSTK" (stock)
}
// Method: dhan_api_instrument_type(is_index_underlying: bool) → &str
```

#### OptionType
```rust
pub enum OptionType {
    Call,  // "CE"
    Put,   // "PE"
}
```

### 2.2 Classification Enums (instrument_types.rs)

#### UnderlyingKind — What is the underlying?
```rust
pub enum UnderlyingKind {
    NseIndex,  // NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, NIFTYNXT50
    BseIndex,  // SENSEX, BANKEX, SENSEX50
    Stock,     // RELIANCE, HDFCBANK, ~206 F&O stocks
}
```

#### DhanInstrumentKind — Dhan's 4 derivative types
```rust
pub enum DhanInstrumentKind {
    FutureIndex,   // "FUTIDX"
    FutureStock,   // "FUTSTK"
    OptionIndex,   // "OPTIDX"
    OptionStock,   // "OPTSTK"
}
```

#### IndexCategory
```rust
pub enum IndexCategory {
    FnoUnderlying,  // Has derivatives (8 indices: NIFTY, BANKNIFTY, etc.)
    DisplayIndex,   // Dashboard only (23 indices: INDIA VIX, sectoral, etc.)
}
```

#### IndexSubcategory
```rust
pub enum IndexSubcategory {
    Volatility,   // INDIA VIX
    BroadMarket,  // NIFTY 100, NIFTY 200, NIFTY 500
    MidCap,       // NIFTYMCAP50, NIFTY MIDCAP 150
    SmallCap,     // NIFTY SMALLCAP 50/100/250
    Sectoral,     // AUTO, PVT BANK, FMCG, ENERGY, IT, METAL, PHARMA, etc.
    Thematic,     // NIFTY CONSUMPTION
    Fno,          // F&O index (when category is FnoUnderlying)
}
```

### 2.3 Core Data Structures (instrument_types.rs)

#### FnoUniverse — THE MASTER OUTPUT
```rust
pub struct FnoUniverse {
    pub underlyings: HashMap<String, FnoUnderlying>,                  // key: underlying_symbol
    pub derivative_contracts: HashMap<SecurityId, DerivativeContract>, // key: contract security_id
    pub instrument_info: HashMap<SecurityId, InstrumentInfo>,         // key: ANY security_id → what is it
    pub option_chains: HashMap<OptionChainKey, OptionChain>,          // key: (symbol, expiry_date)
    pub expiry_calendars: HashMap<String, ExpiryCalendar>,            // key: underlying_symbol
    pub subscribed_indices: Vec<SubscribedIndex>,                     // 31 indices (8 FnO + 23 display)
    pub build_metadata: UniverseBuildMetadata,
}
```

#### FnoUnderlying — One F&O-eligible underlying
```rust
pub struct FnoUnderlying {
    pub underlying_symbol: String,           // "NIFTY", "RELIANCE"
    pub underlying_security_id: SecurityId,  // From CSV UNDERLYING_SECURITY_ID (e.g., 26000 for NIFTY)
    pub price_feed_security_id: SecurityId,  // IDX_I for indices, NSE_EQ for stocks (for WebSocket sub)
    pub price_feed_segment: ExchangeSegment, // IdxI or NseEquity
    pub derivative_segment: ExchangeSegment, // NseFno or BseFno
    pub kind: UnderlyingKind,                // NseIndex | BseIndex | Stock
    pub lot_size: u32,                       // Contract lot size (e.g., 75 for NIFTY)
    pub contract_count: usize,               // Total derivative contracts for this underlying
}
```

#### DerivativeContract — One futures/options contract
```rust
pub struct DerivativeContract {
    pub security_id: SecurityId,              // THIS contract's unique ID (NOT the underlying's)
    pub underlying_symbol: String,            // "NIFTY", "RELIANCE"
    pub instrument_kind: DhanInstrumentKind,  // FutureIndex | FutureStock | OptionIndex | OptionStock
    pub exchange_segment: ExchangeSegment,    // NseFno or BseFno
    pub expiry_date: NaiveDate,               // Contract expiry
    pub strike_price: f64,                    // 0.0 for futures
    pub option_type: Option<OptionType>,      // Some(Call/Put) for options, None for futures
    pub lot_size: u32,
    pub tick_size: f64,
    pub symbol_name: String,                  // "NIFTY-Mar2026-18000-CE"
    pub display_name: String,                 // "NIFTY 18000 CE Mar26"
}
```

#### OptionChain — All strikes for one (underlying, expiry)
```rust
pub struct OptionChain {
    pub underlying_symbol: String,
    pub expiry_date: NaiveDate,
    pub calls: Vec<OptionChainEntry>,           // Sorted by strike ascending
    pub puts: Vec<OptionChainEntry>,            // Sorted by strike ascending
    pub future_security_id: Option<SecurityId>, // Matching future if exists
}
```

#### OptionChainEntry — One strike in the chain
```rust
pub struct OptionChainEntry {
    pub security_id: SecurityId,
    pub strike_price: f64,
    pub lot_size: u32,
}
```

#### OptionChainKey — Composite key for option_chains HashMap
```rust
pub struct OptionChainKey {
    pub underlying_symbol: String,  // "NIFTY"
    pub expiry_date: NaiveDate,     // 2026-03-27
}
// Derives: Hash, PartialEq, Eq (used as HashMap key)
```

#### ExpiryCalendar — All expiry dates for an underlying
```rust
pub struct ExpiryCalendar {
    pub underlying_symbol: String,
    pub expiry_dates: Vec<NaiveDate>,  // Sorted ascending, all >= today
}
```

#### InstrumentInfo — Global lookup: security_id → what is it?
```rust
pub enum InstrumentInfo {
    Index { security_id: SecurityId, symbol: String, exchange: Exchange },
    Equity { security_id: SecurityId, symbol: String },
    Derivative {
        security_id: SecurityId,
        underlying_symbol: String,
        instrument_kind: DhanInstrumentKind,
        exchange_segment: ExchangeSegment,
        expiry_date: NaiveDate,
        strike_price: f64,
        option_type: Option<OptionType>,
    },
}
// Used by tick processor to enrich incoming ticks with instrument metadata
```

#### SubscribedIndex — One index in the subscription list
```rust
pub struct SubscribedIndex {
    pub symbol: String,                  // "NIFTY", "INDIA VIX"
    pub security_id: SecurityId,         // IDX_I security ID
    pub exchange: Exchange,              // NSE or BSE
    pub category: IndexCategory,         // FnoUnderlying or DisplayIndex
    pub subcategory: IndexSubcategory,   // Fno, Volatility, Sectoral, etc.
}
```

#### UniverseBuildMetadata
```rust
pub struct UniverseBuildMetadata {
    pub csv_source: String,                       // "primary" | "fallback" | "cache"
    pub csv_row_count: usize,                     // Total raw CSV rows
    pub parsed_row_count: usize,                  // After segment filter
    pub index_count: usize,                       // Pass 1 result
    pub equity_count: usize,                      // Pass 2 result
    pub underlying_count: usize,                  // Pass 3-4 result
    pub derivative_count: usize,                  // Pass 5 result
    pub option_chain_count: usize,                // Pass 5 result
    pub build_duration: Duration,                 // Wall clock time
    pub build_timestamp: DateTime<FixedOffset>,   // IST timestamp
}
```

### 2.4 Subscription Types (instrument_registry.rs)

#### SubscriptionCategory — What kind of subscription?
```rust
pub enum SubscriptionCategory {
    MajorIndexValue,   // IDX_I feed for NIFTY, BANKNIFTY, etc. (5)
    DisplayIndex,      // IDX_I feed for INDIA VIX, sectoral, etc. (23)
    IndexDerivative,   // NSE_FNO/BSE_FNO contracts for major indices
    StockEquity,       // NSE_EQ price feed for F&O stocks (~206)
    StockDerivative,   // NSE_FNO contracts for stocks (current expiry, ATM±N)
}
```

#### SubscribedInstrument — One instrument ready for WebSocket
```rust
pub struct SubscribedInstrument {
    pub security_id: SecurityId,
    pub exchange_segment: ExchangeSegment,
    pub category: SubscriptionCategory,
    pub display_label: String,
    pub underlying_symbol: String,
    pub instrument_kind: Option<DhanInstrumentKind>,  // None for indices/equities
    pub expiry_date: Option<NaiveDate>,               // None for indices/equities
    pub strike_price: Option<f64>,                    // None for futures/indices/equities
    pub option_type: Option<OptionType>,              // None for futures/indices/equities
    pub feed_mode: FeedMode,
}
```

#### InstrumentRegistry — O(1) runtime lookup
```rust
pub struct InstrumentRegistry {
    instruments: HashMap<SecurityId, SubscribedInstrument>,  // O(1) lookup
    category_counts: HashMap<SubscriptionCategory, usize>,
    total_count: usize,
}
// Methods:
//   from_instruments(Vec<SubscribedInstrument>) → Self
//   get(SecurityId) → Option<&SubscribedInstrument>         O(1)
//   contains(SecurityId) → bool                             O(1)
//   len() → usize
//   category_count(SubscriptionCategory) → usize
//   iter() → impl Iterator<Item = &SubscribedInstrument>
//   by_exchange_segment() → HashMap<ExchangeSegment, Vec<SecurityId>>  (for subscription builder)
```

### 2.5 CSV Parsing Types (csv_parser.rs)

#### ParsedInstrumentRow — One row from CSV
```rust
pub struct ParsedInstrumentRow {
    pub exchange: Exchange,                 // NSE or BSE
    pub segment: char,                      // 'I' | 'E' | 'D'
    pub security_id: SecurityId,
    pub instrument: String,                 // "INDEX" | "EQUITY" | "FUTIDX" | "OPTIDX" | etc.
    pub underlying_security_id: SecurityId,
    pub underlying_symbol: String,
    pub symbol_name: String,
    pub display_name: String,
    pub series: String,                     // "EQ" for equities, "NA" for others
    pub lot_size: u32,
    pub expiry_date: Option<NaiveDate>,     // None for non-derivatives
    pub strike_price: f64,                  // 0.0 for futures, negative for N/A
    pub option_type: Option<OptionType>,
    pub tick_size: f64,
    pub expiry_flag: String,                // "M" (monthly) | "W" (weekly)
}
```

---

## 3. 5-PASS BUILD ALGORITHM

**File:** `crates/core/src/instrument/universe_builder.rs`

| Pass | Input | Output | What It Does |
|------|-------|--------|-------------|
| **1** | All rows with `segment='I'` | `index_lookup: HashMap<String, IndexEntry>` | Maps index symbol → (security_id, exchange). ~50 entries. |
| **2** | NSE rows with `segment='E'`, `series='EQ'` | `equity_lookup: HashMap<String, SecurityId>` | Maps stock symbol → NSE_EQ security_id. ~4000 entries. |
| **3** | FUTIDX + FUTSTK rows | `Vec<UnlinkedUnderlying>` | Discovers unique F&O underlyings from futures. Deduplicates by symbol. Skips TEST instruments. ~215 entries. |
| **4** | Pass 3 + Pass 1/2 lookups | `HashMap<String, FnoUnderlying>` | Links each underlying to its price feed. Index underlyings → IDX_I from Pass 1. Stock underlyings → NSE_EQ from Pass 2. Also uses `INDEX_SYMBOL_ALIASES` for mismatched names (NIFTYNXT50 → "NIFTY NEXT 50"). |
| **5** | All `segment='D'` rows + Pass 4 underlyings | `derivative_contracts`, `option_chains`, `expiry_calendars`, `instrument_info` | Builds every derivative contract, groups into option chains (calls/puts sorted by strike), builds expiry calendars, and populates the global instrument_info lookup. |

**Key functions:**
- `build_index_lookup(rows)` — Pass 1
- `build_equity_lookup(rows)` — Pass 2
- `discover_fno_underlyings(rows)` — Pass 3
- `link_underlyings(unlinked, index_lookup, equity_lookup)` — Pass 4
- Internal Pass 5 logic in `build_fno_universe_from_csv()`

**Index symbol aliasing (Pass 4):**
```
NIFTYNXT50 → "NIFTY NEXT 50" (IDX_I row name)
SENSEX50   → "SNSX50"        (IDX_I row name)
```
Constant: `INDEX_SYMBOL_ALIASES` in `constants.rs`

---

## 4. CSV DOWNLOAD & CACHING

**File:** `crates/core/src/instrument/csv_downloader.rs`

### Download Strategy (3-level fallback)
```
1. Primary URL (detailed CSV)  → retry with exponential backoff
2. Fallback URL (compact CSV)  → retry with exponential backoff
3. Local cache file            → read from disk
4. All fail                    → ApplicationError::InstrumentDownloadFailed
```

### Functions
| Function | Signature | What It Does |
|----------|-----------|-------------|
| `download_instrument_csv` | `async (primary_url, fallback_url, cache_dir, cache_filename, timeout) → Result<CsvDownloadResult>` | Full 3-level download with retry |
| `load_cached_csv` | `async (cache_dir, cache_filename) → Result<CsvDownloadResult>` | Cache-only load (no HTTP) |
| `download_with_retry` | `async (client, url) → Result<String>` (private) | Single URL with exponential backoff |
| `write_cache` | `async (cache_dir, filename, text)` (private) | Best-effort cache write |
| `read_cache` | `async (path) → Result<String>` (private) | Read + size validation |

### CsvDownloadResult
```rust
pub struct CsvDownloadResult {
    pub csv_text: String,      // Full CSV content
    pub source: String,        // "primary" | "fallback" | "cache"
}
```

### Retry config (from constants)
- Initial delay: 2000ms
- Max delay: 8000ms
- Max retries: 3
- Download timeout: 120s
- Minimum CSV size: 1,000,000 bytes

---

## 5. BINARY CACHE (rkyv)

**File:** `crates/core/src/instrument/binary_cache.rs`

### Cache File Format
```
[MAGIC: 4 bytes "RKYV"] [VERSION: 1 byte = 2] [PAD: 3 bytes] [rkyv payload ...]
```
- Total header: 8 bytes (aligned for rkyv)
- Filename: `fno-universe.rkyv`
- Atomic write: temp file → rename

### Functions
| Function | What It Does | Performance |
|----------|-------------|-------------|
| `write_binary_cache(universe, cache_dir)` | Serialize FnoUniverse to rkyv file | ~50ms |
| `read_binary_cache(cache_dir)` | Full deserialization to owned FnoUniverse | ~5-15ms |
| `MappedUniverse::load(cache_dir)` | Memory-mapped zero-copy access | **sub-0.5ms** |
| `MappedUniverse::archived()` | Zero-copy reference to ArchivedFnoUniverse | **sub-microsecond** |
| `MappedUniverse::to_owned()` | Full deserialization from mmap | ~5-15ms |
| `MappedUniverse::derivative_count()` | Count from archived, no alloc | O(1) |
| `MappedUniverse::underlying_count()` | Count from archived, no alloc | O(1) |

### Market-Hours Boot Path
```
MappedUniverse::load() → archived() → build_subscription_plan_from_archived()
```
Total: **sub-0.5ms** — zero HTTP, zero parsing, zero heap allocation for lookups.

---

## 6. SUBSCRIPTION PLANNER

**File:** `crates/core/src/instrument/subscription_planner.rs`

### Strategy (5 stages, in order)
1. **Major index values** (5) — IDX_I feeds for NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, SENSEX
2. **Display indices** (23) — IDX_I feeds for INDIA VIX, sectoral, broad market
3. **Index derivatives** (ALL) — Every futures + options contract for the 5 major indices, all expiries, all strikes. No filtering.
4. **Stock equities** (~206) — NSE_EQ price feed for each F&O stock
5. **Stock derivatives** (progressive fill) — Current expiry only, ATM ± N strikes, nearest expiry first, deterministic sort, up to 25K total capacity

### Capacity
- Max per connection: 5,000 instruments
- Max connections: 5
- **Total capacity: 25,000 instruments**
- Index derivatives (~15K) + stock equities (~206) + stock derivatives (fill remaining)

### Functions
| Function | Input | Output |
|----------|-------|--------|
| `build_subscription_plan(universe, config, today)` | Owned FnoUniverse | `SubscriptionPlan` |
| `build_subscription_plan_from_archived(archived, config, today)` | &ArchivedFnoUniverse | `SubscriptionPlan` |

Both produce the same `SubscriptionPlan { registry, summary }`.

---

## 7. DELTA DETECTION

**File:** `crates/core/src/instrument/delta_detector.rs`

### Function
```rust
pub fn detect_universe_delta(
    yesterday: Option<&FnoUniverse>,
    today: &FnoUniverse,
) -> Vec<LifecycleEvent>
```

### Events Detected
| Event | When | Fields Compared |
|-------|------|----------------|
| `ContractExpired` | In yesterday, not in today | security_id |
| `ContractAdded` | In today, not in yesterday | security_id |
| `LotSizeChanged` | Same security_id, different lot_size | lot_size |
| `TickSizeChanged` | Same security_id, different tick_size | tick_size |
| `StrikePriceChanged` | Same security_id, different strike_price | strike_price (I-P1-02) |
| `OptionTypeChanged` | Same security_id, different option_type | option_type (I-P1-02) |
| `SegmentChanged` | Same security_id, different exchange_segment | exchange_segment (I-P1-02) |
| `DisplayNameChanged` | Same security_id, different display_name | display_name (I-P1-02) |
| `SymbolNameChanged` | Same security_id, different symbol_name | symbol_name (I-P1-02) |
| `SecurityIdReuse` | security_id in both added and expired | Compound identity check (I-P1-03) |

### First Run
Returns empty `Vec` — all contracts treated as initial load.

---

## 8. VALIDATION

**File:** `crates/core/src/instrument/validation.rs`

### validate_fno_universe(universe) → Result<()>

5 checks, any failure = `ApplicationError::UniverseValidationFailed`:

| # | Check | What It Validates |
|---|-------|------------------|
| 1 | Must-exist indices | 8 indices (NIFTY=13, BANKNIFTY=25, ...) have correct price_feed_security_id |
| 2 | Must-exist equities | RELIANCE=2885 exists in instrument_info |
| 3 | Must-exist F&O stocks | RELIANCE, HDFCBANK, INFY, TCS present as underlyings |
| 4 | Stock count range | F&O stocks between 150 and 300 |
| 5 | Non-empty | At least 1 underlying and 1 derivative |

---

## 9. QUESTDB PERSISTENCE

**File:** `crates/storage/src/instrument_persistence.rs`

### 5 Tables

| Table | Rows/Day | DEDUP Key | What It Stores |
|-------|----------|-----------|---------------|
| `instrument_build_metadata` | 1 | `csv_source` | Build health and statistics |
| `fno_underlyings` | ~215 | `underlying_symbol` | Underlying reference data |
| `derivative_contracts` | ~150K | `security_id, underlying_symbol` | Every contract's metadata |
| `subscribed_indices` | 31 | `security_id` | 8 F&O + 23 display indices |
| `instrument_lifecycle` | variable | `security_id, event_type` | Delta events (added/expired/modified) |

### Key Design Decisions
- **No DELETE** — snapshots accumulate across days (SEBI audit trail, security_id reuse tracking)
- **DEDUP UPSERT** — same-day re-runs are idempotent
- **Timestamp convention** — midnight IST stored as UTC (e.g., `2026-02-25T00:00:00Z` = IST midnight Feb 25)
- **expiry_date** — stored as STRING `"YYYY-MM-DD"`, not TIMESTAMP
- **option_type** — empty string for futures (not NULL)
- **strike_price** — 0.0 for futures

### Grafana Query Pattern
```sql
-- Always filter by latest snapshot:
SELECT * FROM derivative_contracts
WHERE timestamp = (SELECT max(timestamp) FROM derivative_contracts)
```

---

## 10. CONFIGURATION REFERENCE

### config/base.toml

#### [instrument] section
| Key | Type | Default | What It Controls |
|-----|------|---------|-----------------|
| `daily_download_time` | string | `"08:15:00"` | IST time for daily CSV refresh |
| `csv_cache_directory` | string | `"/app/data/instrument-cache"` | Local cache directory |
| `csv_cache_filename` | string | `"api-scrip-master-detailed.csv"` | Cached CSV filename |
| `csv_download_timeout_secs` | u64 | `120` | Per-request HTTP timeout |
| `build_window_start` | string | `"09:00:00"` | Market hours start (no downloads) |
| `build_window_end` | string | `"15:30:00"` | Market hours end |

#### [subscription] section
| Key | Type | Default | What It Controls |
|-----|------|---------|-----------------|
| `feed_mode` | string | `"Full"` | WebSocket feed mode: "Ticker", "Quote", "Full" |
| `subscribe_index_derivatives` | bool | `true` | Subscribe to all index F&O contracts |
| `subscribe_stock_derivatives` | bool | `true` | Subscribe to stock F&O contracts |
| `subscribe_display_indices` | bool | `true` | Subscribe to 23 display indices |
| `subscribe_stock_equities` | bool | `true` | Subscribe to stock equity price feeds |
| `stock_atm_strikes_above` | usize | `10` | Stock options: strikes above ATM |
| `stock_atm_strikes_below` | usize | `10` | Stock options: strikes below ATM |
| `index_atm_strikes_above` | usize | `20` | Index options: strikes above ATM (only for Stage 2 fill) |
| `index_atm_strikes_below` | usize | `20` | Index options: strikes below ATM (only for Stage 2 fill) |
| `stock_default_atm_fallback_enabled` | bool | `true` | Use middle strike as ATM when no live price |

#### [dhan] section (instrument-related)
| Key | Value | What It Controls |
|-----|-------|-----------------|
| `instrument_csv_url` | `https://images.dhan.co/api-data/api-scrip-master-detailed.csv` | Primary CSV URL |
| `instrument_csv_fallback_url` | `https://images.dhan.co/api-data/api-scrip-master.csv` | Fallback CSV URL |

---

## 11. CONSTANTS REFERENCE

### CSV Column Names (from Dhan detailed CSV)
| Constant | Value | CSV Column |
|----------|-------|-----------|
| `CSV_COLUMN_EXCH_ID` | `"EXCH_ID"` | Exchange identifier |
| `CSV_COLUMN_SEGMENT` | `"SEGMENT"` | I/E/D |
| `CSV_COLUMN_SECURITY_ID` | `"SECURITY_ID"` | Dhan security ID |
| `CSV_COLUMN_INSTRUMENT` | `"INSTRUMENT"` | INDEX/EQUITY/FUTIDX/etc. |
| `CSV_COLUMN_UNDERLYING_SECURITY_ID` | `"UNDERLYING_SECURITY_ID"` | Underlying's security ID |
| `CSV_COLUMN_UNDERLYING_SYMBOL` | `"UNDERLYING_SYMBOL"` | e.g., "NIFTY" |
| `CSV_COLUMN_SYMBOL_NAME` | `"SYMBOL_NAME"` | e.g., "NIFTY-Mar2026-18000-CE" |
| `CSV_COLUMN_DISPLAY_NAME` | `"DISPLAY_NAME"` | Human-readable name |
| `CSV_COLUMN_SERIES` | `"SERIES"` | "EQ" for equities |
| `CSV_COLUMN_LOT_SIZE` | `"LOT_SIZE"` | Contract lot size |
| `CSV_COLUMN_EXPIRY_DATE` | `"SM_EXPIRY_DATE"` | Expiry date string |
| `CSV_COLUMN_STRIKE_PRICE` | `"STRIKE_PRICE"` | Strike price |
| `CSV_COLUMN_OPTION_TYPE` | `"OPTION_TYPE"` | "CE"/"PE" |
| `CSV_COLUMN_TICK_SIZE` | `"TICK_SIZE"` | Minimum price movement |
| `CSV_COLUMN_EXPIRY_FLAG` | `"EXPIRY_FLAG"` | "M" (monthly) / "W" (weekly) |

### CSV Filter Values
| Constant | Value | Purpose |
|----------|-------|---------|
| `CSV_SEGMENT_INDEX` | `"I"` | Index rows |
| `CSV_SEGMENT_EQUITY` | `"E"` | Equity rows |
| `CSV_SEGMENT_DERIVATIVE` | `"D"` | Derivative rows |
| `CSV_EXCHANGE_NSE` | `"NSE"` | NSE exchange |
| `CSV_EXCHANGE_BSE` | `"BSE"` | BSE exchange |
| `CSV_SERIES_EQUITY` | `"EQ"` | Equity series filter |
| `CSV_INSTRUMENT_FUTIDX` | `"FUTIDX"` | Future on index |
| `CSV_INSTRUMENT_FUTSTK` | `"FUTSTK"` | Future on stock |
| `CSV_INSTRUMENT_OPTIDX` | `"OPTIDX"` | Option on index |
| `CSV_INSTRUMENT_OPTSTK` | `"OPTSTK"` | Option on stock |
| `CSV_OPTION_TYPE_CALL` | `"CE"` | Call European |
| `CSV_OPTION_TYPE_PUT` | `"PE"` | Put European |
| `CSV_TEST_SYMBOL_MARKER` | `"TEST"` | Skip test instruments |

### Index Constants
| Constant | Value | Purpose |
|----------|-------|---------|
| `FULL_CHAIN_INDEX_SYMBOLS` | `["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY", "SENSEX"]` | Get full option chain |
| `FULL_CHAIN_INDEX_COUNT` | `5` | Compile-time assertion |
| `DISPLAY_INDEX_COUNT` | `23` | Display-only indices |
| `TOTAL_SUBSCRIBED_INDEX_COUNT` | `31` | 8 F&O + 23 display |

### Validation Constants
| Constant | Value | Purpose |
|----------|-------|---------|
| `VALIDATION_MUST_EXIST_INDICES` | 8 entries (NIFTY=13, BANKNIFTY=25, ...) | Post-build index check |
| `VALIDATION_MUST_EXIST_EQUITIES` | `[("RELIANCE", 2885)]` | Post-build equity check |
| `VALIDATION_MUST_EXIST_FNO_STOCKS` | `["RELIANCE", "HDFCBANK", "INFY", "TCS"]` | Post-build stock check |
| `VALIDATION_FNO_STOCK_MIN_COUNT` | `150` | Minimum F&O stocks |
| `VALIDATION_FNO_STOCK_MAX_COUNT` | `300` | Maximum F&O stocks |

### WebSocket Limits
| Constant | Value |
|----------|-------|
| `MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION` | `5,000` |
| `MAX_WEBSOCKET_CONNECTIONS` | `5` |
| `MAX_TOTAL_SUBSCRIPTIONS` | `25,000` |

### Cache Constants
| Constant | Value |
|----------|-------|
| `BINARY_CACHE_FILENAME` | `"fno-universe.rkyv"` |
| `INSTRUMENT_FRESHNESS_MARKER_FILENAME` | `"instrument-build-date.txt"` |
| `INSTRUMENT_PERSIST_MARKER_FILENAME` | `"instrument-persist-date.txt"` |
| `INSTRUMENT_CSV_MIN_BYTES` | `1,000,000` |
| `INSTRUMENT_CSV_MIN_ROWS` | `100,000` |
| `INSTRUMENT_CSV_DOWNLOAD_TIMEOUT_SECS` | `120` |
| `INSTRUMENT_CSV_MAX_DOWNLOAD_RETRIES` | `3` |
| `INSTRUMENT_CSV_RETRY_INITIAL_DELAY_MS` | `2,000` |
| `INSTRUMENT_CSV_RETRY_MAX_DELAY_MS` | `8,000` |

---

## 12. DATA FLOW END-TO-END

### Outside Market Hours (Full Build)
```
config/base.toml
  ├─ [dhan].instrument_csv_url
  └─ [instrument].*
       │
       ▼
csv_downloader::download_instrument_csv()
  ├─ Primary URL → retry 3x with exponential backoff
  ├─ Fallback URL → retry 3x
  └─ Cache file → read from disk
       │
       ▼ CsvDownloadResult { csv_text, source }
       │
csv_parser::parse_instrument_csv(csv_text, config)
  ├─ Find column indices from header row
  ├─ Filter: only (NSE,I), (NSE,E), (NSE,D), (BSE,I), (BSE,D) rows
  └─ Parse each row → ParsedInstrumentRow
       │
       ▼ Vec<ParsedInstrumentRow>
       │
universe_builder::build_fno_universe_from_csv(csv_text, config)
  ├─ Pass 1: build_index_lookup()     → HashMap<symbol, IndexEntry>
  ├─ Pass 2: build_equity_lookup()    → HashMap<symbol, SecurityId>
  ├─ Pass 3: discover_fno_underlyings() → Vec<UnlinkedUnderlying>
  ├─ Pass 4: link_underlyings()       → HashMap<symbol, FnoUnderlying>
  └─ Pass 5: build contracts, chains, calendars, instrument_info
       │
       ▼ FnoUniverse
       │
validation::validate_fno_universe()  → error if any check fails
       │
       ▼ (validated)
       │
  ┌────┼────────────────────┐
  │    │                    │
  ▼    ▼                    ▼
binary_cache     delta_detector     instrument_persistence
::write()        ::detect()         ::persist_snapshot()
  │                │                    │
  ▼                ▼                    ▼
fno-universe   LifecycleEvents     QuestDB 5 tables
.rkyv          (to QuestDB)
  │
  ▼
subscription_planner::build_subscription_plan()
  │
  ▼
SubscriptionPlan { registry, summary }
  │
  ▼
WebSocket subscription_builder → JSON subscribe messages
```

### Market Hours (Cache-Only Boot)
```
binary_cache::MappedUniverse::load(cache_dir)  — sub-0.5ms
  │
  ▼ &ArchivedFnoUniverse
  │
subscription_planner::build_subscription_plan_from_archived()
  │
  ▼
SubscriptionPlan { registry, summary }
  │
  ▼
WebSocket subscription_builder → JSON subscribe messages
```

### Runtime (Tick Enrichment)
```
WebSocket binary frame
  │
  ▼
parser extracts SecurityId from header bytes 4-7
  │
  ▼
InstrumentRegistry::get(security_id)  — O(1) HashMap lookup
  │
  ▼
SubscribedInstrument { category, underlying_symbol, ... }
  │
  ▼
Tick enrichment for logging, storage, broadcast
```

---

## 13. TEST COVERAGE MAP

### crates/core/src/instrument/ (unit tests in each file)

| Module | Test Count | Key Tests |
|--------|-----------|-----------|
| `csv_downloader.rs` | 21 | retry success/failure, fallback cascade, cache read/write, boundary sizes, HTTP errors |
| `csv_parser.rs` | ~15 | column detection, row parsing, segment filtering, edge cases |
| `universe_builder.rs` | ~20 | 5-pass correctness, duplicate security_id rejection (I-P0-01), empty CSV, TEST symbol skip |
| `validation.rs` | ~10 | all 5 checks, boundary counts, missing indices/equities/stocks |
| `binary_cache.rs` | 13 | roundtrip, corruption, wrong magic/version, MappedUniverse latency |
| `subscription_planner.rs` | ~12 | capacity limits, feed modes, progressive fill, archived path |
| `delta_detector.rs` | ~15 | all event types, full field coverage (I-P1-02), security_id reuse (I-P1-03) |
| `daily_scheduler.rs` | ~10 | compute_next_trigger_time, parse_daily_download_time, edge cases |
| `s3_backup.rs` | ~8 | config validation, key layout, error types |
| `instrument_loader.rs` | ~12 | market hours detection, freshness marker, cache persistence (I-P0-04) |
| `diagnostic.rs` | 0 | TEST-EXEMPT: requires live HTTP endpoints |

### crates/storage/src/instrument_persistence.rs
~15 tests: snapshot epoch calculation, cross-day accumulation, DEDUP key verification, no-DELETE enforcement

### Integration tests (crates/core/tests/, crates/trading/tests/)
- `gap_enforcement.rs` — mechanical gap tests
- `schema_validation.rs` — type invariant tests

---

## 14. TROUBLESHOOTING QUICK REFERENCE

| Symptom | Likely Cause | Fix |
|---------|-------------|-----|
| "all sources failed" at startup | CSV URLs unreachable + no cache | Check network, verify URLs in config |
| "universe validation failed: must-exist index" | Dhan changed security ID for a major index | Update `VALIDATION_MUST_EXIST_INDICES` in constants |
| "rkyv cache bad magic" | Cache file corrupted | Delete `fno-universe.rkyv`, restart outside market hours |
| "rkyv cache version mismatch" | Code updated rkyv types | Delete cache, restart for fresh build |
| "CSV body too small" | Dhan serving empty/truncated CSV | Wait and retry, or use cached CSV |
| 0 instruments subscribed | build_window_start/end misconfigured | Check `[instrument]` section in config |
| Stale instruments (expired contracts) | No daily refresh | Enable daily_scheduler or restart daily |
| Wrong instrument in tick | Duplicate security_id (I-P0-01) | Universe builder rejects duplicates — check logs |
| Missing stock derivatives | No current expiry chain | Check expiry_calendars for that stock |
| "cache directory is on tmpfs" | EBS not mounted | Ensure `/app/data/` is on persistent EBS volume |

---

## APPENDIX: rkyv SERIALIZATION HELPERS

All major types derive `rkyv::Archive, rkyv::Serialize, rkyv::Deserialize` for zero-copy access.

Custom `ArchiveWith` helpers in `instrument_types.rs`:
- `NaiveDateAsI32` — NaiveDate as days-from-CE in i32_le
- `DateTimeFixedOffsetAsI64` — DateTime<FixedOffset> as (timestamp_secs, offset_secs)
- `DurationAsMillis` — Duration as milliseconds in u64_le

Helper: `naive_date_from_archived_i32(days: &rkyv::rend::i32_le) → NaiveDate`
