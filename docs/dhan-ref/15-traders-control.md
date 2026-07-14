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

Response:
```json
{
    "pnlExitStatus": "ACTIVE",
    "profit": "1500.00",
    "loss": "500.00",
    "productType": ["INTRADAY", "DELIVERY"],
    "enable_kill_switch": true
}
```

> **Field name differences**: POST uses `profitValue`/`lossValue` (camelCase), GET returns `profit`/`loss` (short names). POST uses `enableKillSwitch` (camelCase), GET returns `enable_kill_switch` (snake_case). Handle both conventions in deserialization.

---

## 4. Implementation Notes

1. **Kill Switch disables ALL trading** for the day. Use as emergency stop.
2. **P&L exit is session-scoped** — must reconfigure daily if needed.
3. **`enableKillSwitch: true`** in P&L exit = after auto-exit, also activates kill switch to prevent further trading.
4. **For tickvault**: Wire kill switch activation into your error handlers. If something goes wrong during July+ live trading, call kill switch immediately.

---

## 2026-07-14 Upstream Update (runner-crawled live pages)

**Evidence tier: Verified-live.** Raw HTML of `https://dhanhq.co/docs/v2/traders-control/`
(runs 1–3, sha256 `a1f3da12` content-identical, latest 2026-07-14T07:58:29Z) + the portal
`guides/traders-control.md` export. Full manifest: `00-COVERAGE-MANIFEST.md`.

1. **Kill-switch header-vs-query ambiguity PERSISTS verbatim on the live page:** prose says
   "You can pass **header parameter** as ACTIVATE or DEACTIVATE to manage Kill Switch
   settings." while the curl uses `?killSwitchStatus=ACTIVATE` (query string). The page
   contradicts itself exactly as previously flagged; the repo documents the query form
   (matches the curl — the safer reading). Live-probe on first use. This upgrades
   `verification-2026-07-13.md` §4 flag 5 from paraphrase-quality to Verified-live (the page
   REALLY carries both).
2. **NEW: `GET /killswitch` status endpoint** is explicit on the portal guide — returns the
   enum (`ACTIVATE`/`DEACTIVATE`), while the POST response returns a human message string
   ("Kill Switch has been successfully activated"). Deserialize the POST response as a free
   string, never the enum.
3. **Loss-side immediate-trigger wording (quote the literal; direction Assumed):** live
   verbatim — "In case of profitValue set below the current Profit in P&L, then the P&L based
   exit will be triggered immediately. This applies to **lossValue set above the current
   Loss** in P&L as well." The repo's "lossValue < current loss" direction wording differs;
   both plausibly mean "threshold already breached". Loss-side direction remains Assumed
   until probed.
4. Live-internal wobble: the DELETE /pnlExit example returns `"pnlExitStatus": "DISABLED"`
   while its own param table says ACTIVE/INACTIVE — deserialize as a free string.
