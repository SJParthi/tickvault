# GDF 06 — Instruments & Identity

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/getinstruments/ · …/documentation-support/symbol-naming-conventions/ · …/rest-api-documentation/handling-special-characters/ · …/websockets-api-documentation/getstrikeprices-2/ · …/websockets-api-documentation/getholidays/ · …/rest-api-documentation/getholidays-2/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: instruments.py + README) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH. Verbatim JSON from official artifacts.
> **2026-07-14 upgrade:** the pages named in the Source line above were machine-fetched verbatim (GitHub runner, docs-fetch/gdf-2026-07-14 @ 7d354a8d) — claims below marked **RUNNER-DOC (2026-07-14, \<url\>)** are now official-docs tier. GetHolidays (WS + REST) added — see §6.

---

## 2026-07-14 RUNNER-DOC reconciliation (machine-fetched official docs)

| Verdict | Claim | Evidence |
|---|---|---|
| CONFIRMED | GetInstruments WS request/response shape, `InstrumentsResult` envelope with `Request` echo, `OnlyActive` default true (false → active + expired), `detailedInfo` default false gating TokenNumber/LowPriceRange/HighPriceRange/ISIN/Series/High52Week/Low52Week ("if available from Exchange") | RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/getinstruments/) |
| CONFIRMED | `detailedInfo` gating on the **WS** variant is now DOC-stated (was Assumed for the REST ALL-CAPS variant only); WS param names verbatim: `Series`, `showDummyISIN`, `showETF`, `showInterOperable` (default false, `#`/`$` flagging), `OnlyActive`, `detailedInfo` | same page |
| NEW | Row field **`PriceQuotationUnit`** (string, empty in all sample rows) — absent from the 2025 wheel-README row; add to the field table | same page (2024-vintage sample rows) |
| CONFIRMED | GetStrikePrices → `StrikePricesResult`, `{"Value":"11000"},…` rows; DOC param table lists `OptionType` as a supported optional param (the wsgfdl-py drop of OptionType is therefore an SDK bug, not a protocol fact) | RUNNER-DOC (2026-07-14, …/websockets-api-documentation/getstrikeprices-2/) |
| NEW (closes the U-24 GetHolidays item) | GetHolidays exists on BOTH surfaces — WS (`From`/`To` epoch secs, default from 2010, Exchange optional) → `HolidaysResult`; REST (`year` MANDATORY, date STRINGS `MM-DD-YYYY`, JSON/XML/CSV) — full contract in §6 | RUNNER-DOC (2026-07-14, …/websockets-api-documentation/getholidays/ + …/rest-api-documentation/getholidays-2/) |
| NEW | **BFO long-format identifiers use `IF_`/`SF_`/`IO_` type prefixes with OptionType `FF` for futures** (`IF_BANKEX_28JUN2024_FF_0`, `SF_AARTIIND_27JUN2024_FF_0`, `IO_SENSEX_28JUN2024_PE_71800`) — NOT FUTIDX/OPTIDX; explains the `FF` option-type value | RUNNER-DOC (2026-07-14, …/documentation-support/symbol-naming-conventions/) |
| NEW | Continuous `-I/-II/-III` aliases also exist for **BFO** (`BANKEX-I`, `AARTIIND-I`) and for NFO **stocks** (`RELIANCE-I`, `SBIN-I`) — "I as in India" (verbatim) | same page |
| DRIFT-REFUTED | The prior "4-digit-year short-identifier forms exist too (`NIFTY22AUG2427800CE`)" note was a MIS-PARSE — the doc grammar pins short format year as **last 2 digits** (`NIFTY22AUG2427800CE` = 22-AUG-24, strike 27800); corrected in §2c | same page (grammar comments verbatim) |
| CONFIRMED | MCX long-format double underscore (`FUTCOM_CRUDEOIL_19JAN2022__0`) — the MCX futures grammar comment omits the OptionType slot entirely | same page |
| CONFIRMED | REST URL-encoding for special characters; NEW detail: the doc also encodes `-` (`MCDOWELL-N` → `MCDOWELL%2DN`); examples use `%20` for space (not `+`) | RUNNER-DOC (2026-07-14, …/rest-api-documentation/handling-special-characters/) |
| PARTIAL (U-21) | `TokenNumber` = "Token number of Symbol" (DOC verbatim) — field existence + detailedInfo gating DOC-confirmed; "= the exchange's own token" remains Assumed (values match exchange token spaces); churn-across-listings behavior remains live-probe territory | …/websockets-api-documentation/getinstruments/ |
| Unknown (unchanged) | GetExchanges / GetInstrumentTypes / GetProducts / GetExpiryDates / GetOptionTypes / GetInstrumentsOnSearch pages were NOT in the 2026-07-14 crawl — those rows stay CLIENT-LIB-SOURCE tier | fetch-log (runs 29316509037 + 29317477759) |

## 1. Reference/metadata functions (WS request → Result MessageType → REST path)

| Function | WS request (verbatim shape) | WS Result | REST path |
|---|---|---|---|
| GetExchanges | `{"MessageType":"GetExchanges"}` | `ExchangesResult` — `{"Result":[{"Value":"MCX"},{"Value":"NFO"},{"Value":"NSE"}],…}` (key-entitled subset); REST full list = 9 exchanges (01 §2) | `GetExchanges/` |
| GetInstrumentTypes | `{"MessageType":"GetInstrumentTypes","Exchange":"NFO"}` | `InstrumentTypesResult` — `[{"Value":"FUTIDX"},{"Value":"FUTIVX"},{"Value":"FUTSTK"},{"Value":"OPTIDX"},{"Value":"OPTSTK"}]` | `GetInstrumentTypes/?exchange` |
| GetProducts | `{"MessageType":"GetProducts","Exchange":"NFO"}` (+opt `InstrumentType`) | `ProductsResult` — `[{"Value":"BANKNIFTY"},…,{"Value":"NIFTY"},{"Value":"INDIAVIX"},{"Value":"ACC"}]` | `GetProducts/?exchange[&instrumentType]` |
| GetExpiryDates | `{"MessageType":"GetExpiryDates","Exchange":"NFO","Product":"NIFTY"}` (+opt `InstrumentType`) | `ExpiryDatesResult` — `[{"Value":"25OCT2018"},{"Value":"27SEP2018"},…]` (DDMMMYYYY upper) | `GetExpiryDates/?exchange[&product][&instrumentType]` |
| GetOptionTypes | `{"MessageType":"GetOptionTypes","Exchange":"NFO"}` (+opt filters) | `OptionTypesResult` — `[{"Value":"CE"},{"Value":"PE"}]`; futures rows show `FF`/`XX` | `GetOptionTypes/?exchange[…]` |
| GetStrikePrices | `{"MessageType":"GetStrikePrices","Exchange":"NFO"}` (+opt `InstrumentType`,`Product`,`Expiry`,`OptionType`) — param table **RUNNER-DOC (2026-07-14, …/getstrikeprices-2/)**; DOC sample response `{"Request":{…},"Result":[{"Value":"11000"},{"Value":"10000"},…],"MessageType":"StrikePricesResult"}` (unordered) | `StrikePricesResult` — `[{"Value":"0"},{"Value":"25800"},…]` (`0` = futures rows, CLIENT-LIB sample). ⚠ SDK BUG (confirmed vs DOC 2026-07-14 — OptionType IS a documented param): wsgfdl drops OptionType from the JSON despite accepting the arg; REST sends `optionType` | `GetStrikePrices/?exchange[…][&optionType]` |
| GetInstruments | §3 below | `InstrumentsResult` | `GetInstruments/?exchange[…]` |
| GetInstrumentsOnSearch | `{"MessageType":"GetInstrumentsOnSearch","Exchange":"NFO","Search":"NIFTY"}` (+opt `InstrumentType`,`Product`,`Expiry`,`OptionType`,`StrikePrice`,`Series`,`onlyActive` — default TRUE; false returns even expired contracts — `DetailedInfo`) | `InstrumentsOnSearchResult` — **max 20 rows** | `GetInstrumentsOnSearch/?exchange&search[…]` |
| GetHolidays (NEW 2026-07-14) | §6 below | `HolidaysResult` | `GetHolidays/?accessKey&year[&exchange][&xml]` (REST takes `year`, NOT From/To) |

## 2. Identifier grammar (the full scheme — all examples verbatim from official GFDL comments)

`InstrumentIdentifier` is a human-readable STRING in one of three formats:

### 2a. Long format — `TYPE_PRODUCT_EXPIRY_OPTTYPE_STRIKE`

| Example | Segment | Note |
|---|---|---|
| `FUTIDX_NIFTY_30JUL2020_XX_0` | NFO index future | non-option slots: `XX` + strike `0` |
| `OPTIDX_NIFTY_02JUL2020_CE_10000` | NFO index option | |
| `OPTSTK_RELIANCE_30JUL2020_CE_1700` | NFO stock option | |
| `FUTCUR_USDINR_26JUN2020_XX_0` | CDS future | |
| `OPTCUR_USDINR_29JUL2020_CE_75.5` | CDS option | fractional strikes |
| `FUTCOM_CRUDEOIL_20JUL2020__0` | MCX future | ⚠ **DOUBLE underscore** — EMPTY OptionType slot for MCX futures |
| `OPTFUT_CRUDEOIL_16JUL2020_PE_2050` | MCX option | |
| `IF_BANKEX_28JUN2024_FF_0` / `IF_SENSEX_21JUN2024_FF_0` | **BFO index future** | ⚠ BFO uses `IF_`/`SF_`/`IO_` type prefixes + OptionType **`FF`** for futures — NOT FUTIDX/OPTIDX — **RUNNER-DOC (2026-07-14, symbol-naming-conventions)** |
| `SF_AARTIIND_27JUN2024_FF_0` | BFO stock future | same page |
| `IO_SENSEX_28JUN2024_PE_71800` / `IO_BANKEX_14JUN2024_CE_56400` | BFO index option | same page (no `SO_` stock-option example shown — BSE stock options unconfirmed) |
| `XXX_MCXSCHANADLH_25Jun2015_XX_X` | MCX oddity (greeks sample) | expect grammar outliers |

Expiry in identifiers = `DDMMMYYYY` (upper in requests, e.g. `30JUL2020`); the master's `Expiry` field renders mixed-case `17Apr2025`. MCX futures grammar comment (RUNNER-DOC 2026-07-14) omits the OptionType slot entirely — hence the double underscore (`FUTCOM_CRUDEOIL_19JAN2022__0` verbatim on the conventions page).

### 2b. Continuous futures — ROLLING aliases (never a stable key)

`NIFTY-I` (current month), `NIFTY-II` (near), `NIFTY-III` (far); works on NFO/CDS/MCX (`USDINR-I`, `NATURALGAS-I`, `CRUDEOIL-I`) **and BFO (`BANKEX-I`, `AARTIIND-I`) and NFO stocks (`RELIANCE-I`, `SBIN-I`) — "I as in India" (verbatim), RUNNER-DOC (2026-07-14, symbol-naming-conventions)**. The underlying contract CHANGES at expiry — never use `-I/-II/-III` as a storage identity key.

### 2c. Contractwise / short format (= the master's `TradeSymbol`)

Requires `isShortIdentifier(s):"true"` on quote/snapshot/history calls. Examples: futures `NIFTY20JULFUT`, `NIFTY25MAR21FUT`, `USDINR20JULFUT`, `CRUDEOIL20AUGFUT`; options `NIFTY02JUL2010000CE`, `RELIANCE30JUL201700CE`, `USDINR29JUL2075.5CE`, `GOLD20JUL43700PE`.

**Grammar (RUNNER-DOC 2026-07-14, symbol-naming-conventions, verbatim):** `<underlying><expiry date in 2 digit><expiry month in 3 characters><expiry year last 2 digit>FUT` for futures; `<underlying><expiry date 2 digit><month 3 char><year last 2 digit><strike price><CE|PE>` for options — the short-format year is ALWAYS the last 2 digits. ⚠ 2026-07-14 correction: the earlier "4-digit-year forms exist too" note was a mis-parse — `NIFTY22AUG2427800CE` = 22-AUG-**24** strike 27800, `BANKNIFTY18SEP2451000CE` = 18-SEP-**24** strike 51000 (2-digit year + large strike, not a 4-digit year). Short format works for NFO, MCX (`CRUDEOIL19SEP24FUT`, `CRUDEOIL17SEP2446500CE`), CDS (`USDINR21JAN2273.25PE` — fractional strikes) and BFO (`SENSEX28JUN24FUT`, `SENSEX14JUN2476800CE`).

### 2d. Indices & cash

- Indices: Exchange `NSE_IDX` (or `BSE_IDX`); identifier = index DISPLAY NAME **with spaces**: `NIFTY 50` ("single white space between NIFTY & 50" — verbatim), `NIFTY BANK`, `NIFTY 100`. NOT `NIFTY-I` (that is the future). **BSE indices are BARE names: `SENSEX`, `BANKEX` (no spaces) — RUNNER-DOC (2026-07-14, symbol-naming-conventions).**
- Cash: Exchange `NSE`; symbol as-is including special characters (`BAJAJ-AUTO`, `L&T`, `M&M`); EQ series bare; other series suffixed with a dot: `RELCAPITAL.BE`. Interoperable instruments flagged with `#`/`$` characters (`ShowInteroperable` filter). BSE stocks: bare symbols (`ABB`, `RELIANCE`).
- WS sends special characters AS-IS; REST requires URL-encoding — RUNNER-DOC (2026-07-14, handling-special-characters) verbatim examples: `L&T`→`L%26T`, `M&M`→`M%26M`, `NIFTY 50`→`NIFTY%2050`, `NIFTY BANK`→`NIFTY%20BANK`, **`MCDOWELL-N`→`MCDOWELL%2DN` (hyphen encoded too)**; doc examples use `%20` for space (the earlier `+`-for-space note is SEARCH-tier only).
- Fully-qualified display forms seen in docs: `NIFTY-I.NFO`, `NIFTY 50.NSE_IDX` (display only, not request format).

## 3. GetInstruments — the instrument master

Verbatim GFDL warning: "VERY VERY IMPORTANT : Huge data of several MB is returned if GetInstruments is called without any optional arguments (NSE & NFO). It is strongly advised that users build a local symbol cache at their end and refresh with our server only 'on need basis'." (Arrives as ONE giant WS frame — 02 §7.)

WS request (all filters optional except Exchange; wsgfdl-py 1.3.5): `{"MessageType":"GetInstruments","Exchange":"NFO"[,"InstrumentType"][,"Product"][,"Expiry"][,"optionType"][,"strikePrice"][,"Series"][,"onlyActive"][,"DetailedInfo"][,"ShowDummyISIN"][,"ShowETF"][,"ShowInteroperable"]}` — the `Series`/`ShowDummyISIN`/`ShowETF` filters were added in the 2025-11-28 changelog.

**RUNNER-DOC (2026-07-14, …/websockets-api-documentation/getinstruments/) — the official WS param table, verbatim names/semantics:**

| Param | DOC spec |
|---|---|
| `Exchange` | required; string like `MCX` |
| `InstrumentType` / `Product` / `Expiry` / `OptionType` / `StrikePrice` | optional; F&O-only filters. Expiry sample "String value like **30Jul2015**" (mixed-case tolerated in requests; JS sample comments even show `"30jul2020"`) |
| `Series` | optional; equity/CASH only ("filter those symbols by series wise") |
| `showDummyISIN` | optional, default `false`; true includes dummy-ISIN instruments (equity/CASH) |
| `showETF` | optional, default `false`; true includes ETF instruments (equity/CASH) |
| `showInterOperable` | optional, default `false`; true includes inter-operable instruments "with special character (#/ $)" |
| `OnlyActive` | optional, default `true`; **false → all (active + expired) instruments** |
| `detailedInfo` | optional, default `false`; true adds **TokenNumber, LowPriceRange (lower circuit), HighPriceRange (upper circuit), ISIN, Series, High52Week, Low52Week — "if available from Exchange"** |

Base (non-detailed) return, DOC verbatim: `Identifier` (Symbol), `Name` (Instrument Type), `Expiry`, `StrikePrice`, `Product`, `OptionType`, `ProductMonth`, `TradeSymbol` (ShortIdentifier), `QuotationLot` (Lot Size). Envelope: `{"Request":{"Exchange":"NFO","InstrumentType":"FUTIDX","OnlyActive":true,"MessageType":"GetInstruments"},"Result":[…],"MessageType":"InstrumentsResult"}`.

Verbatim 2024 DOC sample row (note **`PriceQuotationUnit`** — a field the wheel-README row list missed):

```json
{"Identifier":"FUTIDX_NIFTY_28NOV2024_XX_0","Name":"FUTIDX","Expiry":"28Nov2024","StrikePrice":0.0,"Product":"NIFTY","PriceQuotationUnit":"","OptionType":"XX","ProductMonth":"28Nov2024","UnderlyingAsset":"","UnderlyingAssetExpiry":"","IndexName":"","TradeSymbol":"NIFTY28NOV24FUT","QuotationLot":25.0,"Description":"","TokenNumber":"35089","LowPriceRange":21241.55,"HighPriceRange":25961.9,"ISIN":"","Series":"","High52Week":0.0,"Low52Week":0.0}
```

(Same response also carries FUTIDX rows for MIDCPNIFTY lot 50 / FINNIFTY lot 25 / BANKNIFTY lot 15 / NIFTYNXT50 lot 10 — index-futures lot sizes as of the 2024 sample.)

### Row field table (2025-era detailed row; `DetailedInfo` governs richness — older rows lack the tail fields → all Option-al)

| Field | Type | Meaning |
|---|---|---|
| `Identifier` | string | LONG-format identifier (the canonical per-contract key) |
| `Name` | string | instrument TYPE (e.g. `OPTIDX`) — naming quirk |
| `Product` | string | underlying product (`NIFTY`) |
| `PriceQuotationUnit` | string | empty in every observed row — RUNNER-DOC (2026-07-14) sample field, semantics Unknown (likely commodity quotation unit) |
| `Expiry` | string | `17Apr2025` (DDMonYYYY mixed-case); empty for cash/index |
| `StrikePrice` | f64 | 0.0 for futures |
| `OptionType` | string | `CE`/`PE`/`XX`/`FF` |
| `ProductMonth` | string | contract month |
| `UnderlyingAsset` / `UnderlyingAssetExpiry` | string | for options-on-futures chains |
| `IndexName` | string | for index instruments |
| `TradeSymbol` | string | THE short/contractwise identifier (`NIFTY17APR2523700PE`) |
| `QuotationLot` | f64 | lot size |
| `Description` | string | display name |
| `TokenNumber` | string | **exchange numeric token** (e.g. `"48283"`) — Greeks APIs key on it |
| `LowPriceRange` / `HighPriceRange` | f64 | day price band |
| `ISIN` | string | populated for equities, empty for F&O |
| `Series` | string | NSE series (`EQ`, `BE`, …) |
| `High52Week` / `Low52Week` | f64 | 52-week band |

Verbatim 2025 sample row: `{"Identifier":"OPTIDX_NIFTY_17APR2025_PE_23700","Name":"OPTIDX","Expiry":"17Apr2025","StrikePrice":23700.0,…,"OptionType":"PE",…,"TradeSymbol":"NIFTY17APR2523700PE","QuotationLot":75.0,…,"TokenNumber":"48283",…,"ISIN":"","Series":"",…}` — CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 README). 2018 rows carried only the first 11 fields (verbatim in the mirror). REST variant uses ALL-CAPS keys and omits TokenNumber/ISIN unless `detailedInfo` — for the **WS** variant the `detailedInfo` gating is DOC-CONFIRMED (RUNNER-DOC 2026-07-14, param table above); for the REST ALL-CAPS variant it remains Assumed (REST GetInstruments page not in the 2026-07-14 crawl).

## 4. Mapping GDF identity → tickvault `(security_id, exchange_segment, feed)`

| Class | Join key | Rule |
|---|---|---|
| Equities | **ISIN** (GetInstruments row) → same ISIN-primary join as the Groww mapping (`daily-universe-scope-expansion-2026-05-27.md` §31.1) | symbol-alone joins BANNED (rename/reuse risk); `TradeSymbol` as cross-check |
| Indices | canonical index name — strip to `canonicalize_index_symbol` space (`NIFTY 50` → NIFTY, `NIFTY BANK` → BANKNIFTY) | display-name aliases must be evidence-extended, never guessed |
| F&O | contract identity `(underlying, expiry, strike, option_type)` — the §36 FUTIDX cross-feed precedent | NEVER by native token across feeds |
| Numeric id for hot path | `TokenNumber` = exchange token (string of digits) — viable as a per-feed `security_id` with a GDF namespace bit (Groww `1<<62` precedent); fallback = 64-bit hash of `EXCH:IDENTIFIER` with build-time collision fail-close | see 13 §4 |
| Segment mapping | `NFO`→NSE_FNO(2), `NSE`→NSE_EQ(1), `NSE_IDX`/`BSE_IDX`→IDX_I(0), `BSE`→BSE_EQ(4), `BFO`→BSE_FNO(8), `MCX`→MCX_COMM(5); `CDS`/`BSE_DEBT` out of tickvault scope | Dhan-numeric segment enum stays canonical |

## 5. Token & identifier stability caveats

- Long-format identifiers are deterministic from contract attributes → stable for a contract's life (Assumed, by construction).
- `-I/-II/-III` continuous aliases ROLL at expiry — display/subscribe convenience only.
- `TokenNumber` follows EXCHANGE token rules — derivative tokens churn as contracts list/expire (same caveat as Dhan SecurityIds: refresh the master daily, never hardcode).
- "TokenNumber = the exchange's own token" is Assumed (values match exchange token spaces); not vendor-stated. **2026-07-14 (U-21 partial):** the DOC describes it only as "Token number of Symbol" (RUNNER-DOC, getinstruments page) — existence + `detailedInfo` gating are now DOC-tier, but exchange-token identity and churn behavior remain Assumed / live-probe territory.
- `OnlyActive` default true; expired contracts retrievable with `OnlyActive:"false"` — **DOC verbatim "Function will return all (active + expired) instruments if value equals false" (RUNNER-DOC 2026-07-14)**. Master only — expired-contract HISTORY availability is still **Unknown** (U-8; no 2026-07-14 page speaks to expired-contract GetHistory).

## 6. GetHolidays — exchange holiday calendar (NEW 2026-07-14; closes the U-24 GetHolidays item)

**RUNNER-DOC (2026-07-14, https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/getholidays/ + …/rest-api-documentation/getholidays-2/).** "Returns an array of holidays related to the selected exchange" (verbatim). The two surfaces take DIFFERENT range parameters and return DIFFERENT date types:

| Aspect | WS | REST |
|---|---|---|
| Params | `Exchange` **optional**; `From`/`To` = UNIX epoch **seconds**, both optional — "by default it will return the holidays from 2010" (verbatim) | `accessKey` required; `exchange` optional; **`year` MANDATORY** (e.g. 2025); `xml` optional (default false); path `GetHolidays/?accessKey=…&exchange=MCX&year=2025[&xml=true]` (CSV variant also shown) |
| Date type | `Date` = epoch seconds (`1766601000` = 2025-12-25 IST midnight) | `Date` = STRING **`MM-DD-YYYY`** (`"01-26-2025"` = Republic Day) — note US-style month-first |
| Result | `HolidaysResult` | `{"Value":[{"Year":…,"Date":…,"Description":…,"Exchanges":…}]}` |

WS request + response verbatim:

```json
{"MessageType":"GetHolidays","From":1735669800,"To":1767119400}
```
```json
{"Request":{"Exchange":null,"From":1764527400,"To":1767033000,"MessageType":"GetHolidays"},
 "Result":[{"Date":1766601000,"Description":"Christmas",
   "Exchanges":"MCX;NSE;NSE_IDX;NFO;CDS;BSE;BSE_IDX;BSE_DEBT;BFO;NCX","MessageType":null}],
 "MessageType":"HolidaysResult"}
```

Notes:
- Each holiday row carries a semicolon-joined `Exchanges` list; the samples enumerate **10 codes: MCX, NSE, NSE_IDX, NFO, CDS, BSE, BSE_IDX, BSE_DEBT, BFO, NCX** — `NCX` (NCDEX) appears here beyond the 9-exchange GetExchanges list cited in 01 §2 (flag to the 01 owner).
- With `Exchange` omitted the WS echo shows `"Exchange":null` and rows span all exchanges — filter client-side or pass `Exchange`.
- Inner rows carry `MessageType:null` (decoder must tolerate — same class as chain rows).
- ⚠ DOC copy-paste bug: the REST page's clickable Example href points at `GetMarketMessages/` while the visible text reads `GetHolidays/` — trust the visible path.
- tickvault note: this is a REFERENCE calendar only — the NSE holiday source of truth for `TradingCalendar` remains `config/base.toml` `nse_holidays`; GDF's calendar can serve as a cross-check, never a replacement (`market-hours.md`).

- Using `NIFTY-I` as a storage key (rolls at expiry → silent wrong-contract).
- Symbol-only equity joins (ISIN is the immutable key).
- Underestimating the master pull (multi-MB single frame; local cache mandated by GDF).
- Missing the MCX double-underscore when generating long identifiers.
- Generating BFO long identifiers with `FUTIDX_`/`OPTIDX_` prefixes (BFO uses `IF_`/`SF_`/`IO_` + `FF` — 2026-07-14).
- Parsing short-format identifiers with a 4-digit-year assumption (year is always the last 2 digits; the digits after the month are year(2)+strike).
- Parsing the REST GetHolidays `Date` as DD-MM-YYYY (it is MM-DD-YYYY) or the WS `Date` as a string (it is epoch seconds).
