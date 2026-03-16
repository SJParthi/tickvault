# Dhan V2 Postback (Webhooks) — Reference

> **Source**: https://dhanhq.co/docs/v2/postback/
> **Extracted**: 2026-03-13

---

## Overview

HTTP POST webhook with JSON payload on every order status change. Tied to **access token** — all orders from one token go to one webhook URL.

**Setup**: Enter Postback URL when generating access token on web.dhan.co. Cannot use `localhost` or `127.0.0.1`.

---

## Payload Structure

```json
{
    "dhanClientId": "1000000003",
    "orderId": "112111182198",
    "correlationId": "123abc678",
    "orderStatus": "TRADED",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "INTRADAY",
    "orderType": "MARKET",
    "validity": "DAY",
    "securityId": "11536",
    "quantity": 5,
    "price": 0.0,
    "triggerPrice": 0.0,
    "afterMarketOrder": false,
    "boProfitValue": 0.0,
    "boStopLossValue": 0.0,
    "legName": null,
    "createTime": "2021-11-24 13:33:03",
    "updateTime": "2021-11-24 13:33:03",
    "exchangeTime": "2021-11-24 13:33:03",
    "omsErrorCode": null,
    "omsErrorDescription": null,
    "filled_qty": 1,
    "algoId": null
}
```

Triggered on: `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`, modification, partial fill.

---

## Notes

1. **Token-scoped** — one webhook URL per access token.
2. **No localhost** — must be a publicly accessible URL.
3. **`filled_qty`** — note the snake_case (inconsistent with camelCase elsewhere).
4. **`legName`** — populated for Super Order legs (`ENTRY_LEG`, `TARGET_LEG`, `STOP_LOSS_LEG`).
5. **Alternative**: Use Live Order Update WebSocket (`10-live-order-update-websocket.md`) for real-time updates without needing a public URL.
