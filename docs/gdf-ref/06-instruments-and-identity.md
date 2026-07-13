# GDF 06 — Instruments & Identity

> **Source:** https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/websockets-api-documentation/getinstruments/ · …/documentation-support/symbol-naming-conventions/ · …/rest-api-documentation/handling-special-characters/ · …/websockets-api-documentation/getstrikeprices-2/
> **Fetched:** 2026-07-13 · **Evidence tier:** CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 :: instruments.py + README) + GITHUB-SAMPLE(js-2020, dhelm-2018) + SEARCH. Verbatim JSON from official artifacts.

---

## 1. Reference/metadata functions (WS request → Result MessageType → REST path)

| Function | WS request (verbatim shape) | WS Result | REST path |
|---|---|---|---|
| GetExchanges | `{"MessageType":"GetExchanges"}` | `ExchangesResult` — `{"Result":[{"Value":"MCX"},{"Value":"NFO"},{"Value":"NSE"}],…}` (key-entitled subset); REST full list = 9 exchanges (01 §2) | `GetExchanges/` |
| GetInstrumentTypes | `{"MessageType":"GetInstrumentTypes","Exchange":"NFO"}` | `InstrumentTypesResult` — `[{"Value":"FUTIDX"},{"Value":"FUTIVX"},{"Value":"FUTSTK"},{"Value":"OPTIDX"},{"Value":"OPTSTK"}]` | `GetInstrumentTypes/?exchange` |
| GetProducts | `{"MessageType":"GetProducts","Exchange":"NFO"}` (+opt `InstrumentType`) | `ProductsResult` — `[{"Value":"BANKNIFTY"},…,{"Value":"NIFTY"},{"Value":"INDIAVIX"},{"Value":"ACC"}]` | `GetProducts/?exchange[&instrumentType]` |
| GetExpiryDates | `{"MessageType":"GetExpiryDates","Exchange":"NFO","Product":"NIFTY"}` (+opt `InstrumentType`) | `ExpiryDatesResult` — `[{"Value":"25OCT2018"},{"Value":"27SEP2018"},…]` (DDMMMYYYY upper) | `GetExpiryDates/?exchange[&product][&instrumentType]` |
| GetOptionTypes | `{"MessageType":"GetOptionTypes","Exchange":"NFO"}` (+opt filters) | `OptionTypesResult` — `[{"Value":"CE"},{"Value":"PE"}]`; futures rows show `FF`/`XX` | `GetOptionTypes/?exchange[…]` |
| GetStrikePrices | `{"MessageType":"GetStrikePrices","Exchange":"NFO"}` (+opt `InstrumentType`,`Product`,`Expiry`,`OptionType`) | `StrikePricesResult` — `[{"Value":"0"},{"Value":"25800"},…]` (`0` = futures rows). ⚠ SDK BUG: wsgfdl drops OptionType from the JSON despite accepting the arg; REST sends `optionType` | `GetStrikePrices/?exchange[…][&optionType]` |
| GetInstruments | §3 below | `InstrumentsResult` | `GetInstruments/?exchange[…]` |
| GetInstrumentsOnSearch | `{"MessageType":"GetInstrumentsOnSearch","Exchange":"NFO","Search":"NIFTY"}` (+opt `InstrumentType`,`Product`,`Expiry`,`OptionType`,`StrikePrice`,`Series`,`onlyActive` — default TRUE; false returns even expired contracts — `DetailedInfo`) | `InstrumentsOnSearchResult` — **max 20 rows** | `GetInstrumentsOnSearch/?exchange&search[…]` |

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
| `XXX_MCXSCHANADLH_25Jun2015_XX_X` | MCX oddity (greeks sample) | expect grammar outliers |

Expiry in identifiers = `DDMMMYYYY` (upper in requests, e.g. `30JUL2020`); the master's `Expiry` field renders mixed-case `17Apr2025`.

### 2b. Continuous futures — ROLLING aliases (never a stable key)

`NIFTY-I` (current month), `NIFTY-II` (near), `NIFTY-III` (far); works on NFO/CDS/MCX (`USDINR-I`, `NATURALGAS-I`, `CRUDEOIL-I`). The underlying contract CHANGES at expiry — never use `-I/-II/-III` as a storage identity key.

### 2c. Contractwise / short format (= the master's `TradeSymbol`)

Requires `isShortIdentifier(s):"true"` on quote/snapshot/history calls. Examples: futures `NIFTY20JULFUT`, `NIFTY25MAR21FUT`, `USDINR20JULFUT`, `CRUDEOIL20AUGFUT`; options `NIFTY02JUL2010000CE`, `RELIANCE30JUL201700CE`, `USDINR29JUL2075.5CE`, `GOLD20JUL43700PE`; 4-digit-year forms exist too (`NIFTY22AUG2427800CE`, `BANKNIFTY18SEP2451000CE`).

### 2d. Indices & cash

- Indices: Exchange `NSE_IDX` (or `BSE_IDX`); identifier = index DISPLAY NAME **with spaces**: `NIFTY 50` ("single white space between NIFTY & 50" — verbatim), `NIFTY BANK`, `NIFTY 100`. NOT `NIFTY-I` (that is the future).
- Cash: Exchange `NSE`; symbol as-is including special characters (`BAJAJ-AUTO`, `L&T`, `M&M`); EQ series bare; other series suffixed with a dot: `RELCAPITAL.BE`. Interoperable instruments flagged with `#`/`$` characters (`ShowInteroperable` filter).
- WS sends special characters AS-IS; REST requires URL-encoding (`%26` for `&`, `%20`/`+` for space).
- Fully-qualified display forms seen in docs: `NIFTY-I.NFO`, `NIFTY 50.NSE_IDX` (display only, not request format).

## 3. GetInstruments — the instrument master

Verbatim GFDL warning: "VERY VERY IMPORTANT : Huge data of several MB is returned if GetInstruments is called without any optional arguments (NSE & NFO). It is strongly advised that users build a local symbol cache at their end and refresh with our server only 'on need basis'." (Arrives as ONE giant WS frame — 02 §7.)

WS request (all filters optional except Exchange; wsgfdl-py 1.3.5): `{"MessageType":"GetInstruments","Exchange":"NFO"[,"InstrumentType"][,"Product"][,"Expiry"][,"optionType"][,"strikePrice"][,"Series"][,"onlyActive"][,"DetailedInfo"][,"ShowDummyISIN"][,"ShowETF"][,"ShowInteroperable"]}` — the `Series`/`ShowDummyISIN`/`ShowETF` filters were added in the 2025-11-28 changelog.

### Row field table (2025-era detailed row; `DetailedInfo` governs richness — older rows lack the tail fields → all Option-al)

| Field | Type | Meaning |
|---|---|---|
| `Identifier` | string | LONG-format identifier (the canonical per-contract key) |
| `Name` | string | instrument TYPE (e.g. `OPTIDX`) — naming quirk |
| `Product` | string | underlying product (`NIFTY`) |
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

Verbatim 2025 sample row: `{"Identifier":"OPTIDX_NIFTY_17APR2025_PE_23700","Name":"OPTIDX","Expiry":"17Apr2025","StrikePrice":23700.0,…,"OptionType":"PE",…,"TradeSymbol":"NIFTY17APR2523700PE","QuotationLot":75.0,…,"TokenNumber":"48283",…,"ISIN":"","Series":"",…}` — CLIENT-LIB-SOURCE(wsgfdl-py@1.3.5 README). 2018 rows carried only the first 11 fields (verbatim in the mirror). REST variant uses ALL-CAPS keys and omits TokenNumber/ISIN unless `detailedInfo` — Assumed governed by that flag.

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
- "TokenNumber = the exchange's own token" is Assumed (values match exchange token spaces); not vendor-stated (U-21).
- `onlyActive` default true; expired contracts retrievable with `onlyActive:"false"` (master only — expired-contract HISTORY availability is **Unknown**, U-8).

## What this prevents

- Using `NIFTY-I` as a storage key (rolls at expiry → silent wrong-contract).
- Symbol-only equity joins (ISIN is the immutable key).
- Underestimating the master pull (multi-MB single frame; local cache mandated by GDF).
- Missing the MCX double-underscore when generating long identifiers.
