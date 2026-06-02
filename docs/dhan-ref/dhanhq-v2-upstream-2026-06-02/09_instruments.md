# Instrument List (scrip master) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/instruments/ · API v2 · Audited 2 Jun 2026

Master list of all tradable instruments with Security IDs and details needed to build with the APIs.

## CSV downloads (no auth)
- **Compact:** `https://images.dhan.co/api-data/api-scrip-master.csv`
- **Detailed:** `https://images.dhan.co/api-data/api-scrip-master-detailed.csv`

## Segmentwise list (auth'd)
Fetch one exchange-segment at a time:
```bash
curl --location 'https://api.dhan.co/v2/instrument/{exchangeSegment}'
```
`{exchangeSegment}` mapping is in `08_annexure.md` (Exchange Segment).

## Column description (Detailed `tag` → Compact `tag`)
| Detailed | Compact | Description |
|---|---|---|
| `EXCH_ID` | `SEM_EXM_EXCH_ID` | Exchange: `NSE` `BSE` `MCX` |
| `SEGMENT` | `SEM_SEGMENT` | `C` Currency, `D` Derivatives, `E` Equity, `M` Commodity |
| `ISIN` | — | 12-char ISIN |
| `INSTRUMENT` | `SEM_INSTRUMENT_NAME` | Instrument type (see `08_annexure.md`) |
| *(removed)* | `SEM_EXPIRY_CODE` | Expiry code (futures) |
| `UNDERLYING_SECURITY_ID` | — | Underlying security ID (derivatives) |
| `UNDERLYING_SYMBOL` | — | Underlying symbol (derivatives) |
| `SYMBOL_NAME` | `SM_SYMBOL_NAME` | Symbol name |
| *(removed)* | `SEM_TRADING_SYMBOL` | Exchange trading symbol |
| `DISPLAY_NAME` | `SEM_CUSTOM_SYMBOL` | Dhan display name |
| `INSTRUMENT_TYPE` | `SEM_EXCH_INSTRUMENT_TYPE` | Extra instrument detail from exchange |
| `SERIES` | `SEM_SERIES` | Exchange series |
| `LOT_SIZE` | `SEM_LOT_UNITS` | Lot size |
| `SM_EXPIRY_DATE` | `SEM_EXPIRY_DATE` | Expiry date (derivatives) |
| `STRIKE_PRICE` | `SEM_STRIKE_PRICE` | Strike price (options) |
| `OPTION_TYPE` | `SEM_OPTION_TYPE` | `CE` Call / `PE` Put |
| `TICK_SIZE` | `SEM_TICK_SIZE` | Min price decimal |
| `EXPIRY_FLAG` | `SEM_EXPIRY_FLAG` | `M` Monthly / `W` Weekly |
| `BRACKET_FLAG` | — | BO allowed: `N`/`Y` |
| `COVER_FLAG` | — | CO allowed: `N`/`Y` |
| `ASM_GSM_FLAG` | — | `N` not in ASM/GSM, `R` removed, `Y` in ASM/GSM |
| `ASM_GSM_CATEGORY` | — | Category, `NA` if none |
| `BUY_SELL_INDICATOR` | — | `A` if both buy/sell allowed |
| `BUY_CO_MIN_MARGIN_PER` | — | Buy CO min-margin % |
| `SELL_CO_MIN_MARGIN_PER` | — | Sell CO min-margin % |
| `BUY_CO_SL_RANGE_MAX_PERC` | — | Buy CO SL max range % |
| `SELL_CO_SL_RANGE_MAX_PERC` | — | Sell CO SL max range % |
| `BUY_CO_SL_RANGE_MIN_PERC` | — | Buy CO SL min range % |
| `SELL_CO_SL_RANGE_MIN_PERC` | — | Sell CO SL min range % |
| `BUY_BO_MIN_MARGIN_PER` | — | Buy BO min-margin % |
| `SELL_BO_MIN_MARGIN_PER` | — | Sell BO min-margin % |
| `BUY_BO_SL_RANGE_MAX_PERC` | — | Buy BO SL max range % |
| `SELL_BO_SL_RANGE_MAX_PERC` | — | Sell BO SL max range % |
| `BUY_BO_SL_RANGE_MIN_PERC` | — | Buy BO SL min range % |
| `SELL_BO_SL_MIN_RANGE` | — | Sell BO SL min range % |
| `BUY_BO_PROFIT_RANGE_MAX_PERC` | — | Buy BO target max range % |
| `SELL_BO_PROFIT_RANGE_MAX_PERC` | — | Sell BO target max range % |
| `BUY_BO_PROFIT_RANGE_MIN_PERC` | — | Buy BO target min range % |
| `SELL_BO_PROFIT_RANGE_MIN_PERC` | — | Sell BO target min range % |
| `MTF_LEVERAGE` | — | MTF leverage (x) for eligible `EQUITY` |

> `SecurityId` used across all APIs comes from this list. Cache it and refresh daily (it changes with expiries/listings).
