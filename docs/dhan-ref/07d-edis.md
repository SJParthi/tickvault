# Dhan V2 EDIS (Electronic Delivery Instruction Slip) — Reference

> **Source**: https://dhanhq.co/docs/v2/edis/
> **Extracted**: 2026-03-13

---

## Overview

Required to sell holding stocks — CDSL eDIS flow for T-PIN generation and stock marking.

| Method | Endpoint                  | Description                     |
|--------|---------------------------|---------------------------------|
| GET    | `/edis/tpin`              | Generate T-PIN (SMS to mobile)  |
| POST   | `/edis/form`              | Get CDSL HTML form              |
| GET    | `/edis/inquire/{isin}`    | Check eDIS approval status      |

---

## 1. Generate T-PIN

```
GET https://api.dhan.co/v2/edis/tpin
```
Response: `202 Accepted`. T-PIN sent to registered mobile.

## 2. Generate eDIS Form

```
POST https://api.dhan.co/v2/edis/form
Body: { "isin": "INE733E01010", "qty": 1, "exchange": "NSE", "segment": "EQ", "bulk": true }
```
Response: escaped HTML form to render in browser for CDSL T-PIN entry.

`bulk: true` = mark all holdings for sell.

## 3. EDIS Inquiry

```
GET https://api.dhan.co/v2/edis/inquire/{isin}
```
Pass `ALL` instead of ISIN to check all holdings. Response: `totalQty`, `aprvdQty`, `status`.
