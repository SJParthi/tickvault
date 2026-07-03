# DhanHQ API v2 — Conditional Triggers (Price / Technical Indicator Based)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/conditional-triggers
> - https://docs.dhanhq.co/api/v2/guides/conditional-trigger
> - https://docs.dhanhq.co/api/v2/conditional-triggers/place-conditional-trigger
> - https://docs.dhanhq.co/api/v2/conditional-triggers/modify-conditional-trigger
> - https://docs.dhanhq.co/api/v2/conditional-triggers/delete-conditional-trigger
> - https://docs.dhanhq.co/api/v2/conditional-triggers/get-conditional-trigger-by-id
> - https://docs.dhanhq.co/api/v2/conditional-triggers/get-all-conditional-triggers


---

## Conditional Trigger

Conditional Trigger

When the conditional order is triggered, you will receive a postback update if set up [here](/api/v2/guides/postback).

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/alerts/orders` | Place Conditional Trigger |
| PUT | `/alerts/orders/{alertId}` | Modify Conditional Trigger |
| DELETE | `/alerts/orders/{alertId}` | Delete Conditional Trigger |
| GET | `/alerts/orders/{alertId}` | Get Conditional Trigger by ID |
| GET | `/alerts/orders` | Get All Conditional Triggers |

> **note:** - Conditional Triggers are currently supported only for Equities and Indices.
> - You can receive a postback update by providing a Webhook URL ([here](/api/v2/guides/postback)) while generating the Access Token.

---

## Conditional Trigger

Place, modify, delete and retrieve conditional trigger orders based on price or technical indicators

The Conditional Trigger API lets you place orders based on set conditions. These conditions can be based on price or technical indicators or a combination of both. You can set one or multiple orders to be triggered when the condition is met.

When a conditional order is triggered, you will receive a postback update if [configured](/api/v2/guides/postback).

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/alerts/orders` | Place Conditional Trigger |
| PUT | `/alerts/orders/{alertId}` | Modify Conditional Trigger |
| DELETE | `/alerts/orders/{alertId}` | Delete Conditional Trigger |
| GET | `/alerts/orders/{alertId}` | Get Conditional Trigger by ID |
| GET | `/alerts/orders` | Get All Conditional Triggers |

> **note:** - Conditional Triggers are currently supported only for Equities and Indices.
> - You can receive postback updates by providing a Webhook URL while generating the Access Token.

---

## Place Conditional Trigger

Create a new conditional trigger with conditions (price or technical indicators) that, when met, place one or multiple orders automatically.

```bash
curl --request POST \
  --url https://api.dhan.co/v2/alerts/orders \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: {JWT}' \
  --data '{Request Body}'
```

**Request Structure**

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


| Parameter | Data Type | Description | Sample Value |
|-----------|-----------|-------------|--------------|
| `condition`* | object | Alert condition configuration | — |
| `condition.comparisonType`* | string | Type of comparison ([see Annexure](/api/v2/guides/annexure#comparison-type)) | `TECHNICAL_WITH_VALUE` |
| `condition.timeframe`* | string | Timeframe for indicator evaluation — `DATE` `ONE_MIN` `FIVE_MIN` `FIFTEEN_MIN` | `DAY` |
| `condition.exchangeSegment`* | enum | Exchange — `NSE_EQ` `BSE_EQ` `IDX_I` | `NSE_EQ` |
| `condition.securityId`* | string | Exchange standard ID for scrip ([Instruments](/api/v2/guides/instruments)) | `12345` |
| `condition.indicatorName`** | string | Technical indicator ([see Annexure](/api/v2/guides/annexure#indicator-name)) | `SMA_5` |
| `condition.operator`* | string | Condition operator ([see Annexure](/api/v2/guides/annexure#operator)) | `CROSSING_UP` |
| `condition.comparingValue`** | number | Value for comparison | `250` |
| `condition.comparingIndicatorName`** | string | Technical indicator ([see Annexure](/api/v2/guides/annexure#indicator-name)) | `SMA_10` |
| `condition.expDate`* | string (date) | Expiry date of alert (default: 1 year) | `2019-08-24` |
| `condition.frequency`* | string | Trigger frequency | `ONCE` |
| `condition.userNote` | string | User-provided note | `Price crossing SMA` |
| `orders` | array[obj] | List of orders to execute when triggered | — |
| `orders.transactionType`* | enum | `BUY` or `SELL` | `BUY` |
| `orders.exchangeSegment`* | enum | Exchange Segment ([see Annexure](/api/v2/guides/annexure#exchange-segment)) | `NSE_EQ` |
| `orders.productType`* | enum | `CNC` `INTRADAY` `MARGIN` `MTF` | `CNC` |
| `orders.orderType`* | enum | `LIMIT` `MARKET` `STOP_LOSS` `STOP_LOSS_MARKET` | `LIMIT` |
| `orders.securityId`* | string | Exchange standard ID ([Instruments](/api/v2/guides/instruments)) | `12345` |
| `orders.quantity`* | integer | Number of shares | `10` |
| `orders.validity`* | enum | `DAY` or `IOC` | `DAY` |
| `orders.price`* | string | Price at which order is placed | `250` |
| `orders.discQuantity` | string | Disclosed quantity (≥30% of quantity) | `0` |
| `orders.triggerPrice`** | string | Trigger price for SL-M & SL-L | `0` |

**Response**

```json
{
  "alertId": "12345",
  "alertStatus": "ACTIVE"
}
```

---

## Modify Conditional Trigger

Modify a conditional trigger logic and/or the associated order execution parameters.

```bash
curl --request PUT \
  --url https://api.dhan.co/v2/alerts/orders/{alertId} \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: {JWT}' \
  --data '{Request Body}'
```

The request structure and parameters are the same as [Place Conditional Trigger](#place-conditional-trigger), with the addition of `alertId` as a path parameter.

**Response**

```json
{
  "alertId": "12345",
  "alertStatus": "ACTIVE"
}
```

---

## Delete Conditional Trigger

Delete an existing conditional trigger using its `alertId`.

```bash
curl --request DELETE \
  --url https://api.dhan.co/v2/alerts/orders/{alertId} \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Response**

```json
{
  "alertId": "12345",
  "alertStatus": "CANCELLED"
}
```

---

## Get Conditional Trigger by ID

Retrieve status and details for a specific trigger by its `alertId`.

```bash
curl --request GET \
  --url https://api.dhan.co/v2/alerts/orders/{alertId} \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Response**

```json
{
  "alertId": "12345",
  "alertStatus": "ACTIVE",
  "createdTime": "2019-08-24T14:15:22Z",
  "triggeredTime": null,
  "lastPrice": "245.50",
  "condition": {
    "comparisonType": "TECHNICAL_WITH_VALUE",
    "exchangeSegment": "NSE_EQ",
    "securityId": "12345",
    "indicatorName": "SMA_5",
    "timeFrame": "DAY",
    "operator": "CROSSING_UP",
    "comparingValue": "250.00",
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

**Additional Response Fields**

| Parameter | Data Type | Description |
|-----------|-----------|-------------|
| `alertId` | string | Unique identifier of the alert |
| `alertStatus` | string | Status ([see Annexure](/api/v2/guides/annexure#status)) |
| `createdTime` | string | Timestamp when alert was created |
| `triggeredTime` | string | Timestamp when alert was triggered |
| `lastPrice` | string | Last price of the instrument |

---

## Get All Conditional Triggers

Retrieve all conditional triggers for the authenticated account.

```bash
curl --request GET \
  --url https://api.dhan.co/v2/alerts/orders \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Response** — Returns an array of trigger objects with the same structure as [Get by ID](#get-conditional-trigger-by-id).

For description of enum values, refer to the [Annexure](/api/v2/guides/annexure).

---

## Conditional Triggers

### Comparison Type

| Type | Description | Mandatory Fields |
|------|-------------|------------------|
| `TECHNICAL_WITH_VALUE` | Compare technical indicator against a fixed numeric value | `indicatorName` `operator` `timeFrame` `comparingValue` |
| `TECHNICAL_WITH_INDICATOR` | Compare technical indicator against another indicator | `indicatorName` `operator` `timeFrame` `comparingIndicatorName` |
| `TECHNICAL_WITH_CLOSE` | Compare a technical indicator with closing price | `indicatorName` `operator` `timeFrame` |
| `PRICE_WITH_VALUE` | Compare market price against fixed value | `operator` `comparingValue` |

### Indicator Name

| Indicator | Description |
|-----------|-------------|
| `SMA_5` | Simple Moving Average (5 periods) |
| `SMA_10` | Simple Moving Average (10 periods) |
| `SMA_20` | Simple Moving Average (20 periods) |
| `SMA_50` | Simple Moving Average (50 periods) |
| `SMA_100` | Simple Moving Average (100 periods) |
| `SMA_200` | Simple Moving Average (200 periods) |
| `EMA_5` | Exponential Moving Average (5 periods) |
| `EMA_10` | Exponential Moving Average (10 periods) |
| `EMA_20` | Exponential Moving Average (20 periods) |
| `EMA_50` | Exponential Moving Average (50 periods) |
| `EMA_100` | Exponential Moving Average (100 periods) |
| `EMA_200` | Exponential Moving Average (200 periods) |
| `BB_UPPER` | Upper Bollinger Band |
| `BB_LOWER` | Lower Bollinger Band |
| `RSI_14` | Relative Strength Index |
| `ATR_14` | Average True Range |
| `STOCHASTIC` | Stochastic Oscillator |
| `STOCHRSI_14` | Stochastic RSI |
| `MACD_26` | MACD long-term component |
| `MACD_12` | MACD short-term component |
| `MACD_HIST` | MACD histogram |

### Operator

| Operator | Description |
|----------|-------------|
| `CROSSING_UP` | Crosses above |
| `CROSSING_DOWN` | Crosses below |
| `CROSSING_ANY_SIDE` | Crosses either side |
| `GREATER_THAN` | Greater than |
| `LESS_THAN` | Less than |
| `GREATER_THAN_EQUAL` | Greater than or equal |
| `LESS_THAN_EQUAL` | Less than or equal |
| `EQUAL` | Equal |
| `NOT_EQUAL` | Not equal |

### Status

| Status | Description |
|--------|-------------|
| `ACTIVE` | Alert is currently active |
| `TRIGGERED` | Alert condition met |
| `EXPIRED` | Alert expired |
| `CANCELLED` | Alert cancelled |

---

## Place Conditional Trigger

`POST` https://api.dhan.co/v2/alerts/orders

Create a new conditional trigger with conditions (price or technical indicators) that, when met, place one or multiple orders automatically.

Using this API, you can create a new conditional trigger wherein you define conditions (price or technical indicators) that, when met, place one or multiple orders automatically on the user's Dhan account. It supports multiple combinations of indicators and operators.


> **NOTICE:** Order Placement, Modification and Cancellation APIs requires Static IP whitelisting - [refer here](/api/v2/guides/authentication#setup-static-ip)


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| dhanClientId | string | Yes | User specific identification generated by Dhan |
| condition.comparisonType | enum | Yes | Type of comparison for the condition (see Annexure) Values: `TECHNICAL_WITH_VALUE`, `TECHNICAL_WITH_INDICATOR`, `TECHNICAL_WITH_CLOSE`, `PRICE_WITH_VALUE`. Default: `TECHNICAL_WITH_VALUE`. |
| condition.exchangeSegment | enum | Yes | Exchange Segment for the condition (see Annexure) Values: `NSE_EQ`, `BSE_EQ`, `IDX_I`. |
| condition.securityId | string | Yes | Exchange standard ID for each scrip (>= 0 characters, <= 20 characters). refer here |
| condition.indicatorName | string | No | Name of the technical indicator (see Annexure) |
| condition.timeFrame | enum | Yes | Timeframe for the indicator Values: `DAY`, `ONE_MIN`, `FIVE_MIN`, `FIFTEEN_MIN`. Default: `DAY`. |
| condition.operator | enum | Yes | Operator for the condition (see Annexure) Values: `CROSSING_UP`, `CROSSING_DOWN`, `CROSSING_ANY_SIDE`, `GREATER_THAN`, `LESS_THAN`, `GREATER_THAN_EQUAL`, `LESS_THAN_EQUAL`, `EQUAL`, `NOT_EQUAL`. Default: `CROSSING_UP`. |
| condition.comparingValue | float | No | Value to compare against (for TECHNICAL_WITH_VALUE) |
| condition.comparingIndicatorName | string | No | The technical indicator to compare against when using TECHNICAL_WITH_INDICATOR (see Annexure) |
| condition.expDate | string | Yes | Expiry date of the conditional trigger (YYYY-MM-DD) |
| condition.frequency | enum | Yes | Evaluation frequency Values: `ONCE`. Default: `ONCE`. |
| condition.userNote | string | No | Custom note for the condition |
| orders[0].transactionType | enum | Yes | The trading side of transaction Values: `BUY`, `SELL`. Default: `BUY`. |
| orders[0].exchangeSegment | enum | Yes | Exchange Segment for the order (see Annexure) Values: `NSE_EQ`, `NSE_FNO`, `NSE_COMM`, `BSE_EQ`, `BSE_FNO`, `MCX_COMM`. |
| orders[0].productType | enum | Yes | Product type Values: `CNC`, `INTRADAY`, `MARGIN`, `MTF`. Default: `CNC`. |
| orders[0].orderType | enum | Yes | Order Type Values: `LIMIT`, `MARKET`, `STOP_LOSS`, `STOP_LOSS_MARKET`. Default: `LIMIT`. |
| orders[0].securityId | string | Yes | Exchange standard ID for each scrip. refer here |
| orders[0].quantity | int | Yes | Number of shares for the order |
| orders[0].validity | enum | Yes | Validity of order Values: `DAY`, `IOC`. Default: `DAY`. |
| orders[0].price | string | Yes | Price at which order is placed (e.g. "250.5") |
| orders[0].discQuantity | string | No | Disclosed quantity (e.g. "100") |
| orders[0].triggerPrice | string | No | Price at which order is triggered (e.g. "2500.0") |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Conditional trigger created |
| 400 | Bad Request |
| 401 | Unauthorized |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/alerts/orders \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```

---

## Modify Conditional Trigger

`PUT` https://api.dhan.co/v2/alerts/orders/{alertId}

Modify a conditional trigger

> **NOTICE:** Order Placement, Modification and Cancellation APIs requires Static IP whitelisting - [refer here](/api/v2/guides/authentication#setup-static-ip)


### Path Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| alertId | string | Yes | Alert specific identification generated by Dhan |


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| dhanClientId | string | Yes | User specific identification generated by Dhan |
| alertId | string | No | Alert specific identification generated by Dhan |
| condition.comparisonType | enum | Yes | Type of comparison for the condition (see Annexure) Values: `TECHNICAL_WITH_VALUE`, `TECHNICAL_WITH_INDICATOR`, `TECHNICAL_WITH_CLOSE`, `PRICE_WITH_VALUE`. Default: `TECHNICAL_WITH_VALUE`. |
| condition.exchangeSegment | enum | Yes | Exchange Segment for the condition (see Annexure) Values: `NSE_EQ`, `BSE_EQ`, `IDX_I`. |
| condition.securityId | string | Yes | Exchange standard ID for each scrip (>= 0 characters, <= 20 characters). refer here |
| condition.indicatorName | string | No | Name of the technical indicator (see Annexure) |
| condition.timeFrame | enum | Yes | Timeframe for the indicator Values: `DAY`, `ONE_MIN`, `FIVE_MIN`, `FIFTEEN_MIN`. Default: `DAY`. |
| condition.operator | enum | Yes | Operator for the condition (see Annexure) Values: `CROSSING_UP`, `CROSSING_DOWN`, `CROSSING_ANY_SIDE`, `GREATER_THAN`, `LESS_THAN`, `GREATER_THAN_EQUAL`, `LESS_THAN_EQUAL`, `EQUAL`, `NOT_EQUAL`. Default: `CROSSING_UP`. |
| condition.comparingValue | float | No | Value to compare against (for TECHNICAL_WITH_VALUE) |
| condition.comparingIndicatorName | string | No | The technical indicator to compare against when using TECHNICAL_WITH_INDICATOR (see Annexure) |
| condition.expDate | string | Yes | Expiry date of the conditional trigger (YYYY-MM-DD) |
| condition.frequency | enum | Yes | Evaluation frequency Values: `ONCE`. Default: `ONCE`. |
| condition.userNote | string | No | Custom note for the condition |
| orders[0].transactionType | enum | Yes | The trading side of transaction Values: `BUY`, `SELL`. Default: `BUY`. |
| orders[0].exchangeSegment | enum | Yes | Exchange Segment for the order (see Annexure) Values: `NSE_EQ`, `NSE_FNO`, `NSE_COMM`, `BSE_EQ`, `BSE_FNO`, `MCX_COMM`. |
| orders[0].productType | enum | Yes | Product type Values: `CNC`, `INTRADAY`, `MARGIN`, `MTF`. Default: `CNC`. |
| orders[0].orderType | enum | Yes | Order Type Values: `LIMIT`, `MARKET`, `STOP_LOSS`, `STOP_LOSS_MARKET`. Default: `LIMIT`. |
| orders[0].securityId | string | Yes | Exchange standard ID for each scrip. refer here |
| orders[0].quantity | int | Yes | Number of shares for the order |
| orders[0].validity | enum | Yes | Validity of order Values: `DAY`, `IOC`. Default: `DAY`. |
| orders[0].price | string | Yes | Price at which order is placed (e.g. "250.5") |
| orders[0].discQuantity | string | No | Disclosed quantity (e.g. "100") |
| orders[0].triggerPrice | string | No | Price at which order is triggered (e.g. "2500.0") |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| alertId | string | No | Alert specific identification generated by Dhan |
| alertStatus | enum string | No | Last updated alert status ([see Annexure](/api/v2/guides/annexure#status)) Values: `ACTIVE`, `TRIGGERED`, `EXPIRED`, `CANCELLED`. |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Conditional trigger modified |
| 400 | Bad Request |
| 401 | Unauthorized |


### Example Request

```bash
curl -X PUT https://api.dhan.co/v2/alerts/orders/{alertId} \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```

---

## Delete Conditional Trigger

`DELETE` https://api.dhan.co/v2/alerts/orders/{alertId}

Delete an existing conditional trigger.

> **NOTICE:** Order Placement, Modification and Cancellation APIs requires Static IP whitelisting - [refer here](/api/v2/guides/authentication#setup-static-ip)


### Path Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| alertId | string | Yes | Alert specific identification generated by Dhan |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| alertId | string | No | Alert specific identification generated by Dhan |
| alertStatus | enum string | No | Last updated alert status ([see Annexure](/api/v2/guides/annexure#status)) Values: `ACTIVE`, `TRIGGERED`, `EXPIRED`, `CANCELLED`. |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Conditional trigger deleted |
| 401 | Unauthorized |


### Example Request

```bash
curl -X DELETE https://api.dhan.co/v2/alerts/orders/{alertId} \
  -H "access-token: <your-access-token>"
```

---

## Get Conditional Trigger by ID

`GET` https://api.dhan.co/v2/alerts/orders/{alertId}

Retrieve status and details for a specific conditional trigger.

Retrieve the status and detailed conditional triggers for a specific trigger by its unique identification (alertId).


### Path Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| alertId | string | Yes | Alert specific identification generated by Dhan |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Conditional trigger details |
| 401 | Unauthorized |


### Example Request

```bash
curl -X GET https://api.dhan.co/v2/alerts/orders/{alertId} \
  -H "access-token: <your-access-token>"
```

---

## Get All Conditional Triggers

`GET` https://api.dhan.co/v2/alerts/orders

Retrieve all conditional triggers for the authenticated account.

Retrieve a list of all conditional triggers for the authenticated account, along with their current status and configuration details.


### Status Codes

| Code | Description |
|------|-------------|
| 200 | List of conditional triggers |
| 401 | Unauthorized |


### Example Request

```bash
curl -X GET https://api.dhan.co/v2/alerts/orders \
  -H "access-token: <your-access-token>"
```
