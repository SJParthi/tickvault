# EDIS (CDSL eDIS) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/edis/ · API v2 · Audited 2 Jun 2026

To sell holding stocks you must complete the CDSL eDIS flow: generate T-PIN and mark stock to authorise the sell.

| Method | Path | Use |
|---|---|---|
| GET | `/edis/tpin` | Generate T-PIN (sent to registered mobile) |
| POST | `/edis/form` | Get escaped CDSL HTML form & enter T-PIN |
| GET | `/edis/inquire/{isin}` | Inquire eDIS approval status of a stock |

## Generate T-PIN — `GET /edis/tpin`
Sends a T-PIN to your registered mobile. No body.
```bash
curl --request GET --url https://api.dhan.co/v2/edis/tpin \
  --header 'Content-Type: application/json' --header 'access-token: JWT'
```
**Response:** `202 Accepted`.

## Generate eDIS Form — `POST /edis/form`
Returns an **escaped** CDSL HTML form; render (unescape) it at your end so the user can enter the T-PIN and mark stock for approval. Get ISINs from the Holdings API (`21_portfolio.md`).
```bash
curl --request POST --url https://api.dhan.co/v2/edis/form \
  --header 'Content-Type: application/json' --header 'access-token: ' --data '{}'
```
**Request**
```json
{ "isin": "INE733E01010", "qty": 1, "exchange": "NSE", "segment": "EQ", "bulk": true }
```
| Field | Type | Description |
|---|---|---|
| `isin` | string | ISIN |
| `qty` | int | Shares to mark for the eDIS transaction |
| `exchange` | string | `NSE` `BSE` |
| `segment` | string | `EQ` |
| `bulk` | boolean | Mark eDIS for all stocks in portfolio |

**Response**
```json
{
  "dhanClientId": "1000000401",
  "edisFormHtml": "<!DOCTYPE html> <html> ... <form name=\"frmDIS\" method=\"post\" action=\"https://edis.cdslindia.com/eDIS/VerifyDIS/\"> ... </form> </html>"
}
```
| Field | Type | Description |
|---|---|---|
| `dhanClientId` | string | User ID |
| `edisFormHtml` | string | Escaped HTML form (render at your end) |

## EDIS Status & Inquiry — `GET /edis/inquire/{isin}`
Check whether a stock is approved/marked for sell. Pass the stock's ISIN, or `ALL` to get status for every holding. No body.
```bash
curl --request GET --url https://api.dhan.co/v2/edis/inquire/{isin} \
  --header 'Content-Type: application/json' --header 'access-token: JWT'
```
Python: `dhan.edis_inquiry(isin)`
**Response**
```json
{
  "clientId": "1000000401",
  "isin": "INE00IN01015",
  "totalQty": 10,
  "aprvdQty": 4,
  "status": "SUCCESS",
  "remarks": "eDIS transaction done successfully"
}
```
| Field | Type | Description |
|---|---|---|
| `clientId` | string | User ID |
| `isin` | string | ISIN |
| `totalQty` | string | Total shares for the stock |
| `aprvdQty` | string | Approved shares |
| `status` | string | eDIS order status |
| `remarks` | string | Status remarks |

For enum descriptions, see `08_annexure.md`.
