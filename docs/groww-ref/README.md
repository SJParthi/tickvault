# Groww Trading API — Reference (for the tickvault Groww second feed)

> **🎯 IMPLEMENTATION SCOPE (operator 2026-06-19): the Groww LIVE FEED ALONE.**
> We build ONLY the streaming live tick feed → 1-minute candles
> (`07-feed-websocket.md` LTP + index value). Everything else in these docs —
> REST snapshot (`get_quote`/`get_ltp`/`get_ohlc`), order/position updates,
> option chain, greeks, NSE/BSE EOD, indices ingestion — is **REFERENCE ONLY,
> explicitly OUT OF SCOPE**. No order placement, no portfolio, no commodities.
>
> **Why this exists:** the Groww second feed (operator lock 2026-06-19,
> `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`) is implemented
> as **native tickvault Rust** — **brutex is reference only, no code pulled.**
> These are the authoritative Groww docs the implementation is built against.
> Mirrors the `docs/dhan-ref/` pattern.
>
> **Source:** Groww Trading API Python SDK docs (`growwapi`, verified 12–13 Jun 2026).
> Saved verbatim 1:1 for reference. The SDK is **reference for the protocol + data
> shapes only** — we do NOT depend on or vendor the Python package.

## Official doc pack (operator-uploaded 2026-07-03 — prefer these where they overlap)

Captured verbatim from `https://groww.in/trade-api/docs/python-sdk` on
2026-07-03 (SDK 1.5.0, scripted 100%-match verification per page). Index:
[`00-INDEX.md`](./00-INDEX.md).

| File | What it covers |
|---|---|
| `00-INDEX.md` | Index of the 26-page official capture + key facts (1000-subscription cap wording, rate limits, MCX contradiction) |
| `01-introduction-auth-ratelimits.md` | Getting started, API key/auth, rate limits |
| `07-feed-websocket-streaming.md` | **Official** `GrowwFeed` streaming docs: subscribe_ltp / subscribe_index_value / market depth / order + position updates; payload shapes |
| `09-instruments-csv.md` | instrument.csv, exchange tokens, DataFrame helpers |
| `12-sdk-exceptions.md` | SDK exception classes |
| `13-annexures-enums.md` | All enums: exchange, segment, product, order type, validity, statuses |

## Files (2026-06 capture + reverse-read wire notes)

| File | What it covers |
|---|---|
| `01-introduction-auth.md` | Auth (API-key+secret OR **API-key+TOTP**), `get_access_token`, rate limits |
| `02-instruments.md` | Instruments master CSV schema + lookup (exchange / exchange_token / groww_symbol / isin) |
| `03-live-data-rest.md` | REST snapshot: `get_quote` / `get_ltp` / `get_ohlc` / option chain / greeks (pull-based) |
| `04-annexures.md` | Constants & enums (order status, segments, exchanges, products) |
| `05-exceptions.md` | Exception taxonomy (auth / authorisation / bad-request / rate-limit / timeout / feed-connection / feed-not-subscribed) |
| `06-changelog.md` | SDK changelog (latest 1.5.0, 3 Dec 2025) |
| `07-feed-websocket.md` | **Live streaming feed** (`GrowwFeed`): subscribe LTP / index / depth / order+position updates; data shapes |
| `08-master-groww-nse-bse-context.md` | Combined context: Groww SDK + NSE/BSE EOD market-data + **backtesting candles** (for the live-vs-backtest parity goal) |
| `09-prompt-nse-indices-data.md` | NSE indices ingestion brief (taxonomy, endpoints) |
| `instrument-sample.csv` | First 500 rows of the instruments master (header + sample) |

## Instruments master CSV — NOT vendored (fetched at runtime)

The full master is **~23.5 MB / ~164k rows**, daily-changing. Like the Dhan
instrument master, it is **downloaded at runtime, never committed** (a 23 MB
daily-stale blob in git is an anti-pattern). Only a 500-row sample lives here.

- **Canonical URL:** `https://growwapi-assets.groww.in/instruments/instrument.csv`
- **Columns:** `exchange, exchange_token, trading_symbol, groww_symbol, name,
  instrument_type, segment, series, isin, underlying_symbol,
  underlying_exchange_token, expiry_date, strike_price, lot_size, tick_size,
  freeze_quantity, is_reserved, buy_allowed, sell_allowed, internal_trading_symbol,
  is_intraday`
- **Subscription key:** the live feed identifies an instrument by
  `(exchange, segment, exchange_token)` — e.g. `{exchange:"NSE", segment:"CASH",
  exchange_token:"2885"}` for RELIANCE. `exchange_token` (NOT trading symbol) is
  the wire key.
- **ISIN** column lets us join Groww instruments to the Dhan/NSE universe by
  the immutable security identity (same ISIN-primary join used for NTM).

## Verified WIRE protocol (reverse-read from `growwapi==1.5.0` SDK source)

The SDK docs above describe the *Python interface*; the **native Rust client**
needs the underlying wire protocol, which Groww does not narratively document.
These facts were read directly from the SDK source and are the build target:

| Layer | Verified fact |
|---|---|
| Transport | **NATS** over `wss://socket-api.groww.in` (TLS WebSocket), **protobuf** payloads |
| Access token | `POST https://api.groww.in/v1/token/api/access`, header `Authorization: Bearer <api_key>` + `x-api-version: 1.0`, body `{"key_type":"totp","totp":"<6-digit>"}` (TOTP flow) OR `{"key_type":"approval","checksum":...,"timestamp":...}` (secret flow) → `{token}` |
| Socket session | client generates an **Ed25519 NKey** locally → `POST https://api.groww.in/v1/api/apex/v1/socket/token/create/`, `Authorization: Bearer <access_token>`, body `{"socketKey":"<nkey_public>"}` → `{token: <JWT>, subscriptionId}` |
| NATS auth | connect with `user_credentials = (JWT, NKey seed)` (NOT a URL param / post-connect message); `ping_interval = 60s`; NATS auto-reconnect |
| Subscribe | NATS SUB to a subject string = `<prefix><subscriptionId>`; LTP prefixes e.g. `/ld/eq/nse/price.`, `/ld/fo/nse/price.`, `/ld/indices/nse/price.`; depth `…/book.` |
| Tick payload | protobuf `StocksLivePriceProto` (all `double`): `tsInMillis` (**epoch MILLISECONDS** ✅), `ltp`, `open/high/low/close`, `volume`, `value`, `openInterest`, bid/offer qty, ranges; depth `StocksMarketDepthProto { tsInMillis, buyBook[], sellBook[] }` |
| Limits | up to **1000 subscriptions**; Live-Data REST 10/sec, 300/min |

**Honesty note:** Groww publishes no formal wire spec; the wire facts are
reverse-read from the official SDK source. A live smoke-test requires a real
Groww **API key + TOTP secret** (operator-supplied, into SSM/env) — the Rust
client is built correct-by-construction against these docs + unit-tested
offline until those credentials are available.

## Planned native-Rust mapping (Groww second-feed PRs)

| PR | Slice | Groww doc / fact used |
|---|---|---|
| PR-2 | access-token auth (api_key + TOTP → access_token) | `01-introduction-auth.md` + access-token wire fact |
| PR-3 | NKey + socket-token + NATS connect + subscribe | socket-session + NATS facts |
| PR-4 | protobuf tick decode → WAL (`WsType::Groww`) + reconnect | tick-payload fact + `07-feed-websocket.md` |
| PR-5 | 1-minute aggregation → `groww_candles_1m` | ms timestamps |
| PR-6 | live-1m vs Groww **backtest** 1m exact parity | `08-master…` backtesting candles |
| PR-7 | functional per-feed enable/disable + observability + chaos | — |
