# Dhan V2 Instrument List / Master — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/instruments/
> **Extracted**: 2026-03-13
> **Purpose**: Standalone reference for Claude Code — load alongside relevant API docs
> **Related**: 08-annexure-enums.md (for ExchangeSegment, Instrument Type enums)

---

## 1. Instrument Master CSV URLs

**Compact** (smaller, essential fields):
```
https://images.dhan.co/api-data/api-scrip-master.csv
```

**Detailed** (all fields including ISIN, ASM/GSM flags, MTF leverage):
```
https://images.dhan.co/api-data/api-scrip-master-detailed.csv
```

**Per-segment** (fetch only one exchange segment at a time):
```
curl --location 'https://api.dhan.co/v2/instrument/{exchangeSegment}'
```
Where `{exchangeSegment}` is the string enum like `NSE_EQ`, `NSE_FNO`, etc. (see 08-annexure-enums.md Section 1).

> **CRITICAL**: The instrument master CSV is the **sole source of truth** for `SecurityId` values. Never hardcode SecurityIds — they can change when instruments are relisted or new series are issued. Download fresh daily.

---

## 2. Column Description

| Detailed Tag                | Compact Tag                | Description                                                       |
|-----------------------------|----------------------------|-------------------------------------------------------------------|
| `EXCH_ID`                   | `SEM_EXM_EXCH_ID`         | Exchange: `NSE`, `BSE`, `MCX`                                     |
| `SEGMENT`                   | `SEM_SEGMENT`              | `C`=Currency, `D`=Derivatives, `E`=Equity, `I`=Index, `M`=Commodity |
| `SECURITY_ID`               | `SEM_SMST_SECURITY_ID`     | SecurityId — primary key for all Dhan API calls                   |
| `ISIN`                      | —                          | 12-digit ISIN (International Securities Identification Number)    |
| `INSTRUMENT`                | `SEM_INSTRUMENT_NAME`      | Instrument type as defined by exchange (see 08-annexure Section 6)|
| —                           | `SEM_EXPIRY_CODE`          | Expiry code for futures (see 08-annexure Section 7)               |
| `UNDERLYING_SECURITY_ID`    | —                          | SecurityId of underlying (derivatives only)                       |
| `UNDERLYING_SYMBOL`         | —                          | Symbol of underlying (derivatives only)                           |
| `SYMBOL_NAME`               | `SM_SYMBOL_NAME`           | Symbol name of instrument                                         |
| —                           | `SEM_TRADING_SYMBOL`       | Exchange trading symbol                                           |
| `DISPLAY_NAME`              | `SEM_CUSTOM_SYMBOL`        | Dhan display symbol name                                          |
| `INSTRUMENT_TYPE`           | `SEM_EXCH_INSTRUMENT_TYPE` | More detailed instrument type from exchange                       |
| `SERIES`                    | `SEM_SERIES`               | Exchange-defined series                                           |
| `LOT_SIZE`                  | `SEM_LOT_UNITS`            | Lot size (trading multiple)                                       |
| `SM_EXPIRY_DATE`            | `SEM_EXPIRY_DATE`          | Expiry date (derivatives only)                                    |
| `STRIKE_PRICE`              | `SEM_STRIKE_PRICE`         | Strike price (options only)                                       |
| `OPTION_TYPE`               | `SEM_OPTION_TYPE`          | `CE`=Call, `PE`=Put                                               |
| `TICK_SIZE`                 | `SEM_TICK_SIZE`            | Minimum price increment                                           |
| `EXPIRY_FLAG`               | `SEM_EXPIRY_FLAG`          | `M`=Monthly, `W`=Weekly expiry                                   |
| `ASM_GSM_FLAG`              | —                          | `N`=Not ASM/GSM, `R`=Removed, `Y`=ASM/GSM                       |
| `ASM_GSM_CATEGORY`          | —                          | Surveillance category (`NA` if none)                              |
| `BUY_SELL_INDICATOR`        | —                          | `A` if both Buy/Sell allowed                                     |
| `MTF_LEVERAGE`              | —                          | MTF leverage multiplier (EQUITY only)                             |

---

## 3. Key Fields for Live Market Feed

When subscribing to instruments via WebSocket, you need:

1. **`SecurityId`** — lookup in CSV by symbol/ISIN. This is the `int32` in binary response header bytes 5-8.
2. **`ExchangeSegment`** — the string enum (`NSE_EQ`, `NSE_FNO`, etc.) for JSON subscribe. Maps to numeric enum in binary header byte 4.
3. **`LOT_SIZE`** — needed to interpret `Last Traded Quantity` correctly for derivatives.

---

## 4. Key Fields for Order Placement

1. **`SecurityId`** — required for all order APIs
2. **`ExchangeSegment`** — determines which exchange/segment the order goes to
3. **`LOT_SIZE`** — derivatives must be ordered in multiples of lot size
4. **`TICK_SIZE`** — price must be in multiples of tick size
5. **`OPTION_TYPE`** — CE/PE for options orders
6. **`EXPIRY_FLAG`** / **`SM_EXPIRY_DATE`** — to identify the correct contract

---

## 5. Rust Struct

```rust
/// Represents a single row from the instrument master CSV
#[derive(Debug, Clone)]
pub struct InstrumentMaster {
    pub exchange: String,              // NSE, BSE, MCX
    pub segment: char,                 // C, D, E, I, M
    pub security_id: u32,              // Primary key for all API calls
    pub isin: Option<String>,          // Only in detailed CSV
    pub instrument: String,            // EQUITY, FUTIDX, OPTIDX, etc.
    pub symbol_name: String,           // e.g., "HDFCBANK"
    pub trading_symbol: String,        // Exchange trading symbol
    pub display_name: String,          // Dhan display name
    pub instrument_type: String,       // Exchange instrument type
    pub series: String,                // Exchange series
    pub lot_size: u32,                 // Trading lot size
    pub expiry_date: Option<String>,   // YYYY-MM-DD for derivatives
    pub strike_price: Option<f64>,     // Options only
    pub option_type: Option<String>,   // CE or PE
    pub tick_size: f64,                // Minimum price increment
    pub expiry_flag: Option<String>,   // M or W
    pub exchange_segment: ExchangeSegment, // Derived from exchange + segment
}
```

---

## 6. Implementation Notes

1. **Download the CSV daily** — instrument list changes when new F&O contracts are listed, old ones expire, or corporate actions happen.

2. **Compact vs Detailed** — use compact for equity-only trading (smaller, faster to parse). **For F&O, the detailed CSV is required** because it contains `UNDERLYING_SECURITY_ID` and `UNDERLYING_SYMBOL` which are essential for F&O universe building (mapping derivative contracts to their underlying) and are NOT available in the compact CSV.

3. **SecurityId is NOT the same as ISIN** — SecurityId is Dhan/exchange-specific numeric. ISIN is the universal 12-char alphanumeric.

4. **Derivatives have two SecurityIds** — the derivative contract has its own SecurityId, and `UNDERLYING_SECURITY_ID` points to the underlying equity/index.

5. **Per-segment endpoint** requires `access-token` header and counts against Data API rate limits (5/sec, 100K/day).
