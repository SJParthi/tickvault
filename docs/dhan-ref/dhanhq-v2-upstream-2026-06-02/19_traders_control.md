# Trader's Control (Kill Switch & P&L Exit) — DhanHQ v2

> Source: https://dhanhq.co/docs/v2/traders-control/ · API v2 · Audited 2 Jun 2026

Risk-management tools: Kill Switch (disable trading for the day) and P&L-based auto-exit.

| Method | Path | Use |
|---|---|---|
| POST | `/killswitch` | Activate/deactivate Kill Switch |
| GET | `/killswitch` | Kill Switch status |
| POST | `/pnlExit` | Configure P&L-based exit |
| DELETE | `/pnlExit` | Stop P&L-based exit |
| GET | `/pnlExit` | Get P&L-based exit config |

## Manage Kill Switch — `POST /killswitch`
Disables trading for the current trading day. Pass `killSwitchStatus=ACTIVATE` or `DEACTIVATE` as a query param. No body.
> To activate, ensure all positions are closed and there are no pending orders.
```bash
curl --request POST \
  --url 'https://api.dhan.co/v2/killswitch?killSwitchStatus=ACTIVATE' \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: JWT'
```
**Response**
```json
{ "dhanClientId": "1000000009", "killSwitchStatus": "Kill Switch has been successfully activated" }
```

## Kill Switch Status — `GET /killswitch`
No body.
**Response**
```json
{ "dhanClientId": "1000000009", "killSwitchStatus": "ACTIVATE" }
```
`killSwitchStatus` enum: `ACTIVATE` / `DEACTIVATE`.

## P&L Based Exit — `POST /pnlExit`
Auto-exit all applicable positions when cumulative profit/loss thresholds are breached. **Active for the current day; reset at session end.**
```bash
curl --request POST --url https://api.dhan.co/v2/pnlExit \
  --header 'Accept: application/json' --header 'Content-Type: application/json' \
  --header 'access-token: ' --data '{Request Body}'
```
**Request**
```json
{
  "profitValue": "1500.00",
  "lossValue": "500.00",
  "productType": ["INTRADAY", "DELIVERY"],
  "enableKillSwitch": true
}
```
| Field | Type | Description |
|---|---|---|
| `profitValue` | float | Target profit amount |
| `lossValue` | float | Target loss amount |
| `productType` | array string | Applicable types: `INTRADAY`, `DELIVERY` |
| `enableKillSwitch` | boolean | Enable kill switch with this exit |

**Response**
```json
{ "pnlExitStatus": "ACTIVE", "message": "P&L based exit configured successfully" }
```
`pnlExitStatus` enum: `ACTIVE` / `INACTIVE`.

> **Warning:** if `profitValue` is set below the current P&L profit (or `lossValue` above the current loss), the exit triggers immediately.

## Stop P&L Based Exit — `DELETE /pnlExit`
No body.
**Response**
```json
{ "pnlExitStatus": "DISABLED", "message": "P&L based exit stopped successfully" }
```

## Get P&L Based Exit — `GET /pnlExit`
No body. Returns the current day's active config.
**Response**
```json
{
  "pnlExitStatus": "ACTIVE",
  "profit": "1500.00",
  "loss": "500.00",
  "productType": ["INTRADAY", "DELIVERY"],
  "enable_kill_switch": true
}
```
| Field | Type | Description |
|---|---|---|
| `pnlExitStatus` | string | `ACTIVE` / `INACTIVE` |
| `profit` | float | Target profit amount |
| `loss` | float | Target loss amount |
| `enableKillSwitch` | boolean | (returned as `enable_kill_switch`) |
| `productType` | array string | `INTRADAY`, `DELIVERY` |

> Note the field-name inconsistency: request uses `enableKillSwitch`; the GET response sample uses `enable_kill_switch`. Handle both.
For enum descriptions, see `08_annexure.md`. (Exit-All-Positions lives in `21_portfolio.md`.)
