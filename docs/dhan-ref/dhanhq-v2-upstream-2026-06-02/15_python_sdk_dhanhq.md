# Python SDK — `dhanhq` (DhanHQ-py)

> Sources: https://pypi.org/project/dhanhq/ + https://github.com/dhan-oss/DhanHQ-py · Audited 2 Jun 2026

- **Latest version:** `2.2.0` (released 2026-04-24). Pre-2.1-refactor stable: `pip install dhanhq==2.0.2`.
- **License:** MIT. **Author:** Dhan (`dhan-oss@dhan.co`). **Homepage:** https://dhanhq.co/
- **Python docs:** https://dhanhq.co/docs/DhanHQ-py/
- **Runtime deps:** `pandas>=1.4.3`, `requests>=2.28.1`, `websockets>=12.0.1`, `pyOpenSSL>=20.0.1`.
- **Repo layout:** package under `src/dhanhq/`.

```bash
pip install dhanhq
```

## Version history (PyPI)
`1.0` (2022-09) · `1.1` · `1.2` · `1.2.1` · `1.2.2` · `1.2.3` · `1.3` · `1.3.2` · `1.3.3` (2024-10) · `2.0.0` (2024-10-25) · `2.0.1` · `2.0.2` (2024-11-29) · `2.1.0` (2025-03-12) · `2.2.0rc1` (2026-01-02) · `2.2.0` (2026-04-24).

## What's new (2.2.0)
Full Market Depth (200-level) in the library; expired-options data for NSE & BSE; Super Orders; IP set/modify/get from the library.

## Import / init changes (2.1.0, still current)
- Imports are flat: `from dhanhq import dhanhq, MarketFeed, OrderUpdate, FullDepth, DhanContext, DhanLogin`.
- Constants moved onto classes: `MarketFeed.NSE`, `MarketFeed.Ticker`, etc.
- Pass a **`DhanContext`** (defined once) into every class instead of raw `client_id`/`access_token` strings.

| Before | After |
|---|---|
| `from dhanhq import marketfeed.MarketFeed` | `from dhanhq import MarketFeed` |
| `from dhanhq import orderupdate.OrderUpdate` | `from dhanhq import OrderUpdate` |
| `marketfeed.NSE` | `MarketFeed.NSE` |
| `dhanhq('client_id','access_token')` | `dhanhq(dhan_context)` |
| `MarketFeed('client_id','access_token',instruments)` | `MarketFeed(dhan_context, instruments)` |
| `OrderUpdate('client_id','access_token')` | `OrderUpdate(dhan_context)` |

## Package exports (`src/dhanhq/__init__.py`)
`DhanContext`, `DhanLogin`, `DhanHTTP`, `Order`, `ForeverOrder`, `SuperOrder`, `Portfolio`, `Funds`, `Statement`, `TraderControl`, `Security`, `HistoricalData`, `OptionChain`, `MarketFeed`, `dhanhq`, `OrderUpdate`, `FullDepth`.

> Note: `MarketFeed` is bound twice — the internal REST-quote mixin and the WebSocket class. The WebSocket `marketfeed.MarketFeed` import is last, so **`MarketFeed` resolves to the WebSocket class**. The composite `dhanhq` still inherits REST quote methods (`ticker_data`/`ohlc_data`/`quote_data`) from the internal mixin.

## `DhanContext` — credentials holder
```python
DhanContext(client_id, access_token, disable_ssl=False, pool=None)
# .get_client_id() .get_access_token() .get_dhan_http() .get_dhan_login()
```

## `DhanLogin` — auth / token / IP
```python
DhanLogin(client_id)
  .generate_login_session(app_id, app_secret)        # OAuth: consent + open browser
  .consume_token_id(token_id, app_id, app_secret)    # OAuth: get access_token
  .generate_token(pin, totp)                         # PIN + TOTP flow
  .renew_token(access_token)
  .user_profile(access_token)
  .set_ip(access_token, ip, ip_flag, dhan_client_id=None)
  .modify_ip(access_token, ip, ip_flag, dhan_client_id=None)
  .get_ip(access_token, dhan_client_id=None)
```
**Auth examples**
```python
from dhanhq import DhanLogin
# OAuth
dhan_login = DhanLogin("YOUR_CLIENT_ID")
consent_id = dhan_login.generate_login_session(app_id, app_secret)   # opens browser
access_token = dhan_login.consume_token_id(token_id, app_id, app_secret)
# PIN + TOTP
access_token_data = dhan_login.generate_token(pin, totp)
# Renew / profile
dhan_login.renew_token(access_token)
dhan_login.user_profile(access_token)
# IP
dhan_login.set_ip(access_token, "10.200.10.10", "PRIMARY")
dhan_login.modify_ip(access_token, "10.200.10.11", "PRIMARY")
dhan_login.get_ip(access_token)
```

## `dhanhq` — core composite client
```python
from dhanhq import DhanContext, dhanhq
ctx = DhanContext("client_id", "access_token")
dhan = dhanhq(ctx)
```
**Class constants**
- Exchange: `NSE='NSE_EQ'`, `BSE='BSE_EQ'`, `CUR='NSE_CURRENCY'`, `MCX='MCX_COMM'`, `FNO='NSE_FNO'`, `NSE_FNO='NSE_FNO'`, `BSE_FNO='BSE_FNO'`, `INDEX='IDX_I'`
- Txn: `BUY='BUY'`, `SELL='SELL'`
- Product: `CNC='CNC'`, `INTRA='INTRADAY'`, `MARGIN='MARGIN'`, `CO='CO'`, `BO='BO'`, `MTF='MTF'`
- Order type: `LIMIT='LIMIT'`, `MARKET='MARKET'`, `SL='STOP_LOSS'`, `SLM='STOP_LOSS_MARKET'`
- Validity: `DAY='DAY'`, `IOC='IOC'`

**Data / market**
```python
dhan.historical_daily_data(security_id, exchange_segment, instrument_type, from_date, to_date, expiry_code=0, oi=False)
dhan.intraday_minute_data(security_id, exchange_segment, instrument_type, from_date, to_date, interval=1, oi=False)
dhan.expired_options_data(security_id, exchange_segment, instrument_type, expiry_flag, expiry_code,
                          strike, drv_option_type, required_data, from_date, to_date, interval=1)
dhan.option_chain(under_security_id, under_exchange_segment, expiry)
dhan.expiry_list(under_security_id, under_exchange_segment)
dhan.ticker_data(securities)     # LTP;  securities = {"NSE_EQ":[1333], ...}
dhan.ohlc_data(securities)       # OHLC
dhan.quote_data(securities)      # full quote
dhan.fetch_security_list(mode='compact', filename='security_id_list.csv')
dhan.convert_to_date_time(epoch) # epoch -> IST datetime/date (uses datetime.fromtimestamp -> Unix/1970)
```

**Orders / portfolio / funds**
```python
dhan.place_order(security_id, exchange_segment, transaction_type, quantity, order_type, product_type,
                 price, trigger_price=0, disclosed_quantity=0, after_market_order=False,
                 validity='DAY', amo_time='OPEN', bo_profit_value=None, bo_stop_loss_Value=None,
                 tag=None, should_slice=False)
dhan.place_slice_order(...)                       # same args minus should_slice
dhan.modify_order(order_id, order_type, leg_name, quantity, price, trigger_price, disclosed_quantity, validity)
dhan.cancel_order(order_id)
dhan.get_order_list()
dhan.get_order_by_id(order_id)
dhan.get_order_by_correlationID(correlation_id)
dhan.get_trade_book(order_id)
dhan.get_trade_history(from_date, to_date, page_number=0)
dhan.get_positions()
dhan.get_holdings()
dhan.convert_position(from_product_type, exchange_segment, position_type, security_id, convert_qty, to_product_type)
dhan.get_fund_limits()
dhan.margin_calculator(security_id, exchange_segment, transaction_type, quantity, product_type, price, trigger_price=0)
```

**Super Orders**
```python
dhan.place_super_order(security_id, exchange_segment, transaction_type, quantity, order_type, product_type,
                       price, targetPrice=0.0, stopLossPrice=0.0, trailingJump=0.0, tag=None)
dhan.modify_super_order(order_id, order_type, leg_name, quantity=0, price=0.0, targetPrice=0.0,
                        stopLossPrice=0.0, trailingJump=0.0)
dhan.cancel_super_order(order_id, order_leg)
dhan.get_super_order_list()
```

**Forever Orders**
```python
dhan.place_forever(security_id, exchange_segment, transaction_type, product_type, order_type, quantity,
                   price, trigger_Price, order_flag="SINGLE", disclosed_quantity=0, validity='DAY',
                   price1=0, trigger_Price1=0, quantity1=0, tag=None, symbol="")
dhan.modify_forever(order_id, order_flag, order_type, leg_name, quantity, price, trigger_price, disclosed_quantity, validity)
dhan.get_forever()
dhan.cancel_forever(order_id)
```

**eDIS** (to sell holdings via CDSL eDIS)
```python
dhan.generate_tpin()
dhan.open_browser_for_tpin(isin, qty, exchange, segment='EQ', bulk=False)
dhan.edis_inquiry(isin)
```

## `MarketFeed` — live feed WebSocket
```python
from dhanhq import DhanContext, MarketFeed
ctx = DhanContext("client_id", "access_token")
instruments = [(MarketFeed.NSE, "1333", MarketFeed.Ticker),
               (MarketFeed.NSE, "1333", MarketFeed.Quote),
               (MarketFeed.NSE, "1333", MarketFeed.Full)]
feed = MarketFeed(ctx, instruments, version="v2")
feed.run_forever()
while True:
    print(feed.get_data())

feed.subscribe_symbols([(MarketFeed.NSE, "14436", MarketFeed.Ticker)])
feed.unsubscribe_symbols([(MarketFeed.NSE, "1333", 16)])   # 16 = unsubscribe ticker
feed.close_connection()
```
- Constructor: `MarketFeed(dhan_context, instruments, version='v2', on_connect=None, on_message=None, on_close=None, on_error=None, on_ticks=None)`.
- Tuple: `(exchange_segment_int, "security_id", subscription_type)`; omit type → Ticker default.
- **Constants** — Exchange: `IDX=0, NSE=1, NSE_FNO=2, NSE_CURR=3, BSE=4, MCX=5, BSE_CURR=7, BSE_FNO=8`. Subscription: `Ticker=15, Quote=17, Depth=19, Full=21`.

## `FullDepth` — 20/200-level depth WebSocket
```python
from dhanhq import DhanContext, FullDepth
ctx = DhanContext(client_id, access_token)
instruments = [(1, "1333")]                 # (exchange_segment_int, security_id); up to 50 for 20-depth
depth = FullDepth(ctx, instruments, depth_level=200)   # 20 or 200 (default 20)
depth.run_forever()
while True:
    depth.get_data()
    if depth.on_close:
        print("Server disconnection detected. Kindly try again.")
        break
```
- Constructor: `FullDepth(dhan_context, instruments, depth_level=20)`. Constants: `NSE=1, NSE_FNO=2`. Methods: `subscribe_symbols`, `unsubscribe_symbols`, `close_connection`.

## `OrderUpdate` — live order updates WebSocket
```python
from dhanhq import DhanContext, OrderUpdate
import time
ctx = DhanContext("client_id", "access_token")

def on_order_update(order_data: dict):
    print(order_data["Data"])

client = OrderUpdate(ctx)
client.on_update = on_order_update          # optional callback
while True:
    try:
        client.connect_to_dhan_websocket_sync()
    except Exception as e:
        print(f"reconnecting in 5s: {e}")
        time.sleep(5)
```
- Constructor: `OrderUpdate(dhan_context)`. Methods: `connect_to_dhan_websocket_sync()`, `handle_order_update(order_update)`, plus assignable `on_update`.

## `DhanHTTP` — low-level HTTP (rarely used directly)
- Constants: `API_BASE_URL='https://api.dhan.co/v2'`, `HTTP_DEFAULT_TIME_OUT=60`.
- Methods: `get(endpoint)`, `post(endpoint, payload)`, `put(endpoint, payload)`, `delete(endpoint)`.

> The composite `dhanhq` also inherits `Statement` (ledger) and `TraderControl` (kill switch / PnL exit) mixins. Changelog: https://github.com/dhan-oss/DhanHQ-py/releases
