# Dhan Annexure & Enums Enforcement

> **Ground truth:** `docs/dhan-ref/08-annexure-enums.md`
> **Scope:** Any file touching Dhan enums, error codes, feed codes, rate limits, or API mappings.
> **Load alongside:** Every other Dhan rule — this file contains shared enums used everywhere.

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any Dhan API enum, constant, error handler, or rate limiter: `Read docs/dhan-ref/08-annexure-enums.md`.

2. **ExchangeSegment — dual usage, exact values.**
   - JSON requests: use STRING attribute (`"NSE_EQ"`, `"NSE_FNO"`, etc.)
   - Binary response header byte 4: use NUMERIC enum
   - Exact mapping: `IDX_I=0`, `NSE_EQ=1`, `NSE_FNO=2`, `NSE_CURRENCY=3`, `BSE_EQ=4`, `MCX_COMM=5`, `BSE_CURRENCY=7`, `BSE_FNO=8`
   - **There is NO enum 6.** Gap between MCX_COMM(5) and BSE_CURRENCY(7).
   - `from_byte()` must return `None` for unknown values including 6. No panic, no unreachable.
   - **2026-07-03 upstream update:** the live annexure REMOVED `NSE_CURRENCY=3` and
     `BSE_CURRENCY=7` (NSE currency derivatives discontinued; the 2026-06-02 snapshot still
     had them). Our decode-side enums RETAIN 3/7 as defensive arms — Dhan never emits them,
     no subscription path uses currency, and keeping them honors the no-panic-on-unknown
     contract. Do NOT delete the variants; do NOT add currency subscriptions. See
     `docs/dhan-ref/08-annexure-enums.md` "2026-07-03 Upstream Update" §(a).
   - **2026-07-14 correction (runner-crawled, Verified-live):** the 2026-07-03 note above read
     the NEW-PORTAL export, not "the live annexure". Dhan runs TWO official doc surfaces that
     DISAGREE: the classic `dhanhq.co/docs/v2/annexure/` RETAINS `NSE_CURRENCY=3` +
     `BSE_CURRENCY=7` (verbatim-stable 2026-06-02 → 2026-07-14); the portal export
     (`docs.dhanhq.co/markdown/api/v2/guides/annexure.md`) omits them (stable since
     2026-06-30). Cross-SURFACE divergence, not cross-time drift. Posture UNCHANGED: keep the
     defensive 3/7 decode arms, never add currency subscriptions. See
     `docs/dhan-ref/08-annexure-enums.md` "2026-07-14 Upstream Update".

3. **FeedRequestCode — exact numeric codes.**
   - `11`=Connect, `12`=Disconnect, `15`=SubscribeTicker, `16`=UnsubscribeTicker, `17`=SubscribeQuote, `18`=UnsubscribeQuote, `21`=SubscribeFull, `22`=UnsubscribeFull, `23`=SubscribeFullDepth, `25`=UnsubscribeFullDepth
   - **UnsubscribeFullDepth is 25, NOT 24.**
   - **2026-07-14 note (cross-surface split — UNVERIFIED-LIVE both ways):** the "25, NOT 24"
     line above is no longer assertable as doc fact. The classic annexure says **24**
     (verbatim-stable 2026-06-02 → 2026-07-14); the portal export says **25** (stable since
     2026-06-30); SDK evidence is contested (generic `subscribe_code+1` ⇒ 24 vs the
     `constants.rs:395` comment citing `fulldepth.py` ⇒ 25 — the latter needs re-verification,
     code-comment follow-up in a separate PR). Zero runtime impact — depth is FORBIDDEN
     FOREVER; any future (re-authorized) depth work MUST live-probe both codes first. See
     `docs/dhan-ref/08-annexure-enums.md` "2026-07-14 Upstream Update" §(b).

4. **FeedResponseCode — exact numeric codes.**
   - `1`=Index, `2`=Ticker, `3`=MarketDepth (v1 legacy, deprecated in v2 — replaced by Full code 8; Python SDK still handles for backward compat), `4`=Quote, `5`=OI, `6`=PrevClose, `7`=MarketStatus (8 bytes, header only), `8`=Full, `50`=Disconnect
   - Code `3` is v1 legacy only. Our code handles it defensively (112-byte parser) but v2 subscriptions will not receive it.
   - Unknown codes must log + skip. Never panic.

5. **ProductType — 6 variants.**
   - `CNC` (Cash & Carry), `INTRADAY`, `MARGIN` (Carry Forward F&O), `MTF` (Margin Trading Facility), `CO` (Cover Order), `BO` (Bracket Order)
   - Annexure table lists 3; `MTF` appears in Order/Forever Order APIs. `CO`/`BO` appear in Order Update WebSocket (`Product` field: `V`=CO, `B`=BO).

6. **OrderStatus — 9 variants.**
   - `TRANSIT`, `PENDING`, `CLOSED`, `TRIGGERED`, `REJECTED`, `CANCELLED`, `PART_TRADED`, `TRADED`, `EXPIRED`
   - `CLOSED`/`TRIGGERED` = Super Order specific. `EXPIRED` = Order Book/WS specific.
   - Handle ALL in every match arm. Context-specific subsets noted in each API rule.

7. **InstrumentType — exactly 10 variants.**
   - `INDEX`, `FUTIDX`, `OPTIDX`, `EQUITY`, `FUTSTK`, `OPTSTK`, `FUTCOM`, `OPTFUT`, `FUTCUR`, `OPTCUR`
   - **2026-07-03 upstream update:** the live annexure now lists 8 types — `FUTCUR`/`OPTCUR`
     dropped with the currency-segment removal. Our enum variants are KEPT (CSV back-compat;
     in crates they appear only in exclusion filters + `types.rs`). See
     `docs/dhan-ref/08-annexure-enums.md` "2026-07-03 Upstream Update" §(c).
   - **2026-07-14 correction (runner-crawled, Verified-live):** the "8 types" reading is the
     PORTAL export's; the classic annexure still lists all 10 incl. FUTCUR/OPTCUR
     (verbatim-stable 2026-06-02 → 2026-07-14). Cross-surface divergence, not time drift.
     Variants stay KEPT — posture unchanged.

8. **ExpiryCode — exactly 3 values.**
   - `0`=Current/Near, `1`=Next, `2`=Far
   - **2026-07-03 upstream update — UNVERIFIED-LIVE:** the live annexure renumbered to
     `1`=Near, `2`=Next, `3`=Far. No runtime consumer exists (the historical fetch chain —
     the only `expiryCode` send site — was deleted 2026-05-26; re-adding it is banned by
     `no-rest-except-live-feed-2026-06-27.md`), so our `ExpiryCode` enum stays 0/1/2. Any
     FUTURE consumer MUST live-probe `/v2/charts/historical` with BOTH conventions before
     trusting either numbering. See `docs/dhan-ref/08-annexure-enums.md` "2026-07-03
     Upstream Update" §(b).
   - **2026-07-14 correction (runner-crawled, Verified-live for the DOCS):** only the PORTAL
     annexure says 1/2/3 (stable since 2026-06-30); the classic annexure still says 0/1/2
     (verbatim-stable 2026-06-02 → 2026-07-14) — an intra-Dhan cross-surface contradiction,
     not a renumber-in-time. Endpoint samples split the same way: the classic
     `/charts/historical` sample sends `"expiryCode": 0`; the expired-options page
     (`/charts/rollingoption`, a DIFFERENT API) samples `1` — do not conflate. The
     live-probe-BOTH-conventions mandate STANDS unchanged.

9. **AfterMarketOrder — exactly 4 values.**
   - `PRE_OPEN`, `OPEN`, `OPEN_30`, `OPEN_60`

10. **Rate limits — exact numbers per API category.**
    - Order: 10/sec, 250/min, 1000/hr, 7000/day. Max 25 modifications per order.
    - Data: 5/sec, 100,000/day
    - Quote: 1/sec (unlimited per min/hr/day)
    - Non-Trading: 20/sec (unlimited per min/hr/day)
    - Limits are per `dhanClientId`, NOT per IP.

11. **Trading API Errors — exact codes and handling.**
    - `DH-901`: Invalid auth → rotate token → retry ONCE → HALT if still fails
    - `DH-902`: No API access → HALT + alert
    - `DH-903`: Account issues → HALT + alert
    - `DH-904`: Rate limit → exponential backoff (10s→20s→40s→80s→give up + CRITICAL)
    - `DH-905`: Input exception → NEVER retry (fix the request)
    - `DH-906`: Order error → NEVER retry (fix the order)
    - `DH-907`: Data error → check params, no blind retry
    - `DH-908`: Internal server error → retry with backoff (rare)
    - `DH-909`: Network error → retry with backoff
    - `DH-910`: Other → log + alert

12. **Data API Errors — exact codes.**
    - `800`=Internal Server Error, `804`=Instruments exceed limit, `805`=Too many requests/connections (STOP ALL 60s), `806`=Data APIs not subscribed, `807`=Access token expired (trigger token refresh), `808`=Auth failed, `809`=Token invalid, `810`=Client ID invalid, `811`=Invalid expiry date, `812`=Invalid date format, `813`=Invalid SecurityId, `814`=Invalid request

13. **Timestamps — two conventions depending on data source.**
    - **Historical REST API** (`/v2/charts/historical`, `/v2/charts/intraday`): standard UNIX epoch (seconds since 1970-01-01 00:00 UTC). Add +5:30 (19800 seconds) for IST display.
    - **WebSocket Live Market Feed** (LTT fields in binary packets): IST epoch seconds. Do NOT add +5:30 — the raw value already represents IST wall-clock time. To convert to UTC, subtract 19800 seconds.
    - NOT milliseconds, NOT microseconds, NOT custom epoch.

14. **Error response structure — exactly 3 string fields.**
    - `errorType`, `errorCode`, `errorMessage` — all strings, all always present.

15. **Unknown codes must not panic.** Every `TryFrom` / `from_byte()` / `from_*` must have a fallback arm returning `None` or `Err(Unknown(value))`. No `unreachable!()`, no `panic!()`.

## What This Prevents

- Wrong numeric code → wrong packet parsing → silent data corruption
- Missing enum variant → unhandled match arm → panic in production
- Wrong retry strategy → API block (805) or token loop (901)
- Hallucinated codes → enum variants that don't exist in Dhan API
- Wrong rate limits → DH-904 flood → account suspension
- Stale mappings → code says 24, Dhan says 25 → subscription failures
- Timestamp confusion → dates off by 1000x

## Trigger

This rule activates when editing files matching:
- `crates/common/src/types.rs`
- `crates/common/src/order_types.rs`
- `crates/common/src/instrument_types.rs`
- `crates/common/src/constants.rs`
- `crates/common/src/error.rs`
- `crates/common/src/segment.rs`
- `crates/core/src/parser/types.rs`
- `crates/core/src/parser/disconnect.rs`
- `crates/core/src/websocket/types.rs`
- `crates/trading/src/oms/rate_limiter.rs`
- `crates/trading/src/oms/api_client.rs`
- `crates/trading/tests/gap_enforcement.rs`
- `crates/core/tests/gap_enforcement.rs`
- `crates/common/tests/schema_validation.rs`
- Any file containing `ExchangeSegment`, `FeedRequestCode`, `FeedResponseCode`, `DisconnectCode`, `DhanErrorCode`, `DataApiError`, `InstrumentType`, `OrderStatus`, `ProductType`, `ExpiryCode`, `AfterMarketOrder`, `DH-901`, `DH-904`, `DH-905`, `DH-906`
