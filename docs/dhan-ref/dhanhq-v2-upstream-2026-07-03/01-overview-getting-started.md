# DhanHQ API v2 — Overview & Getting Started

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/
> - https://docs.dhanhq.co/api/v2/guides/getting-started
> - https://docs.dhanhq.co/api/v2/trading-apis
> - https://docs.dhanhq.co/api/v2/data-apis
> - https://docs.dhanhq.co/api/v2/sandbox


---

## DhanHQ API

Introduction to DhanHQ Trading & Investment API

# Getting Started


DhanHQ API is a state-of-the-art platform that enables you to build trading and investment services, applications, and strategies.

It provides a set of REST-like APIs that integrate directly with our trading platform. With DhanHQ APIs, you can execute and modify orders in real time, manage portfolios, access live market data, and more through a fast and reliable API collection.

The platform uses resource-based URLs and supports JSON as well as form-encoded requests. Responses are returned in JSON format using standard HTTP response codes, verbs, and authentication methods.


  
  - [DhanHQ Python Client](https://github.com/dhan-oss/DhanHQ-py)


## Structure


#### REST

All **GET** and **DELETE** request parameters go as query parameters, and **POST** and **PUT** parameters as form-encoded. User has to input an access token in the header for every request.

```bash
curl --request POST \
  --url https://api.dhan.co/v2/ \
  --header 'Content-Type: application/json' \
  --header 'access-token: JWT' \
  --data '{Request JSON}'
```

#### Python

Install Python Package directly using following command in command line.

```bash
pip install dhanhq
```
This installs entire DhanHQ Python Client along with the required packages. Now, you can start using DhanHQ Client with your Python script.

You can now import 'dhanhq' module and connect to your Dhan account.

```bash
from dhanhq import dhanhq

dhan = dhanhq("client_id","access_token")
```


## Errors

Error responses come with the error code and message generated internally by the system. The sample structure of error response is shown below.

```json
{
    "errorType": "",
    "errorCode": "",
    "errorMessage": ""
}
```

You can find detailed error code and message under [Annexure](/api/v2/guides/annexure).


## Rate Limit

| Rate Limit | Order APIs | Data APIs | Quote APIs | Non Trading APIs |
|------------|------------|-----------|------------|------------------|
| **per second** | 10 | 5 | 1 | 20 |
| **per minute** | 250 | - | Unlimited | Unlimited |
| **per hour** | 1000 | - | Unlimited | Unlimited |
| **per day** | 7000 | 100000 | Unlimited | Unlimited |

> **info:** Order Modifications are capped at **25 modifications/order**.

---

## Getting Started

Get started with DhanHQ API for algorithmic trading

with DhanHQ API


DhanHQ API is a state-of-the-art platform that enables you to build trading and investment services, applications, and strategies.

It provides a set of REST-like APIs that integrate directly with our trading platform. With DhanHQ APIs, you can execute and modify orders in real time, manage portfolios, access live market data, and more through a fast and reliable API collection.

The platform uses resource-based URLs and supports JSON as well as form-encoded requests. Responses are returned in JSON format using standard HTTP response codes, verbs, and authentication methods.

## 1. Create an Account

Sign up at [dhan.co](https://dhan.co) and complete KYC verification.

## 2. Generate API Credentials

Navigate to your account settings to generate:
- **Client ID**: Your unique identifier
- **Access Token**: JWT authentication token for API requests

## 3. Install the SDK

```bash
pip install dhanhq
```

## 4. Initialize the Client

```python
from dhanhq import DhanContext, dhanhq

context = DhanContext("YOUR_CLIENT_ID", "YOUR_ACCESS_TOKEN")
dhan = dhanhq(context)
```

## 5. Place Your First Order

```python
response = dhan.place_order(
    security_id="1333",
    exchange_segment="NSE_EQ",
    transaction_type="BUY",
    quantity=10,
    order_type="MARKET",
    product_type="INTRADAY"
)
print(response)
```

## API Structure

All API requests follow REST conventions with JSON request/response bodies.

### Base URLs

| Environment | URL | Description |
|-------------|-----|-------------|
| **Production** | `https://api.dhan.co/v2` | Live trading environment |
| **Sandbox** | `https://sandbox.dhan.co/v2` | Testing environment |

### Request Headers

| Header | Required | Description |
|--------|----------|-------------|
| `access-token` | Yes | JWT access token from DhanHQ |
| `Content-Type` | Yes | `application/json` |

## What's Next?

- **[Authentication](/api/v2/guides/authentication)** — Learn about access tokens and security
- **[API Reference](/api/v2/)** — Browse all 31+ API endpoints
- **[Python SDK](/api/v2/guides/sdks/python)** — Full Python SDK reference
- **[Rate Limits](/api/v2/guides/rate-limits)** — Understand API rate limits

---

## Trading APIs

All endpoints for order placement, modification, portfolio management, and trading controls.

---

## Orders

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Place Order](/api/v2/orders/place-order) | Place a new order on the exchange |
| PUT | [Modify Order](/api/v2/orders/modify-order) | Modify a pending order |
| DELETE | [Cancel Order](/api/v2/orders/cancel-order) | Cancel a pending order |
| GET | [Get Order Book](/api/v2/orders/get-orders) | Retrieve all orders for the day |
| GET | [Get Order by ID](/api/v2/orders/get-order-by-id) | Get a specific order by order ID |
| GET | [Get Order by Correlation ID](/api/v2/orders/get-order-by-correlation-id) | Get order by partner correlation ID |
| POST | [Slice Order](/api/v2/orders/slice-order) | Place order with slicing for freeze limit |
| GET | [Get Trade Book](/api/v2/orders/get-trades) | Retrieve all trades for the day |
| GET | [Get Trades for Order](/api/v2/orders/get-trades-by-order-id) | Get trades for a specific order |

## Super Orders

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Place Super Order](/api/v2/super-orders/place-super-order) | Place a bracket order with entry, target, and stop-loss |
| PUT | [Modify Super Order](/api/v2/super-orders/modify-super-order) | Modify a pending super order leg |
| DELETE | [Cancel Super Order Leg](/api/v2/super-orders/cancel-super-order-leg) | Cancel a specific leg of a super order |
| GET | [Get Super Orders](/api/v2/super-orders/get-super-orders) | Retrieve all super orders |

## Conditional Triggers

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Place Conditional Trigger](/api/v2/conditional-triggers/place-conditional-trigger) | Create a new conditional trigger |
| PUT | [Modify Conditional Trigger](/api/v2/conditional-triggers/modify-conditional-trigger) | Modify an existing trigger |
| DELETE | [Delete Conditional Trigger](/api/v2/conditional-triggers/delete-conditional-trigger) | Delete a trigger |
| GET | [Get Conditional Trigger by ID](/api/v2/conditional-triggers/get-conditional-trigger-by-id) | Get a specific trigger |
| GET | [Get All Conditional Triggers](/api/v2/conditional-triggers/get-all-conditional-triggers) | Retrieve all triggers |

## Portfolio

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | [Get Holdings](/api/v2/portfolio/get-holdings) | Retrieve current holdings |
| GET | [Get Positions](/api/v2/portfolio/get-positions) | Retrieve current positions |
| POST | [Convert Position](/api/v2/portfolio/convert-position) | Convert position between product types |
| DELETE | [Exit All Positions](/api/v2/portfolio/exit-all-positions) | Exit all open positions |

## Trader's Control

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Manage Kill Switch](/api/v2/traders-control/manage-kill-switch) | Enable or disable kill switch |
| GET | [Kill Switch Status](/api/v2/traders-control/get-kill-switch-status) | Get current kill switch status |
| POST | [Configure P&L Exit](/api/v2/traders-control/configure-pnl-exit) | Set up P&L based exit |
| GET | [Get P&L Exit](/api/v2/traders-control/get-pnl-exit) | Get current P&L exit config |
| DELETE | [Stop P&L Exit](/api/v2/traders-control/stop-pnl-exit) | Stop P&L based exit |

## EDIS

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | [Generate T-PIN](/api/v2/edis/generate-tpin) | Generate T-PIN for EDIS |
| POST | [Generate eDIS Form](/api/v2/edis/generate-edis-form) | Generate eDIS authorization form |
| GET | [EDIS Status Inquiry](/api/v2/edis/edis-inquiry) | Check EDIS authorization status |

## Funds

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | [Get Fund Limits](/api/v2/funds/get-fund-limits) | Retrieve available fund limits |
| POST | [Calculate Margin](/api/v2/funds/calculate-margin) | Calculate margin for an order |
| POST | [Calculate Multi-Order Margin](/api/v2/funds/calculate-multi-margin) | Calculate margin for multiple orders |

---

## Data APIs

All endpoints for market data, historical charts, option chains, and account statements.

---

## Market Quote

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Get LTP](/api/v2/market-quote/get-ltp) | Get last traded price for instruments |
| POST | [Get OHLC](/api/v2/market-quote/get-ohlc) | Get open, high, low, close data |
| POST | [Get Full Quote](/api/v2/market-quote/get-quote) | Get complete market quote with depth |

## Historical Data

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Daily Historical Data](/api/v2/historical-data/get-daily-historical) | Get daily OHLC candles |
| POST | [Intraday Historical Data](/api/v2/historical-data/get-intraday-historical) | Get intraday candles (1min, 5min, etc.) |

## Option Chain

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Get Expiry List](/api/v2/option-chain/get-expiry-list) | Get available expiry dates |
| POST | [Get Option Chain](/api/v2/option-chain/get-option-chain) | Get full option chain data |

## Expired Options Data

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | [Historical Rolling Options Data](/api/v2/expired-options-data/get-expired-options-data) | Fetch expired options data with OHLC, IV, OI |

## Statements

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | [Trade History](/api/v2/statements/get-trade-history) | Get detailed trade history |
| GET | [Ledger Report](/api/v2/statements/get-ledger-report) | Get ledger/fund statement |

---

## Sandbox

Test DhanHQ API integrations in the sandbox environment

DhanHQ Sandbox is the testing environment for DhanHQ API integrations. Use it to validate request payloads, authentication handling, response parsing, and order-management workflows before pointing your application to the live API.

Sandbox requests use the sandbox base URL:

```text
https://sandbox.dhan.co/v2
```

Generate a sandbox access token from the DhanHQ developer portal at [developer.dhanhq.co](https://developer.dhanhq.co/). Use this sandbox token in the same `access-token` request header used by production APIs.

```bash
curl --request GET \
  --url https://sandbox.dhan.co/v2/orders \
  --header 'access-token: <your-sandbox-access-token>' \
  --header 'Content-Type: application/json'
```

Sandbox and production are separate environments:

- Use `https://sandbox.dhan.co/v2` with a sandbox token for testing.
- Use `https://api.dhan.co/v2` with a production access token for live API calls.
- Both of these environments can be accessed in the documentation for testing and live API calls.
- Do not reuse production tokens for sandbox requests or sandbox tokens for production requests.
- Keep the request method, path, headers, and payload structure the same as the documented endpoint unless the endpoint documentation states otherwise.

Only endpoints marked as sandbox-enabled in this documentation are available in the sandbox environment:

| Method | Endpoint | Path |
|--------|----------|------|
| POST | [Place Order](/api/v2/orders/place-order) | `/orders` |
| PUT | [Modify Order](/api/v2/orders/modify-order) | `/orders/{orderId}` |
| DELETE | [Cancel Order](/api/v2/orders/cancel-order) | `/orders/{order-id}` |
| GET | [Get Order Book](/api/v2/orders/get-orders) | `/orders` |
| GET | [Get Order by ID](/api/v2/orders/get-order-by-id) | `/orders/{order-id}` |
| GET | [Get Order by Correlation ID](/api/v2/orders/get-order-by-correlation-id) | `/orders/external/{correlation-id}` |
| POST | [Slice Order](/api/v2/orders/slice-order) | `/orders/slicing` |
| GET | [Get Trade Book](/api/v2/orders/get-trades) | `/trades` |
| GET | [Get Trades for Order](/api/v2/orders/get-trades-by-order-id) | `/trades/{order-id}` |
| GET | [Get Holdings](/api/v2/portfolio/get-holdings) | `/holdings` |
| GET | [Get Positions](/api/v2/portfolio/get-positions) | `/positions` |
| POST | [Convert Position](/api/v2/portfolio/convert-position) | `/positions/convert` |
| POST | [Manage Kill Switch](/api/v2/traders-control/manage-kill-switch) | `/killswitch` |
| GET | [Generate T-PIN](/api/v2/edis/generate-tpin) | `/edis/tpin` |
| POST | [Generate eDIS Form](/api/v2/edis/generate-edis-form) | `/edis/bulkform` |
| GET | [EDIS Status Inquiry](/api/v2/edis/edis-inquiry) | `/edis/inquire/{isin}` |
| GET | [Get Fund Limits](/api/v2/funds/get-fund-limits) | `/fundlimit` |
| POST | [Calculate Margin](/api/v2/funds/calculate-margin) | `/margincalculator` |
| GET | [Ledger Report](/api/v2/statements/get-ledger-report) | `/ledger` |
| GET | [Trade History](/api/v2/statements/get-trade-history) | `/trades/{from-date}/{to-date}/{page-number}` |
| POST | [Get Intraday Historical Data](/api/v2/historical-data/get-intraday-historical) | `/charts/intraday` |
| POST | [Get Daily Historical Data](/api/v2/historical-data/get-daily-historical) | `/charts/historical` |
