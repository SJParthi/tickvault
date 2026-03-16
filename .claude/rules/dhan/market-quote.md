# Dhan Market Quote REST Enforcement

> **Ground truth:** `docs/dhan-ref/11-market-quote-rest.md`
> **Scope:** Any file touching REST-based market quote snapshots (LTP, OHLC, full quote with depth).
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment, rate limits), `docs/dhan-ref/03-live-market-feed-websocket.md` (streaming alternative)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any market quote REST handler: `Read docs/dhan-ref/11-market-quote-rest.md`.

2. **Three endpoints — exact URLs.**
   - LTP: `POST /v2/marketfeed/ltp`
   - OHLC: `POST /v2/marketfeed/ohlc`
   - Full quote: `POST /v2/marketfeed/quote`

3. **Requires extra `client-id` header** in addition to `access-token` (same as Option Chain API). Missing `client-id` returns auth error.

4. **Rate limit: 1 request per second.** Unlimited per minute/hour/day.

5. **Up to 1000 instruments per request.**

6. **Request body is a HashMap.** Keys = exchange segment strings, values = arrays of SecurityId integers (NOT strings).
   ```json
   { "NSE_EQ": [11536], "NSE_FNO": [49081, 49082] }
   ```

7. **Response is nested HashMap.** `data[segment][security_id_string]`. Note: SecurityId is integer in request but STRING KEY in response.

8. **`last_trade_time` is a string**, NOT epoch. Format varies (seen `DD/MM/YYYY HH:MM:SS`). May show `01/01/1980 00:00:00` for untraded instruments.

9. **Depth in full quote is 5 levels** — same depth as Live Market Feed Full mode, but JSON not binary. Each level: `quantity`, `orders`, `price`.

10. **Use LTP mode when you only need price.** Saves bandwidth vs full quote.

11. **This is REST, not WebSocket.** Use for point-in-time snapshots. For continuous data, use Live Market Feed WebSocket.

## What This Prevents

- Missing `client-id` header → auth error on every request
- String SecurityId in request → request body expects integers
- Integer key lookup on response → key not found (response keys are strings)
- Rate limit violation → actual limit is 1/sec for quotes (not 5/sec)
- Using full quote when only LTP needed → wasted bandwidth and parsing

## Trigger

This rule activates when editing files matching:
- `crates/api/src/handlers/quote.rs`
- `crates/core/src/quote/*.rs`
- `crates/trading/src/oms/market_data.rs`
- Any file containing `MarketQuoteRequest`, `LtpResponse`, `LtpData`, `OhlcResponse`, `OhlcData`, `QuoteResponse`, `QuoteData`, `DepthData`, `DepthLevel`, `marketfeed/ltp`, `marketfeed/ohlc`, `marketfeed/quote`, `client-id`, `last_trade_time`
