# Dhan V2 Forever Order — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/forever/
> **Extracted**: 2026-03-13

---

## 1. Overview

Good Till Triggered (GTT) orders. Two types: `SINGLE` (one trigger) and `OCO` (two triggers — whichever hits first).

| Method | Endpoint                        | Description          |
|--------|---------------------------------|----------------------|
| POST   | `/forever/orders`               | Create               |
| PUT    | `/forever/orders/{order-id}`    | Modify               |
| DELETE | `/forever/orders/{order-id}`    | Delete               |
| GET    | `/forever/orders`               | List all forever orders |

> **Updated (April 2026):** Dhan official docs now show `GET /forever/orders` for listing (not `/forever/all`). Both the SDK and official docs agree on this endpoint.

**Static IP required.** Product types: `CNC`, `MTF` only.

---

## 2. Create Forever Order

```json
{
    "dhanClientId": "1000000132",
    "correlationId": "",
    "orderFlag": "OCO",
    "transactionType": "BUY",
    "exchangeSegment": "NSE_EQ",
    "productType": "CNC",
    "orderType": "LIMIT",
    "validity": "DAY",
    "securityId": "1333",
    "quantity": 5,
    "disclosedQuantity": 1,
    "price": 1428,
    "triggerPrice": 1427,
    "price1": 1420,
    "triggerPrice1": 1419,
    "quantity1": 10
}
```

| Field          | Required     | Description                           |
|----------------|--------------|---------------------------------------|
| `orderFlag`    | Yes          | `SINGLE` or `OCO`                     |
| `price`        | Yes          | First leg price                       |
| `triggerPrice` | Yes          | First leg trigger                     |
| `price1`       | OCO only     | Second leg (target) price             |
| `triggerPrice1` | OCO only    | Second leg trigger                    |
| `quantity1`    | OCO only     | Second leg quantity                   |

> **SDK Note**: The Dhan API (Python SDK ref) (v2.0.2+) includes a `tradingSymbol` field (empty string `""`) in the forever order create request body. This field is not documented in the official API but may be accepted/ignored by the server.

---

## 3. Modify

`legName`: `TARGET_LEG` (single/first OCO leg), `STOP_LOSS_LEG` (second OCO leg)

---

## 4. Status Values

`TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`, `CONFIRM`

> `CONFIRM` = forever order is active and waiting for trigger.

---

## 2026-07-14 Upstream Update (runner-crawled live page)

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/forever/` (runs 1–3,
sha256 `91bc414b` content-identical, latest 2026-07-14T07:57:56Z). Full manifest:
`00-COVERAGE-MANIFEST.md`.

1. **The §1 April-2026 note is OVERSTATED — `/forever/all` persists on the live page:** the
   summary table does say `GET /forever/orders`, BUT the "All Forever Order Detail" section's
   curl still reads `https://api.dhan.co/v2/forever/all` (raw counts: `forever/orders` ×7,
   `forever/all` ×1). The page is internally split; `/forever/all` has NOT been removed from
   the official doc.
2. **List-response `orderType` is a REPURPOSED field carrying `SINGLE`/`OCO`** (the order
   FLAG, not LIMIT/MARKET) — verbatim "orderType | enum string | Order Type SINGLE OCO".
   Deserializers must not assume order-type literals there.
3. Live-doc typos/inconsistencies recorded: the create `exchangeSegment` enum literal lists
   "NSE_EQ NSE_FNO BSE_EQ NSE_FNO" (NSE_FNO twice, BSE_FNO absent — doc typo); the list
   response `productType` enum "CNC INTRADAY MARGIN CO BO" contradicts the create-side
   CNC/MTF (live-internal inconsistency — the create table is the binding one); the modify
   `orderType` enum adds STOP_LOSS/STOP_LOSS_MARKET (create side is LIMIT/MARKET only).
