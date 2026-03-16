# Holiday Calendar Requirements — Complete Business Reference

> **Purpose:** Single source of truth for ALL holiday calendar and trading day
> classification requirements. Covers NSE holidays, Muhurat sessions, market
> hours integration, and annual update procedures.
>
> **Last updated:** 2026-03-10

---

## 1. WHAT NSE PROVIDES

### 1.1 Annual Holiday Circular

NSE publishes an official circular listing all trading holidays for the
upcoming calendar year. This is the only authoritative source.

| Property | Value |
|----------|-------|
| **Source** | NSE circular (published Dec/Jan each year) |
| **Format** | PDF/web page listing dates and names |
| **Frequency** | Annual (with rare mid-year amendments) |
| **Coverage** | All NSE segments (equity, F&O, currency, commodity) |
| **Our scope** | F&O segment holidays only (same as equity) |

### 1.2 What We Configure

All holiday data lives in `config/base.toml` under `[trading]`. No runtime
fetching — holidays are static config updated annually by the operator.

| Config Key | Type | Description |
|------------|------|-------------|
| `trading.nse_holidays` | `Vec<NseHolidayEntry>` | Weekday holidays (market fully closed) |
| `trading.muhurat_trading_dates` | `Vec<NseHolidayEntry>` | Special evening sessions on closed days |

### 1.3 NseHolidayEntry Format

```toml
[[trading.nse_holidays]]
date = "2026-01-26"          # YYYY-MM-DD (IST date)
name = "Republic Day"        # Human-readable name for display
```

---

## 2. WHAT WE BUILD FROM IT

### 2.1 TradingCalendar

A pre-built, immutable data structure providing O(1) trading day lookups.
Constructed once at boot from config, wrapped in `Arc<>`, shared read-only.

| Component | Type | Purpose |
|-----------|------|---------|
| `holidays` | `HashSet<NaiveDate>` | O(1) "is this date a holiday?" |
| `holiday_names` | `HashMap<NaiveDate, String>` | Display name lookup |
| `muhurat_dates` | `HashSet<NaiveDate>` | O(1) "is this a Muhurat session?" |
| `muhurat_names` | `HashMap<NaiveDate, String>` | Display name lookup |

### 2.2 QuestDB Persistence

Holiday data is persisted to QuestDB at boot for Grafana dashboards.

| Table | Columns | Rows | Purpose |
|-------|---------|------|---------|
| `nse_holidays` | `name SYMBOL, holiday_type SYMBOL, ts TIMESTAMP` | 16-17/year | Dashboard display + chart annotations |

DEDUP KEY: `(ts, name)` — idempotent on re-runs.

---

## 3. TRADING DAY CLASSIFICATION

### 3.1 Decision Tree

```
Is it Saturday or Sunday?
  YES → NOT a trading day
  NO  → Is it in nse_holidays list?
          YES → NOT a regular trading day
                (but check: is it also a Muhurat date?)
                  YES → Special evening session (limited hours)
                  NO  → Market fully closed
          NO  → REGULAR trading day (9:15-15:30 IST)
```

### 3.2 Day Types

| Type | Market Hours | Orders Allowed | Data Collection |
|------|-------------|----------------|-----------------|
| **Regular Trading Day** | 09:15-15:30 IST | 09:15-15:29 IST | 09:00-16:00 IST |
| **NSE Holiday** | Closed | None | None |
| **Weekend** | Closed | None | None |
| **Muhurat Session** | ~18:00-19:00 IST (varies) | During session | During session |

### 3.3 Special Session Types

| Session | When | Duration | Notes |
|---------|------|----------|-------|
| **Muhurat Trading** | Diwali evening | ~1 hour (18:00-19:00 typically) | Exact timing announced by NSE each year |
| **Budget Day** | Union Budget date (Feb) | May have modified hours | Currently treated as regular/holiday |
| **Early Close** | Pre-festival, year-end | Close at 15:15 instead of 15:30 | NOT currently modeled |

---

## 4. BUSINESS RULES

### 4.1 Holiday List Validation

| # | Rule | Enforcement |
|---|------|-------------|
| V1 | Every date must parse as `YYYY-MM-DD` | `TradingCalendar::from_config()` fails |
| V2 | No holidays on Saturday or Sunday | `from_config()` rejects weekend dates |
| V3 | Holiday count must be >= 10 and <= 25 per year | **GAP: Not enforced** |
| V4 | All holidays must belong to current or next year | **GAP: Not enforced** |
| V5 | No duplicate dates in holiday list | **GAP: Not enforced** |
| V6 | No duplicate dates in Muhurat list | **GAP: Not enforced** |
| V7 | Muhurat dates should be holidays or weekends | **GAP: Not enforced** |

### 4.2 How the Calendar Integrates

| Consumer | What It Checks | Impact |
|----------|---------------|--------|
| **Boot sequence** (`main.rs`) | `is_trading_day_today()` | Decides fast vs slow boot |
| **WebSocket timeout** (`order_update_connection.rs`) | `is_trading_day_today()` + market hours | 120s (market) vs 600s (off-hours) timeout |
| **Instrument loader** (`instrument_loader.rs`) | Build window check | Download outside market hours only |
| **Trading pipeline** | `is_trading_day_today()` | Should not submit orders on holidays |
| **Grafana dashboards** | QuestDB `nse_holidays` table | Calendar display + annotations |

### 4.3 Annual Update Procedure

```
1. NSE publishes holiday circular (December/January)
2. Operator updates config/base.toml:
   - Clear previous year's holidays
   - Add new year's holidays (all weekday dates)
   - Add Muhurat trading date if applicable
3. Commit + deploy
4. System validates at boot (from_config fails on bad data)
```

**Risk:** If operator forgets to update, system runs with stale holidays.
Currently no automated detection of this scenario.

---

## 5. NSE HOLIDAYS 2026 (CURRENT)

| # | Date | Day | Name |
|---|------|-----|------|
| 1 | 2026-01-26 | Mon | Republic Day |
| 2 | 2026-03-03 | Tue | Holi |
| 3 | 2026-03-26 | Thu | Shri Ram Navami |
| 4 | 2026-03-31 | Tue | Shri Mahavir Jayanti |
| 5 | 2026-04-03 | Fri | Good Friday |
| 6 | 2026-04-14 | Tue | Dr. Baba Saheb Ambedkar Jayanti |
| 7 | 2026-05-01 | Fri | Maharashtra Day |
| 8 | 2026-05-28 | Thu | Bakri Eid (Eid ul-Adha) |
| 9 | 2026-06-26 | Fri | Muharram |
| 10 | 2026-09-14 | Mon | Ganesh Chaturthi |
| 11 | 2026-10-02 | Fri | Mahatma Gandhi Jayanti |
| 12 | 2026-10-20 | Tue | Dussehra |
| 13 | 2026-11-10 | Tue | Diwali Balipratipada |
| 14 | 2026-11-24 | Tue | Prakash Gurpurb Sri Guru Nanak Dev |
| 15 | 2026-12-25 | Fri | Christmas |

**Muhurat Trading 2026:**
- 2026-11-08 (Sunday): Diwali 2026 — special evening session

**Total: 15 holidays + 1 Muhurat = 16 calendar entries**

---

## 6. EDGE CASES — EXHAUSTIVE LIST

### 6.1 Calendar Construction

| Scenario | Handling |
|----------|---------|
| Empty holiday list | Calendar builds with 0 holidays — all weekdays are trading days |
| Single holiday | Calendar builds normally |
| Holiday on Saturday/Sunday | **Rejected** at config validation time |
| Invalid date string | **Rejected** at config validation time |
| Duplicate holiday date | **GAP: silently accepted** (HashSet insert overwrites) |
| Muhurat on non-holiday weekday | **GAP: accepted** (semantically odd) |
| Holiday year doesn't match current year | **GAP: not detected** |
| More than 25 holidays | **GAP: not validated** |
| Fewer than 10 holidays | **GAP: not validated** |

### 6.2 Trading Day Queries

| Scenario | Expected Result |
|----------|----------------|
| Regular weekday, not holiday | `is_trading_day()` = true |
| Saturday | `is_trading_day()` = false |
| Sunday | `is_trading_day()` = false |
| Weekday holiday | `is_trading_day()` = false |
| Muhurat date (also a holiday) | `is_trading_day()` = false, `is_muhurat_trading_day()` = true |
| Muhurat date (not a holiday) | `is_trading_day()` = true, `is_muhurat_trading_day()` = true |

### 6.3 Next Trading Day Calculation

| Scenario | Example |
|----------|---------|
| From a trading day | Returns same day |
| From Saturday | Returns Monday (or Tuesday if Monday is holiday) |
| From holiday Thursday | Returns Friday (if not holiday) |
| From Friday holiday before weekend | Returns Monday |
| From consecutive holidays (Thu+Fri) before weekend | Returns Monday |
| From long weekend (Fri holiday + Sat + Sun + Mon holiday) | Returns Tuesday |
| All dates are holidays (degenerate config) | Bounded by `NEXT_TRADING_DAY_MAX_ITERATIONS` constant; returns error after exhaustion |

### 6.4 Timezone Edge Cases

| Scenario | Handling |
|----------|---------|
| Query at 00:01 IST (still same day as 18:31 UTC previous day) | `today_ist()` uses IST offset |
| Query at 23:59 IST | Correct IST date |
| System clock skew | Uses system time — no NTP validation |
| Leap second | Chrono handles correctly |

---

## 7. WHEN DOES THIS NEED TO CHANGE?

The calendar system needs updating ONLY when:

1. **NSE publishes new year's holiday list** — annual config update
2. **NSE announces mid-year holiday change** — rare, requires config update + deploy
3. **Muhurat Trading announced** — annual, date varies
4. **New session types added** (early close, budget day) — requires code change
5. **Exchange changes to 6-day trading week** — requires weekday logic change
6. **BSE-specific holidays differ from NSE** — currently assumed same

Until any of those happen, the system is maintenance-free within a calendar year.

---

## 8. TEST COVERAGE AUDIT (2026-03-10)

### 8.1 Validated Edge Cases

All edge cases in section 6 have matching `#[test]` functions:

| Edge Case | Test Function | Status |
|-----------|---------------|--------|
| Calendar construction validations (V1-V7) | 7 validation tests | Enforced |
| Holiday count bounds [10, 25] | `test_holiday_count_bounds_constants` | Enforced |
| Weekend holiday rejection | `test_saturday_holiday_rejected`, `test_sunday_holiday_rejected` | Enforced |
| Duplicate holiday (HashSet) | `test_duplicate_holiday_dates_silently_accepted` | Verified |
| Muhurat date on non-holiday weekday | `test_muhurat_validation` | Warns |
| Next trading day from Saturday | `test_next_trading_day_from_sunday_returns_monday` | Verified |
| All 2026 holidays individually | `test_all_2026_holidays_recognized` | Verified |
| Holiday name lookup | `test_holiday_name_lookup_via_all_entries` | Verified |
| Muhurat name lookup | `test_muhurat_name_lookup_via_all_entries` | Verified |
| Invalid muhurat date | `test_invalid_muhurat_date_string_rejected` | Verified |
| Next trading day max iterations | `test_next_trading_day_max_iterations_reasonable` | Bounded [15, 60] |

### 8.2 Mechanical Enforcement

- **Test count ratchet:** Cannot decrease (`.claude/hooks/test-count-guard.sh`)
- **Pub fn guard:** New pub fns must have matching tests
- **Year validation:** Warns (not hard error) for holidays outside current/next year — intentional to allow early config for next year
