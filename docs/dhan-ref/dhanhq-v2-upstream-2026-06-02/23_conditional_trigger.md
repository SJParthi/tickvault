# Conditional Trigger (Alert Orders) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/conditional-trigger/ · API v2 · Audited 2 Jun 2026 · *(introduced in v2.5)*

Place order(s) automatically when a condition is met — based on price, technical indicators, or a combination. One condition can trigger one or multiple orders.

| Method | Path | Use |
|---|---|---|
| POST | `/alerts/orders` | Place conditional trigger |
| PUT | `/alerts/orders/{alertId}` | Modify conditional trigger |
| DELETE | `/alerts/orders/{alertId}` | Delete conditional trigger |
| GET | `/alerts/orders/{alertId}` | Get conditional trigger by ID |
| GET | `/alerts/orders` | Get all conditional triggers |

> - Supported only for **Equities and Indices** currently.
> - On trigger, you get a Postback update if a Webhook URL is configured (`16_postback.md`).
> - Enum reference (comparison type, indicator name, operator, status): `08_annexure.md`.

## Place — `POST /alerts/orders`
```bash
curl --request POST --url https://api.dhan.co/v2/alerts/orders \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: ' --data '{Request Body}'
```
**Request**
```json
{
  "dhanClientId": "123456789",
  "condition": {
    "comparisonType": "TECHNICAL_WITH_VALUE",
    "exchangeSegment": "NSE_EQ",
    "securityId": "12345",
    "indicatorName": "SMA_5",
    "timeFrame": "DAY",
    "operator": "CROSSING_UP",
    "comparingValue": 250,
    "expDate": "2019-08-24",
    "frequency": "ONCE",
    "userNote": "Price crossing SMA"
  },
  "orders": [
    {
      "transactionType": "BUY",
      "exchangeSegment": "NSE_EQ",
      "productType": "CNC",
      "orderType": "LIMIT",
      "securityId": "12345",
      "quantity": 10,
      "validity": "DAY",
      "price": "250.00",
      "discQuantity": "0",
      "triggerPrice": "0"
    }
  ]
}
```

**`condition` object**
| Field | Type | Req | Description |
|---|---|---|---|
| `comparisonType` | string | ✓ | See Comparison Type in `08_annexure.md` |
| `timeFrame` | string | ✓ | `DATE`/`DAY`, `ONE_MIN`, `FIVE_MIN`, `FIFTEEN_MIN` |
| `exchangeSegment` | enum | ✓ | `NSE_EQ` `BSE_EQ` `IDX_I` |
| `securityId` | string | ✓ | From instrument master |
| `indicatorName` | string | cond. | See Indicator Name in `08_annexure.md` (e.g. `SMA_5`) |
| `operator` | string | ✓ | See Operator in `08_annexure.md` (e.g. `CROSSING_UP`) |
| `comparingValue` | number | cond. | Value the indicator/price is compared against |
| `comparingIndicatorName` | string | cond. | Second indicator (for `TECHNICAL_WITH_INDICATOR`) |
| `expDate` | string (date) | ✓ | Alert expiry; default 1 year |
| `frequency` | string | ✓ | Trigger frequency (e.g. `ONCE`) |
| `userNote` | string | — | User note |

**`orders[]` array (orders to execute on trigger)**
| Field | Type | Req | Description |
|---|---|---|---|
| `transactionType` | enum | ✓ | `BUY` `SELL` |
| `exchangeSegment` | enum | ✓ | See `08_annexure.md` |
| `productType` | enum | ✓ | `CNC` `INTRADAY` `MARGIN` `MTF` |
| `orderType` | enum | ✓ | `LIMIT` `MARKET` `STOP_LOSS` `STOP_LOSS_MARKET` |
| `securityId` | string | ✓ | From instrument master |
| `quantity` | int | ✓ | Order quantity |
| `validity` | enum | ✓ | `DAY` `IOC` |
| `price` | string | ✓ | Order price |
| `discQuantity` | string | — | Disclosed qty (keep > 30% of quantity) |
| `triggerPrice` | string | cond. | For SL-M & SL-L |

**Response**
```json
{ "alertId": "12345", "alertStatus": "ACTIVE" }
```
| Field | Type | Description |
|---|---|---|
| `alertId` | string | Unique ID of the created trigger |
| `alertStatus` | string | See Status in `08_annexure.md` (`ACTIVE` `TRIGGERED` `EXPIRED` `CANCELLED`) |

## Modify — `PUT /alerts/orders/{alertId}`
Modify the condition and/or order parameters. Same `condition`/`orders` schema as Place, plus `alertId` in the body.
```bash
curl --request PUT --url https://api.dhan.co/v2/alerts/orders/{alertId} \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: ' --data '{Request Body}'
```
**Response:** `{ "alertId": "12345", "alertStatus": "ACTIVE" }`

## Delete — `DELETE /alerts/orders/{alertId}`
No body.
**Response:** `{ "alertId": "12345", "alertStatus": "CANCELLED" }`

## Get by ID — `GET /alerts/orders/{alertId}`
No body. Returns status + full config.
**Response**
```json
{
  "alertId": "12345",
  "alertStatus": "ACTIVE",
  "createdTime": "2019-08-24T14:15:22Z",
  "triggeredTime": null,
  "lastPrice": "245.50",
  "condition": { "...": "as in Place" },
  "orders": [ { "...": "as in Place" } ]
}
```
Adds, over the Place schema: `createdTime` (string), `triggeredTime` (string|null), `lastPrice` (string).

## Get All — `GET /alerts/orders`
No body. Returns an array of the Get-by-ID structure.

For enum descriptions, see `08_annexure.md`.
