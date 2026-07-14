# Dhan V2 Annexure — Enums & Reference Values

> **Source**: https://dhanhq.co/docs/v2/annexure/
> **Extracted**: 2026-03-13
> **Purpose**: Shared reference — load alongside any other Dhan API doc in Claude Code
> **⚠ Read the "2026-07-03 Upstream Update" section below FIRST** — three enum tables in this
> file drifted from the live annexure after the 2026-06-02 upstream snapshot. Original tables
> are retained per repo convention (dated supersede sections, never silent rewrites).
> **⚠ Then read the "2026-07-14 Upstream Update"** — a runner crawl proved the 2026-07-03
> section compared the NEW-PORTAL export, not the classic annexure; Dhan runs TWO official
> doc surfaces that DISAGREE with each other (the two-surface story below).

---

## 2026-07-03 Upstream Update (post-2026-06-02 live-annexure drift)

The repo's upstream snapshot `dhanhq-v2-upstream-2026-06-02/` still matched the tables below.
The CURRENT live annexure (compared 2026-07-03) has three changes. Original tables in §1/§6/§7
are RETAINED as annotated history; do NOT silently rewrite them.

### (a) Exchange Segment — NSE_CURRENCY (3) and BSE_CURRENCY (7) REMOVED upstream

The live annexure now lists only: `IDX_I=0`, `NSE_EQ=1`, `NSE_FNO=2`, `BSE_EQ=4`, `MCX_COMM=5`,
`BSE_FNO=8`. NSE currency derivatives were discontinued, and Dhan dropped both currency segments
from the docs. **Our runtime enums (`crates/common/src/types.rs` / `segment.rs`) RETAIN 3 and 7
as defensive decode arms** — harmless (Dhan simply never emits them; no subscription path uses
currency, which is explicitly EXCLUDED per the daily-universe lock §5), and consistent with the
no-panic-on-unknown contract. The "NO enum 6" gap note in §1 remains true; there are now
additional gaps at 3 and 7 upstream.

### (b) Expiry Code — upstream renumbered to 1=Near, 2=Next, 3=Far — **UNVERIFIED-LIVE**

The live annexure now reads `1`=Near, `2`=Next, `3`=Far (was `0`=Current/Near, `1`=Next,
`2`=Far per §7 below and the 2026-06-02 snapshot). It is NOT determinable from docs alone
whether this is a REAL API renumber or a Dhan docs error — the long-standing SDK note in §7
(Python SDK accepts an undocumented `3`) hints the API may have tolerated a 4th value for a
while, consistent with a genuine 1/2/3 renumber. **Runtime impact: NONE today** — `ExpiryCode`
exists only as an unused type in `crates/common/src/instrument_types.rs` (zero send sites; the
historical fetch chain, the only `expiryCode` consumer, was deleted 2026-05-26 and re-adding it
is banned by `no-rest-except-live-feed-2026-06-27.md`). **Any future consumer MUST live-probe
the API before trusting EITHER numbering** (send both conventions to `/v2/charts/historical`
and compare responses). Do NOT change the §7 table or the Rust enum without that live probe.

### (c) Instrument Types — now 8 upstream (FUTCUR / OPTCUR dropped)

With the currency segments gone, the live annexure lists 8 instrument types (the §6 table's 10
minus `FUTCUR` and `OPTCUR`). Our enum variants are KEPT for CSV back-compat — in crates they
appear only in the F&O extractor's EXCLUSION filters and `types.rs`; if the master CSV stops
carrying those rows the exclusion filter simply matches nothing. No code change.

---

## 2026-07-14 Upstream Update (runner-crawled live pages) — TWO official doc surfaces DISAGREE

**Evidence tier: Verified-live.** A GitHub-Actions runner fetched the raw server-rendered HTML
of `https://dhanhq.co/docs/v2/annexure/` (runs 1–3, 2026-07-13T19:36Z → 2026-07-14T07:58:09Z,
sha256 `1a9330a1` content-identical across runs) AND the NEW portal's markdown export
`https://docs.dhanhq.co/markdown/api/v2/guides/annexure.md` (2026-07-14T08:01:01Z, sha
`40a17f84`). Diff was comment-aware (content inside HTML comments counted as non-rendered).
This section SUPERSEDES the FRAMING of the 2026-07-03 §(a)/(b)/(c) above — the originals stay
per house style. Full manifest: `00-COVERAGE-MANIFEST.md`.

### (a) The two-surface story (supersedes "the live annexure removed / renumbered / lists 8")

Dhan operates TWO official documentation surfaces that DISAGREE with each other on every
contested enum, and NEITHER has changed since first observed:

| Capture | Surface | Currency 3/7 | ExpiryCode | Instr. types | Unsub full depth |
|---|---|---|---|---|---|
| 2026-06-02 snapshot (`dhanhq-v2-upstream-2026-06-02/08_annexure.md`) | classic `dhanhq.co/docs/v2/annexure/` | PRESENT | 0/1/2 | 10 | **24** |
| 2026-06-30 LLM export (= the 2026-07-03 snapshot `20-annexure.md`) | portal `docs.dhanhq.co` | ABSENT | 1/2/3 | 8 | **25** |
| 2026-07-13/14 runner crawl (runs 1–3, sha `1a9330a1`, stable) | classic | PRESENT | 0/1/2 | 10 | **24** |
| 2026-07-14T08:01Z run-3 markdown export (sha `40a17f84`) | portal `/markdown/` | ABSENT | 1/2/3 | 8 | **25** |

- The **classic annexure NEVER changed**: `NSE_CURRENCY | 3` and `BSE_CURRENCY | 7` are
  RENDERED (not comments); ExpiryCode reads `0 | Current Expiry/Near Expiry`, `1 | Next`,
  `2 | Far`; all 10 instrument types incl. FUTCUR/OPTCUR present — verbatim-stable
  2026-06-02 → 2026-07-14.
- The **portal export never changed either** since first observed (2026-06-30 → 2026-07-14):
  numeric table `IDX_I=0, NSE_EQ=1, NSE_FNO=2, BSE_EQ=4, MCX_COMM=5, BSE_FNO=8` (no currency
  rows), ExpiryCode `1=Near, 2=Next, 3=Far`, 8 instrument types.
- The 2026-07-03 "Upstream Update" above **read the PORTAL export** (its own source header:
  docs.dhanhq.co "Export .md for LLMs", generated 2026-06-30) and **misattributed
  cross-SURFACE divergence as cross-TIME drift** ("Dhan dropped…", "upstream renumbered…").
  The reading was accurate FOR ITS SURFACE but wrong about "the live annexure" having changed.
- Which surface is authoritative per-item CANNOT be adjudicated from docs alone: the portal's
  values align with real-world facts (NSE currency-derivative discontinuation; the SDK's
  tolerated `expiryCode=3`; the expired-options sample `1`), but the same export family
  carries known LIVE corruption (the transposed rate-limit dailies — see
  `01-introduction-and-rate-limits.md` "2026-07-14 Upstream Update"). Wire truth for every
  contested enum stays **UNVERIFIED-LIVE**.
- **Repo posture that survives ALL readings (UNCHANGED):** keep the defensive 3/7 decode
  arms; keep `ExpiryCode` 0/1/2 with §(b)'s live-probe-BOTH-conventions mandate (note: the
  classic historical-data page's sample sends `"expiryCode": 0`; the expired-options page —
  a DIFFERENT API, `/charts/rollingoption` — samples `1`; do not conflate the two endpoints);
  keep the 10 instrument-type variants; keep depth forbidden. Nothing here is
  runtime-load-bearing today.

### (b) Unsubscribe Full Market Depth — 24-vs-25 is a CROSS-SURFACE split (supersedes §2's "25, per this annexure")

Classic annexure (BOTH the 2026-06-02 snapshot at `08_annexure.md:84` AND the 2026-07-14 live
page, rendered): `23 | Subscribe - Full Market Depth` then `24 | Unsubscribe - Full Market
Depth`. Portal export (2026-06-30 snapshot AND 2026-07-14 live): `25`. The §2 SDK note's
"the correct code per this annexure is 25" was ALREADY contradicted by the repo's own
2026-06-02 classic snapshot — the 25-vs-annexure conflict predates this crawl. Evidence stack:
**24** = classic surface (stable) + the SDK's generic `subscribe_code + 1` behavior (per the
§2 SDK note itself); **25** = portal export + a `crates/common/src/constants.rs:395` comment
citing `fulldepth.py` (not vendored — unverifiable from this repo). Weight of evidence has
SHIFTED toward 24; both readings are recorded and NEITHER is asserted as wire truth —
**UNVERIFIED-LIVE both ways, zero runtime impact** (depth WebSockets are FORBIDDEN FOREVER
per `websocket-connection-scope-lock.md`; any currently-banned future depth work MUST
live-probe before trusting either value). Code follow-up (comment-only, separate PR): the
`constants.rs:395` "There is NO code 24" claim is now cross-surface-contested.

### (c) Smaller classic-surface deltas (2026-07-14, Verified-live)

- **Order Status (§5):** the classic annexure table lists exactly **8** statuses (TRANSIT,
  PENDING, CLOSED, TRIGGERED, REJECTED, CANCELLED, PART_TRADED, TRADED) — no EXPIRED row; the
  §5 EXPIRED/CONFIRM "not in original annexure table" annotations remain correct.
- **Product Type (§4):** the classic annexure table now INCLUDES `CO | Cover Order` and
  `BO | Bracket Order`, plus a new table note: "CO & BO product types will be valid only for
  Intraday." — §4's "(appears in Order Update WS …)" framing for CO/BO is stale (they are
  annexure-listed now). MTF remains ABSENT from the annexure table (§4's MTF annotation
  stands).
- **DH-911 (§10):** NOT present on either live surface (the classic Trading API Error table
  ends at DH-910; grep across the whole 191-page crawl incl. comments: 0 hits, releases page
  included). The §10 DH-911 row is comms/inference-sourced, NOT annexure-backed — and no
  `ErrorCode` variant exists in `crates/common/src/error_code.rs` either (only a "Reserved"
  rule-file stub in `wave-4-error-codes.md`). Related Secondary datapoint: a live static-IP
  reject has been observed surfacing as **DH-905** with errorMessage "Invalid IP".
- **Rate Limits (§9) placement:** the classic ANNEXURE page carries no rate-limit section —
  the table lives on the introduction page (verified identical there). §9 is
  content-correct, placement-annotated; do not cite "the annexure" for rate limits.
- **§12 placement note:** the live annexure has no timestamp-format section; §12 is
  repo-derived cross-verification (Ticket/SDK-based) — correct, but not annexure content.

---

## 1. Exchange Segment

Used in TWO different ways:
- **JSON requests** (subscribe, order placement): Use the **string** attribute (e.g., `"NSE_EQ"`)
- **Binary response header** (byte 4 in WebSocket): Use the **numeric enum** value

| Attribute       | Exchange | Segment              | Numeric Enum |
|-----------------|----------|----------------------|--------------|
| `IDX_I`         | Index    | Index Value          | `0`          |
| `NSE_EQ`        | NSE      | Equity Cash          | `1`          |
| `NSE_FNO`       | NSE      | Futures & Options    | `2`          |
| `NSE_CURRENCY`  | NSE      | Currency             | `3`          |
| `BSE_EQ`        | BSE      | Equity Cash          | `4`          |
| `MCX_COMM`      | MCX      | Commodity            | `5`          |
| `BSE_CURRENCY`  | BSE      | Currency             | `7`          |
| `BSE_FNO`       | BSE      | Futures & Options    | `8`          |

> **CRITICAL**: There is NO enum `6`. Gap between MCX_COMM(5) and BSE_CURRENCY(7).

### Rust Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExchangeSegment {
    IdxI        = 0,
    NseEq       = 1,
    NseFno      = 2,
    NseCurrency = 3,
    BseEq       = 4,
    McxComm     = 5,
    BseCurrency = 7,
    BseFno      = 8,
}

impl ExchangeSegment {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::IdxI),
            1 => Some(Self::NseEq),
            2 => Some(Self::NseFno),
            3 => Some(Self::NseCurrency),
            4 => Some(Self::BseEq),
            5 => Some(Self::McxComm),
            7 => Some(Self::BseCurrency),
            8 => Some(Self::BseFno),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IdxI        => "IDX_I",
            Self::NseEq       => "NSE_EQ",
            Self::NseFno      => "NSE_FNO",
            Self::NseCurrency => "NSE_CURRENCY",
            Self::BseEq       => "BSE_EQ",
            Self::McxComm     => "MCX_COMM",
            Self::BseCurrency => "BSE_CURRENCY",
            Self::BseFno      => "BSE_FNO",
        }
    }
}
```

---

## 2. Feed Request Codes (JSON subscribe/unsubscribe)

| Code | Action                         |
|------|--------------------------------|
| `11` | Connect Feed                   |
| `12` | Disconnect Feed                |
| `15` | Subscribe — Ticker Packet      |
| `16` | Unsubscribe — Ticker Packet    |
| `17` | Subscribe — Quote Packet       |
| `18` | Unsubscribe — Quote Packet     |
| `21` | Subscribe — Full Packet        |
| `22` | Unsubscribe — Full Packet      |
| `23` | Subscribe — Full Market Depth  |
| `25` | Unsubscribe — Full Market Depth|

> **SDK Note**: Python SDK `marketfeed.py` defines `Depth = 19` as a v1-only depth subscribe code (standalone market depth packet). This is deprecated in v2 and NOT listed above. The SDK rejects it for v2 subscriptions. Also note: the SDK's generic unsubscribe logic (`subscribe_code + 1`) produces code `24` for depth unsubscribe, but the correct code per this annexure is `25`. Our code uses `25`.

---

## 3. Feed Response Codes (Binary Header Byte 1)

| Code | Meaning                |
|------|------------------------|
| `1`  | Index Packet           |
| `2`  | Ticker Packet          |
| `3`  | Market Depth Packet (v1 legacy — deprecated in v2, replaced by Full packet code 8. Python SDK still handles it for backward compat.) |
| `4`  | Quote Packet           |
| `5`  | OI Packet              |
| `6`  | Prev Close Packet      |
| `7`  | Market Status Packet (8 bytes, header only — no additional payload) |
| `8`  | Full Packet            |
| `50` | Feed Disconnect        |

---

## 4. Product Type

| Attribute  | Detail                                  |
|------------|-----------------------------------------|
| `CNC`      | Cash & Carry for equity deliveries      |
| `INTRADAY` | Intraday for Equity, Futures & Options  |
| `MARGIN`   | Carry Forward in Futures & Options      |
| `MTF`      | Margin Trading Facility (appears in Order/Forever Order APIs, not in annexure table) |
| `CO`       | Cover Order — with stop-loss (appears in Order Update WS `Product` field as `"V"`) |
| `BO`       | Bracket Order — with target + stop-loss (appears in Order Update WS `Product` field as `"B"`) |

---

## 5. Order Status

| Attribute      | Detail                                                    |
|----------------|-----------------------------------------------------------|
| `TRANSIT`      | Did not reach the exchange server                         |
| `PENDING`      | Awaiting execution                                        |
| `CLOSED`       | Super Order: both entry and exit orders placed            |
| `TRIGGERED`    | Super Order: Target or Stop Loss leg triggered            |
| `REJECTED`     | Rejected by broker/exchange                               |
| `CANCELLED`    | Cancelled by user                                         |
| `PART_TRADED`  | Partial quantity traded                                   |
| `TRADED`       | Executed successfully                                     |
| `EXPIRED`      | Order expired (Order Book, Forever Orders, Order Update WS — not in original annexure table) |
| `CONFIRM`      | Forever Order specific: active and waiting for trigger condition to be met |

---

## 6. Instrument Types

| Attribute  | Detail                      |
|------------|-----------------------------|
| `INDEX`    | Index                       |
| `FUTIDX`   | Futures of Index            |
| `OPTIDX`   | Options of Index            |
| `EQUITY`   | Equity                      |
| `FUTSTK`   | Futures of Stock            |
| `OPTSTK`   | Options of Stock            |
| `FUTCOM`   | Futures of Commodity        |
| `OPTFUT`   | Options of Commodity Futures|
| `FUTCUR`   | Futures of Currency         |
| `OPTCUR`   | Options of Currency         |

---

## 7. Expiry Code

| Code | Detail                     |
|------|----------------------------|
| `0`  | Current Expiry/Near Expiry |
| `1`  | Next Expiry                |
| `2`  | Far Expiry                 |

> **SDK Note**: Python SDK `_historical_data.py` validates `expiry_code` against `[0, 1, 2, 3]`, accepting a fourth value `3`. The Dhan API documentation only lists 0/1/2. The meaning of `3` is undocumented — avoid using it unless verified.

---

## 8. After Market Order Time

| Attribute  | Detail                                |
|------------|---------------------------------------|
| `PRE_OPEN` | AMO pumped at pre-market session      |
| `OPEN`     | AMO pumped at market open             |
| `OPEN_30`  | AMO pumped 30 min after market open   |
| `OPEN_60`  | AMO pumped 60 min after market open   |

---

## 9. Rate Limits

| Rate Limit  | Order APIs | Data APIs | Quote APIs | Non Trading APIs |
|-------------|-----------|-----------|------------|------------------|
| per second  | 10        | 5         | 1          | 20               |
| per minute  | 250       | —         | Unlimited  | Unlimited        |
| per hour    | 1000      | —         | Unlimited  | Unlimited        |
| per day     | 7000      | 100000    | Unlimited  | Unlimited        |

> Order modifications capped at **25 modifications per order**.

---

## 10. Trading API Errors

| Type                   | Code     | Message                                                |
|------------------------|----------|--------------------------------------------------------|
| Invalid Authentication | `DH-901` | Client ID or access token invalid/expired              |
| Invalid Access         | `DH-902` | Not subscribed to Data APIs or no Trading API access   |
| User Account           | `DH-903` | Account issues — check segment activation              |
| Rate Limit             | `DH-904` | Too many requests — throttle API calls                 |
| Input Exception        | `DH-905` | Missing fields, bad parameter values                   |
| Order Error            | `DH-906` | Incorrect order request                                |
| Data Error             | `DH-907` | Incorrect params or no data present                    |
| Internal Server Error  | `DH-908` | Server processing failure (rare)                       |
| Network Error          | `DH-909` | API unable to communicate with backend                 |
| Others                 | `DH-910` | Other errors                                           |
| Invalid IP             | `DH-911` | Invalid IP address (static IP enforcement)             |

---

## 11. Data API Errors (WebSocket & Data endpoints)

| Code  | Description                                                              |
|-------|--------------------------------------------------------------------------|
| `800` | Internal Server Error                                                    |
| `804` | Requested number of instruments exceeds limit                            |
| `805` | Too many requests/connections — may result in user being blocked         |
| `806` | Data APIs not subscribed                                                 |
| `807` | Access token expired                                                     |
| `808` | Authentication Failed — Client ID or Access Token invalid                |
| `809` | Access token invalid                                                     |
| `810` | Client ID invalid                                                        |
| `811` | Invalid Expiry Date                                                      |
| `812` | Invalid Date Format                                                      |
| `813` | Invalid SecurityId                                                       |
| `814` | Invalid Request                                                          |

---

## 12. Timestamp Format (V2)

Dhan V2 uses **two different timestamp conventions** depending on the data source:

### 12a. Historical REST API — UTC epoch seconds

Historical Data endpoints (`/v2/charts/historical`, `/v2/charts/intraday`) return **standard UNIX epoch (seconds since Jan 1, 1970 00:00 UTC)**.

Cross-verified from 2 sources:
- Historical Data daily: `1326220200` = IST 2012-01-11 00:00:00 (midnight IST) = UTC 2012-01-10 18:30:00
- Historical Data intraday: `1328845500` = IST 09:15:00 (NSE market open) = UTC 03:45:00

**To get IST from REST timestamps: add +5:30 (19800 seconds) or use timezone-aware parsing.**

```rust
use chrono::{FixedOffset, TimeZone};
let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
let dt = ist.timestamp_opt(epoch as i64, 0).unwrap();
```

### 12b. WebSocket Live Market Feed — IST epoch seconds

WebSocket LTT (Last Trade Time) fields send **IST epoch seconds (seconds since Jan 1, 1970 00:00 IST)**, NOT UTC. The raw u32 value already represents IST. Do NOT add +5:30 — that would double-offset. To convert to UTC for storage, SUBTRACT 19800 seconds.

```rust
use chrono::{FixedOffset, TimeZone, NaiveDateTime};
// WebSocket LTT is IST epoch — interpret directly as IST
let naive = NaiveDateTime::from_timestamp_opt(ltt as i64, 0).unwrap();
let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
let ist_dt = ist.from_local_datetime(&naive).unwrap();
// To get UTC: subtract 19800s from the raw value
let utc_epoch = ltt as i64 - 19800;
```

> **SDK caveat**: Python SDK `marketfeed.py` `utc_time()` uses `datetime.utcfromtimestamp()` on WebSocket LTT values, which would be incorrect if the values are IST epoch. The SDK's `convert_to_date_time()` in `dhanhq.py` adds `+5:30`, which is correct for REST API timestamps but would double-offset WebSocket LTT. Treat SDK timestamp handling as potentially unreliable for WebSocket data.

> Dhan v1 Historical Data used custom epoch from Jan 1, 1980 IST — v2 uses standard UNIX epoch for REST, IST epoch for WebSocket.

---

## 13. Conditional Trigger Enums

### Comparison Types

| Type                        | What it compares                           |
|-----------------------------|-------------------------------------------|
| `TECHNICAL_WITH_VALUE`      | Technical indicator vs fixed number        |
| `TECHNICAL_WITH_INDICATOR`  | Technical indicator vs another indicator   |
| `TECHNICAL_WITH_CLOSE`      | Technical indicator vs closing price       |
| `PRICE_WITH_VALUE`          | Market price vs fixed value                |

### Indicator Names

SMA: `SMA_5`, `SMA_10`, `SMA_20`, `SMA_50`, `SMA_100`, `SMA_200`
EMA: `EMA_5`, `EMA_10`, `EMA_20`, `EMA_50`, `EMA_100`, `EMA_200`
Bollinger: `BB_UPPER`, `BB_LOWER`
Others: `RSI_14`, `ATR_14`, `STOCHASTIC`, `STOCHRSI_14`, `MACD_26`, `MACD_12`, `MACD_HIST`

### Operators

| Operator             | Description        |
|----------------------|--------------------|
| `CROSSING_UP`        | Crosses above      |
| `CROSSING_DOWN`      | Crosses below      |
| `CROSSING_ANY_SIDE`  | Crosses either side|
| `GREATER_THAN`       | Greater than       |
| `LESS_THAN`          | Less than          |
| `GREATER_THAN_EQUAL` | Greater or equal   |
| `LESS_THAN_EQUAL`    | Less or equal      |
| `EQUAL`              | Equal              |
| `NOT_EQUAL`          | Not equal          |

### Alert Status

| Status      | Description           |
|-------------|-----------------------|
| `ACTIVE`    | Alert currently active|
| `TRIGGERED` | Condition met         |
| `EXPIRED`   | Alert expired         |
| `CANCELLED` | Alert cancelled       |

> **Note**: Conditional trigger order sub-objects use `discQuantity` (abbreviated), while regular orders use `disclosedQuantity`. Use the exact field name for each endpoint.
