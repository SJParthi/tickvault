# DhanHQ API v2 — Annexure (All Enums, Constants & Error Codes)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/guides/annexure


---

## Annexure

Complete reference for all enum values, exchange segments, product types, error codes and more

Reference tables for all enum values used across DhanHQ APIs.

---

## Exchange Segment

| Attribute | Exchange | Segment | Enum |
|-----------|----------|---------|------|
| `IDX_I` | Index | Index Value | `0` |
| `NSE_EQ` | NSE | Equity Cash | `1` |
| `NSE_FNO` | NSE | Futures & Options | `2` |
| `BSE_EQ` | BSE | Equity Cash | `4` |
| `MCX_COMM` | MCX | Commodity | `5` |
| `BSE_FNO` | BSE | Futures & Options | `8` |

---

## Product Type

| Attribute | Detail |
|-----------|--------|
| `CNC` | Cash & Carry for equity deliveries |
| `INTRADAY` | Intraday for Equity, Futures & Options |
| `MARGIN` | Carry Forward in Futures & Options |
| `MTF` | Margin Trading Facility |

---

## Order Status

| Attribute | Detail |
|-----------|--------|
| `TRANSIT` | Did not reach the exchange server |
| `PENDING` | Awaiting execution |
| `CLOSED` | Used for Super Order, once both entry and exit orders are placed |
| `TRIGGERED` | Used for Super Order, if Target or Stop Loss leg is triggered |
| `REJECTED` | Rejected by broker/exchange |
| `CANCELLED` | Cancelled by user |
| `PART_TRADED` | Partial Quantity traded successfully |
| `TRADED` | Executed successfully |

---

## After Market Order Time

| Attribute | Detail |
|-----------|--------|
| `PRE_OPEN` | AMO pumped at pre-market session |
| `OPEN` | AMO pumped at market open |
| `OPEN_30` | AMO pumped 30 minutes after market open |
| `OPEN_60` | AMO pumped 60 minutes after market open |

---

## Expiry Code

| Attribute | Detail |
|-----------|--------|
| `1` | Near Expiry |
| `2` | Next Expiry |
| `3` | Far Expiry |

---

## Instrument

| Attribute | Detail |
|-----------|--------|
| `INDEX` | Index |
| `FUTIDX` | Futures of Index |
| `OPTIDX` | Options of Index |
| `EQUITY` | Equity |
| `FUTSTK` | Futures of Stock |
| `OPTSTK` | Options of Stock |
| `FUTCOM` | Futures of Commodity |
| `OPTFUT` | Options of Commodity Futures |

---

## Feed Request Code

| Code | Action |
|------|--------|
| `11` | Connect Feed |
| `12` | Disconnect Feed |
| `15` | Subscribe — Ticker Packet |
| `16` | Unsubscribe — Ticker Packet |
| `17` | Subscribe — Quote Packet |
| `18` | Unsubscribe — Quote Packet |
| `21` | Subscribe — Full Packet |
| `22` | Unsubscribe — Full Packet |
| `23` | Subscribe — Full Market Depth |
| `25` | Unsubscribe — Full Market Depth |

---

## Feed Response Code

| Code | Packet |
|------|--------|
| `1` | Index Packet |
| `2` | Ticker Packet |
| `4` | Quote Packet |
| `5` | OI Packet |
| `6` | Prev Close Packet |
| `7` | Market Status Packet |
| `8` | Full Packet |
| `50` | Feed Disconnect |

---

## Trading API Error

| Type | Code | Message |
|------|------|---------|
| Invalid Authentication | `DH-901` | Client ID or user generated access token is invalid or expired |
| Invalid Access | `DH-902` | User has not subscribed to Data APIs or does not have access to Trading APIs |
| User Account | `DH-903` | Errors related to User's Account — check if required segments are activated |
| Rate Limit | `DH-904` | Too many requests — try throttling API calls |
| Input Exception | `DH-905` | Missing required fields, bad values for parameters etc. |
| Order Error | `DH-906` | Incorrect request for order — cannot be processed |
| Data Error | `DH-907` | Unable to fetch data due to incorrect parameters or no data present |
| Internal Server Error | `DH-908` | Server was not able to process request — occurs rarely |
| Network Error | `DH-909` | API was unable to communicate with backend system |
| Others | `DH-910` | Error originating from other reasons |

---

## Data API Error

| Code | Description |
|------|-------------|
| `800` | Internal Server Error |
| `804` | Requested number of instruments exceeds limit |
| `805` | Too many requests or connections — further requests may result in blocking |
| `806` | Data APIs not subscribed |
| `807` | Access token is expired |
| `808` | Authentication Failed — Client ID or Access Token invalid |
| `809` | Access token is invalid |
| `810` | Client ID is invalid |
| `811` | Invalid Expiry Date |
| `812` | Invalid Date Format |
| `813` | Invalid SecurityId |
| `814` | Invalid Request |
