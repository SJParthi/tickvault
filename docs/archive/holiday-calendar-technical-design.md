# Holiday Calendar Technical Design — Complete Architecture Reference

> **Purpose:** Single source of truth for ALL holiday calendar technical
> decisions, architecture, data structures, and test strategy.
>
> **Last updated:** 2026-03-10

---

## 1. SYSTEM ARCHITECTURE

### 1.1 Data Flow

```
config/base.toml
    │
    ▼
ApplicationConfig::validate()
    │ (TradingCalendar::from_config — validates dates + weekends)
    ▼
TradingCalendar (Arc<> wrapped, immutable)
    │
    ├──► is_trading_day_today() ──► Boot path (fast/slow decision)
    ├──► is_trading_day_today() ──► WebSocket timeout selection
    ├──► is_trading_day_today() ──► Order submission guard
    ├──► all_entries() ──► QuestDB persistence (nse_holidays table)
    └──► is_trading_day() ──► Scheduling (next_trading_day)
```

### 1.2 Boot Sequence Position

```
CryptoProvider → Config → ★ TradingCalendar ★ → Observability → Logging
→ Notification → IP Verification → Auth → QuestDB → Universe → WebSocket ...
```

Calendar is built BEFORE auth — it's used to decide boot strategy.

### 1.3 Crate Ownership

| Crate | What it owns |
|-------|-------------|
| `common` | `TradingCalendar`, `HolidayInfo`, `TradingConfig`, `NseHolidayEntry` |
| `storage` | `calendar_persistence.rs` (QuestDB writer) |
| `app` | Boot sequence integration, calendar→pipeline wiring |

### 1.4 Key Files

| File | Purpose |
|------|---------|
| `crates/common/src/trading_calendar.rs` | Core calendar logic, O(1) lookups |
| `crates/common/src/config.rs` | `TradingConfig`, `NseHolidayEntry`, validation |
| `crates/storage/src/calendar_persistence.rs` | QuestDB persistence |
| `crates/app/src/main.rs` | Boot integration |
| `crates/core/src/websocket/order_update_connection.rs` | Market hours check |
| `config/base.toml` | Holiday configuration data |

---

## 2. DATA STRUCTURES

### 2.1 TradingCalendar

```rust
pub struct TradingCalendar {
    holidays: HashSet<NaiveDate>,              // O(1) lookup
    holiday_names: HashMap<NaiveDate, String>,  // display names
    muhurat_dates: HashSet<NaiveDate>,          // O(1) lookup
    muhurat_names: HashMap<NaiveDate, String>,  // display names
}
```

**Key properties:**
- Immutable after construction
- Wrapped in `Arc<>` for thread-safe sharing
- Zero allocation on reads (hot path)
- All lookups O(1) via HashSet/HashMap

### 2.2 HolidayInfo

```rust
pub struct HolidayInfo {
    pub date: NaiveDate,
    pub name: String,
    pub is_muhurat: bool,
}
```

Used for persistence and display. Sorted by date in `all_entries()`.

### 2.3 NseHolidayEntry (Config)

```rust
pub struct NseHolidayEntry {
    pub date: String,   // "YYYY-MM-DD"
    pub name: String,   // "Republic Day"
}
```

Deserialized from TOML. Validated during `TradingCalendar::from_config()`.

---

## 3. QuestDB SCHEMA

### 3.1 nse_holidays Table

```sql
CREATE TABLE IF NOT EXISTS nse_holidays (
    name SYMBOL,
    holiday_type SYMBOL,
    ts TIMESTAMP
) TIMESTAMP(ts) PARTITION BY YEAR WAL
```

DEDUP KEY: `(ts, name)`

### 3.2 ILP Write Format

```
nse_holidays,name=Republic\ Day,holiday_type=Holiday 1737849600000000000
nse_holidays,name=Diwali\ 2026,holiday_type=Muhurat\ Trading 1731024000000000000
```

Timestamp: IST midnight as epoch nanoseconds (IST-as-UTC convention).

---

## 4. VALIDATION RULES

### 4.1 Current Validations (Implemented)

| # | Check | Location |
|---|-------|----------|
| 1 | Date parses as YYYY-MM-DD | `from_config()` |
| 2 | No holidays on Saturday/Sunday | `from_config()` |

### 4.2 Missing Validations (To Implement)

| # | Check | Rule | Error |
|---|-------|------|-------|
| 3 | Minimum holiday count | `holiday_count >= 10` | Hard error — incomplete calendar |
| 4 | Maximum holiday count | `holiday_count <= 25` | Hard error — likely config error |
| 5 | Year validation | All dates in current or next year | Warn — possibly stale config |
| 6 | No duplicate holiday dates | `HashSet::insert` returns false | Hard error — config has dupes |
| 7 | No duplicate Muhurat dates | Same check | Hard error |
| 8 | Muhurat on non-trading day | Muhurat dates should be holidays/weekends | Warn |

---

## 5. TEST STRATEGY

### 5.1 Current Tests (14)

| Test | Module | Coverage |
|------|--------|----------|
| from_config builds calendar | trading_calendar | Construction |
| weekday non-holiday is trading | trading_calendar | Positive case |
| holiday is not trading | trading_calendar | Negative case |
| Saturday not trading | trading_calendar | Weekend |
| Sunday not trading | trading_calendar | Weekend |
| Muhurat detected | trading_calendar | Special session |
| is_holiday | trading_calendar | Lookup |
| next_trading_day skips weekend | trading_calendar | Scheduling |
| next_trading_day skips holiday | trading_calendar | Scheduling |
| weekend holiday rejected | trading_calendar | Validation |
| invalid date rejected | trading_calendar | Validation |
| empty holidays = all weekdays | trading_calendar | Edge case |
| holidays loaded | trading_calendar | Config loading |
| all_entries sorted | trading_calendar | Persistence |

### 5.2 Gap Tests to Add (16 gaps)

**CRITICAL (C1-C5):**

| ID | Test | What It Validates |
|----|------|-------------------|
| C1 | `test_year_boundary_validation` | Holidays must be in current/next year |
| C2 | `test_minimum_holiday_count_validation` | Reject config with < 10 holidays |
| C3 | `test_duplicate_holiday_date_rejected` | Duplicate dates detected and rejected |
| C4 | `test_calendar_persistence_roundtrip` | QuestDB write produces correct ILP |
| C5 | `test_next_trading_day_long_weekend` | Holiday+weekend combo (4+ day gap) |

**HIGH (H1-H5):**

| ID | Test | What It Validates |
|----|------|-------------------|
| H1 | `test_muhurat_session_on_holiday` | Muhurat flag correct when date is also holiday |
| H2 | `test_all_entries_dedup_holiday_and_muhurat` | Same date as both produces 2 entries |
| H3 | `test_holiday_order_submission_guard` | is_trading_day checked before order flow |
| H4 | `test_early_close_day_handling` | Future-proofing: early close type |
| H5 | `test_today_ist_timezone_boundary` | IST date correct near midnight |

**MEDIUM (M1-M6):**

| ID | Test | What It Validates |
|----|------|-------------------|
| M1 | `test_consecutive_weekday_holidays` | Thu+Fri holidays → Monday is next |
| M2 | `test_max_gap_holiday_weekend_combo` | 5+ consecutive non-trading days |
| M3 | `test_muhurat_on_regular_weekday` | Muhurat date that's NOT a holiday |
| M4 | `test_year_end_boundary` | Dec 31 → Jan 1 transition |
| M5 | `test_maximum_holiday_count_validation` | Reject config with > 25 holidays |
| M6 | `test_next_trading_day_bounded_iterations` | Loop terminates within reasonable bound |

---

## 6. PERFORMANCE

### 6.1 Construction (Cold Path)

| Operation | Time | Notes |
|-----------|------|-------|
| Parse + validate dates | <1ms | 16 entries |
| Build HashSets | <1ms | 16 insertions |
| QuestDB persistence | ~100ms | 16 ILP rows |

### 6.2 Runtime Lookups (Hot Path)

| Operation | Time | Allocation |
|-----------|------|-----------|
| `is_trading_day()` | O(1) | Zero |
| `is_muhurat_trading_day()` | O(1) | Zero |
| `is_holiday()` | O(1) | Zero |
| `next_trading_day()` | O(k) where k=gap length | Zero |

---

## 7. CONFIGURATION

```toml
[trading]
market_open_time = "09:00:00"
market_close_time = "15:30:00"
order_cutoff_time = "15:29:00"
data_collection_start = "09:00:00"
data_collection_end = "16:00:00"
timezone = "Asia/Kolkata"
max_orders_per_second = 10

[[trading.nse_holidays]]
date = "2026-01-26"
name = "Republic Day"

# ... (15 total for 2026)

[[trading.muhurat_trading_dates]]
date = "2026-11-08"
name = "Diwali 2026 (Sunday — special session)"
```
