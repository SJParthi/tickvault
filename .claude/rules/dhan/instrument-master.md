# Dhan Instrument Master Enforcement

> **Ground truth:** `docs/dhan-ref/09-instrument-master.md`
> **Scope:** Any file touching instrument master download, CSV parsing, SecurityId lookup, or universe building.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment, InstrumentType enums)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any instrument loader, CSV parser, or SecurityId mapper: `Read docs/dhan-ref/09-instrument-master.md`.

2. **CSV download URLs are exact.**
   - Compact: `https://images.dhan.co/api-data/api-scrip-master.csv` (no auth needed — public)
   - Detailed: `https://images.dhan.co/api-data/api-scrip-master-detailed.csv` (no auth needed — public)
   - Per-segment: `https://api.dhan.co/v2/instrument/{exchangeSegment}` (requires `access-token` header, counts against Data API rate limits 5/sec)

3. **Column names differ between compact and detailed CSV.**
   - Compact uses `SEM_*` / `SM_*` prefixes: `SEM_EXM_EXCH_ID`, `SEM_SEGMENT`, `SEM_INSTRUMENT_NAME`, `SEM_EXPIRY_CODE`, `SM_SYMBOL_NAME`, `SEM_TRADING_SYMBOL`, `SEM_CUSTOM_SYMBOL`, `SEM_EXCH_INSTRUMENT_TYPE`, `SEM_SERIES`, `SEM_LOT_UNITS`, `SEM_EXPIRY_DATE`, `SEM_STRIKE_PRICE`, `SEM_OPTION_TYPE`, `SEM_TICK_SIZE`, `SEM_EXPIRY_FLAG`
   - Detailed uses plain names: `EXCH_ID`, `SEGMENT`, `ISIN`, `INSTRUMENT`, `UNDERLYING_SECURITY_ID`, `UNDERLYING_SYMBOL`, `SYMBOL_NAME`, `DISPLAY_NAME`, `INSTRUMENT_TYPE`, `SERIES`, `LOT_SIZE`, `SM_EXPIRY_DATE`, `STRIKE_PRICE`, `OPTION_TYPE`, `TICK_SIZE`, `EXPIRY_FLAG`, `ASM_GSM_FLAG`, `ASM_GSM_CATEGORY`, `BUY_SELL_INDICATOR`, `MTF_LEVERAGE`
   - Check the mapping table in the reference when parsing.

4. **SecurityId is the primary key for ALL Dhan API calls.** Every subscription, order, quote, and historical data request uses SecurityId. The instrument master CSV is the ONLY source of truth. Never hardcode SecurityId values — they can change when instruments are relisted or new series are issued.

5. **Refresh daily before market open.** Download at system startup. Validate row count > 0. Cache in memory + Valkey. If download fails after 3 retries → CRITICAL alert + HALT.

6. **Use detailed CSV for F&O.** Detailed CSV has `UNDERLYING_SECURITY_ID`, `UNDERLYING_SYMBOL`, `ISIN`, `ASM_GSM_FLAG`, `MTF_LEVERAGE`. Compact CSV lacks these — insufficient for F&O universe building.

7. **Derivative SecurityIds are NOT stable across days.** New contracts get new IDs. Expired contracts lose their IDs. Never hardcode. Always lookup from fresh daily download.

8. **SecurityId is NOT the same as ISIN.** SecurityId = Dhan/exchange-specific numeric ID (u32). ISIN = universal 12-character alphanumeric identifier.

9. **Derivatives have two SecurityIds.** The derivative contract has its own SecurityId, and `UNDERLYING_SECURITY_ID` points to the underlying equity/index.

10. **Segment codes in CSV.** `C`=Currency, `D`=Derivatives, `E`=Equity, `I`=Index, `M`=Commodity.

11. **Option type values.** `CE`=Call European, `PE`=Put European. Use `Option<String>` — not all instruments have option types.

12. **Expiry flag values.** `M`=Monthly, `W`=Weekly. Use `Option<String>`.

13. **SecurityId in binary WebSocket header = bytes 4-7 (0-based), int32 LE.** See `dhan-live-market-feed.md` rule 4 for full header layout. SecurityId in JSON subscribe = STRING (`"1333"` not `1333`).

## What This Prevents

- Stale instrument master → expired contracts in universe → orders for dead instruments
- Wrong column name → parse failure → zero instruments loaded
- Hardcoded SecurityId → works today, breaks tomorrow when new contract listed
- Compact CSV for F&O → missing strike/expiry/underlying data → cannot identify contracts
- Failed download without alert → silent failure, no instruments
- Wrong SecurityId type (int vs string) in wrong context → subscription rejection

## Trigger

This rule activates when editing files matching:
- `crates/core/src/instrument/*.rs`
- `crates/common/src/instrument_registry.rs`
- `crates/common/src/instrument_types.rs`
- `crates/storage/src/instrument_persistence.rs`
- `crates/api/src/handlers/instruments.rs`
- Any file containing `InstrumentMaster`, `InstrumentRecord`, `scrip-master`, `api-scrip-master`, `SecurityId`, `SEM_INSTRUMENT_NAME`, `SEM_LOT_UNITS`, `SEM_EXPIRY_DATE`, `SEM_STRIKE_PRICE`, `SM_SYMBOL_NAME`, `FnoKey`, `universe_builder`, `csv_parser`, `csv_downloader`, `instrument_loader`
