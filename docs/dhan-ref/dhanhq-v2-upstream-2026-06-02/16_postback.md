# Postback (Webhooks) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/postback/ · API v2 · Audited 2 Jun 2026

Postback / Webhooks: Dhan sends an HTTP `POST` with a **JSON body** to your Postback URL whenever an order's status changes (`TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`) or whenever an order is modified or partially filled.

This is at the **access-token level** — all trades originating from a given access token are sent to that token's Webhook URL. Good for individual developers. (For all orders across a platform's users, use the Partner Login module.)

> This is a **push to your server** (you host the endpoint). For a pull/stream alternative see `11_live_order_update.md` (WebSocket). Conditional Trigger fires also deliver a postback if a URL is configured (`23_conditional_trigger.md`).

## Postback payload (raw HTTP POST body)
```json
{
  "dhanClientId": "1000000003",
  "orderId": "112111182198",
  "correlationId": "123abc678",
  "orderStatus": "PENDING",
  "transactionType": "BUY",
  "exchangeSegment": "NSE_EQ",
  "productType": "INTRADAY",
  "orderType": "MARKET",
  "validity": "DAY",
  "tradingSymbol": "",
  "securityId": "11536",
  "quantity": 5,
  "disclosedQuantity": 0,
  "price": 0.0,
  "triggerPrice": 0.0,
  "afterMarketOrder": false,
  "boProfitValue": 0.0,
  "boStopLossValue": 0.0,
  "legName": null,
  "createTime": "2021-11-24 13:33:03",
  "updateTime": "2021-11-24 13:33:03",
  "exchangeTime": "2021-11-24 13:33:03",
  "drvExpiryDate": null,
  "drvOptionType": null,
  "drvStrikePrice": 0.0,
  "omsErrorCode": null,
  "omsErrorDescription": null,
  "filled_qty": 1,
  "algoId": null
}
```

**Fields**
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `orderId` | string | Dhan order id |
| `correlationId` | string | Your tracking id; max 30 chars; allowed `[^a-zA-Z0-9 _-]` |
| `orderStatus` | enum string | `TRANSIT` `PENDING` `REJECTED` `CANCELLED` `TRADED` `EXPIRED` |
| `transactionType` | enum string | `BUY` `SELL` |
| `exchangeSegment` | enum string | `NSE_EQ` `NSE_FNO` `NSE_CURRENCY` `BSE_EQ` `MCX_COMM` |
| `productType` | enum string | `CNC` `INTRADAY` `MARGIN` `MTF` `CO` `BO` |
| `orderType` | enum string | `LIMIT` `MARKET` `STOP_LOSS` `STOP_LOSS_MARKET` |
| `validity` | enum string | `DAY` `IOC` |
| `tradingSymbol` | string | Trading symbol |
| `securityId` | string | From instrument master |
| `quantity` | int | Order quantity |
| `disclosedQuantity` | int | Disclosed (visible) qty |
| `price` | float | Order price |
| `triggerPrice` | float | For SL-M, SL-L, CO, BO |
| `afterMarketOrder` | boolean | Is AMO? |
| `boProfitValue` | float | BO target price change |
| `boStopLossValue` | float | BO stop-loss price change |
| `legName` | enum string | `ENTRY_LEG` `TARGET_LEG` `STOP_LOSS_LEG` (BO) |
| `createTime` | string | Order created time |
| `updateTime` | string | Last activity time |
| `exchangeTime` | string | Reached exchange time |
| `drvExpiryDate` | string | F&O contract expiry |
| `drvOptionType` | enum string | `CALL` `PUT` |
| `drvStrikePrice` | float | Option strike |
| `omsErrorCode` | string | Error code if rejected/failed |
| `omsErrorDescription` | string | Error description |
| `filled_qty` | int | Quantity already traded |
| `algoId` | string | Exchange-registered algo ID |

## Setting up Postback
- While generating the access token at https://web.dhan.co, enter your URL in the **Postback URL** field, then click **Generate**.
- **Important:** no postback calls are sent if the URL is `localhost` or `127.0.0.1`.
- For postback across all orders on a platform/app, use the Partner Login module.
