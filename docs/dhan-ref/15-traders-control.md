# Dhan V2 Trader's Control — Complete Reference

> **Source**: https://dhanhq.co/docs/v2/traders-control/
> **Extracted**: 2026-03-13

---

## 1. Overview

Risk management APIs: Kill Switch and P&L-based auto-exit.

| Method | Endpoint       | Description                |
|--------|----------------|----------------------------|
| POST   | `/killswitch`  | Activate/deactivate        |
| GET    | `/killswitch`  | Check status               |
| POST   | `/pnlExit`     | Configure P&L exit         |
| DELETE | `/pnlExit`     | Stop P&L exit              |
| GET    | `/pnlExit`     | Get P&L exit config        |

---

## 2. Kill Switch

### Activate/Deactivate

```
POST https://api.dhan.co/v2/killswitch?killSwitchStatus=ACTIVATE
Headers: access-token
```

Query param: `killSwitchStatus` = `ACTIVATE` or `DEACTIVATE`

> **Prerequisite**: All positions must be closed and no pending orders to activate.

### Check Status

```
GET https://api.dhan.co/v2/killswitch
Headers: access-token
```

Response: `{ "killSwitchStatus": "ACTIVATE" }` or `"DEACTIVATE"`

---

## 3. P&L Based Exit

### Configure

```
POST https://api.dhan.co/v2/pnlExit
Headers: access-token
```

```json
{
    "profitValue": "1500.00",
    "lossValue": "500.00",
    "productType": ["INTRADAY", "DELIVERY"],
    "enableKillSwitch": true
}
```

> **WARNING**: If `profitValue` < current profit OR `lossValue` < current loss, exit triggers **immediately**.

Response: `{ "pnlExitStatus": "ACTIVE", "message": "P&L based exit configured successfully" }`

Active for current day only — resets at end of session.

> **Note**: The `productType` array uses `"DELIVERY"` (not `"CNC"`) to refer to delivery/cash-and-carry positions. This differs from the `"CNC"` value used in order placement APIs. Both refer to the same concept but use different string values depending on the API.

### Stop

```
DELETE https://api.dhan.co/v2/pnlExit
```

### Get Config

```
GET https://api.dhan.co/v2/pnlExit
```

Response: `{ "pnlExitStatus": "ACTIVE", "profit": "1500.00", "loss": "500.00", ... }`

---

## 4. Implementation Notes

1. **Kill Switch disables ALL trading** for the day. Use as emergency stop.
2. **P&L exit is session-scoped** — must reconfigure daily if needed.
3. **`enableKillSwitch: true`** in P&L exit = after auto-exit, also activates kill switch to prevent further trading.
4. **For dhan-live-trader**: Wire kill switch activation into your error handlers. If something goes wrong during July+ live trading, call kill switch immediately.
