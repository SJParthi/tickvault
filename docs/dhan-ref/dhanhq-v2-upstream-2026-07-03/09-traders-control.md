# DhanHQ API v2 — Trader's Control (Kill Switch + P&L Based Exit)

> **Source:** Official DhanHQ documentation (docs.dhanhq.co) — captured from DhanHQ's own "Export .md for LLMs" file, generated 2026-06-30.
> **Pages covered in this file:**
> - https://docs.dhanhq.co/api/v2/traders-control
> - https://docs.dhanhq.co/api/v2/guides/traders-control
> - https://docs.dhanhq.co/api/v2/traders-control/manage-kill-switch
> - https://docs.dhanhq.co/api/v2/traders-control/get-kill-switch-status
> - https://docs.dhanhq.co/api/v2/traders-control/configure-pnl-exit
> - https://docs.dhanhq.co/api/v2/traders-control/get-pnl-exit
> - https://docs.dhanhq.co/api/v2/traders-control/stop-pnl-exit


---

## Trader's Control

Trader's Control

These set of APIs are built for traders to manage their risks and preferences using advanced tools built in right into Dhan. You can set and manage Kill Switch for your account along with having P&L based auto-exit feature.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/killswitch` | Manage Kill Switch |
| GET | `/killswitch` | Kill Switch Status |
| POST | `/pnlExit` | Configure P&L Based Exit |
| DELETE | `/pnlExit` | Stop P&L Based Exit |
| GET | `/pnlExit` | Get P&L Based Exit |

---

## Trader's Control

Kill Switch and P&L based auto-exit APIs for managing trading risk

APIs built for traders to manage their risks and preferences using advanced tools. You can set and manage Kill Switch for your account along with P&L based auto-exit feature.

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/killswitch` | Manage Kill Switch |
| GET | `/killswitch` | Kill Switch Status |
| POST | `/pnlExit` | Configure P&L Based Exit |
| DELETE | `/pnlExit` | Stop P&L Based Exit |
| GET | `/pnlExit` | Get P&L Based Exit |

---

## Manage Kill Switch

Activate or deactivate the kill switch for your account, which will disable trading for the current trading day. Pass query parameter as `ACTIVATE` or `DEACTIVATE`.

> **note:** Ensure all your positions are closed and there are no pending orders to be able to activate Kill Switch.

```bash
curl --request POST \
  --url 'https://api.dhan.co/v2/killswitch?killSwitchStatus=ACTIVATE' \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: {JWT}'
```

**Request** — No Body

**Response**

```json
{
    "dhanClientId": "1000000009",
    "killSwitchStatus": "Kill Switch has been successfully activated"
}
```

---

## Kill Switch Status

Check whether kill switch is active for the current trading day.

```bash
curl --request GET \
  --url https://api.dhan.co/v2/killswitch \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Response**

```json
{
    "dhanClientId": "1000000009",
    "killSwitchStatus": "ACTIVATE"
}
```

---

## P&L Based Exit

Configure automatic exit rules based on cumulative profit or loss thresholds. When the defined limits are breached, all applicable positions are exited.

> **note:** The configured P&L based exit remains active for the current day and is reset at the end of the trading session.

```bash
curl --request POST \
  --url https://api.dhan.co/v2/pnlExit \
  --header 'Accept: application/json' \
  --header 'Content-Type: application/json' \
  --header 'access-token: {JWT}' \
  --data '{Request Body}'
```

**Request Structure**

```json
{
    "profitValue": "1500.00",
    "lossValue": "500.00",
    "productType": ["INTRADAY", "DELIVERY"],
    "enableKillSwitch": true
}
```


| Parameter | Data Type | Description |
|-----------|-----------|-------------|
| `profitValue` | float | Target profit amount for the P&L exit |
| `lossValue` | float | Target loss amount for the P&L exit |
| `productType` | string | Product types — `INTRADAY` `DELIVERY` |
| `enableKillSwitch` | boolean | Enable kill switch for this P&L exit |

**Response**

```json
{
    "pnlExitStatus": "ACTIVE",
    "message": "P&L based exit configured successfully"
}
```

> **warning:** If `profitValue` is set below the current profit in P&L, the exit will be triggered immediately. The same applies if `lossValue` is set above the current loss.

---

## Stop P&L Based Exit

Disable the active P&L based exit configuration.

```bash
curl --request DELETE \
  --url https://api.dhan.co/v2/pnlExit \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

**Response**

```json
{
    "pnlExitStatus": "DISABLED",
    "message": "P&L based exit stopped successfully"
}
```

---

## Get P&L Based Exit

Fetch the currently active P&L based exit configuration for the current trading day.

```bash
curl --request GET \
  --url https://api.dhan.co/v2/pnlExit \
  --header 'Accept: application/json' \
  --header 'access-token: {JWT}'
```

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

| Parameter | Data Type | Description |
|-----------|-----------|-------------|
| `pnlExitStatus` | string | `ACTIVE` or `INACTIVE` |
| `profit` | float | Target profit amount |
| `loss` | float | Target loss amount |
| `enableKillSwitch` | boolean | Kill switch status |
| `productType` | string | Applicable product types |

For description of enum values, refer to the [Annexure](/api/v2/guides/annexure).


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification |
| killSwitchStatus | string | No | Status of Kill Switch — activated or not |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification |
| killSwitchStatus | string | No | or Values: `ACTIVATE`, `DEACTIVATE`. |

---

## Manage Kill Switch

`POST` https://api.dhan.co/v2/killswitch

Activate or deactivate the kill switch for your account. Disables trading for the current day.

This API lets you activate the kill switch for your account, which will disable trading for current trading day. Pass `killSwitchStatus` as a query parameter.

> **Note:** You need to ensure that all your positions are closed and there are no pending orders in your account to be able to activate Kill Switch.


> **NOTICE:** Order Placement, Modification and Cancellation APIs requires Static IP whitelisting - [refer here](/api/v2/guides/authentication#setup-static-ip)


### Query Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| killSwitchStatus | string | No | Kill switch status Values: `ACTIVATE`, `DEACTIVATE`. |


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| killSwitchStatus | string | No | Status of Kill Switch Values: `ACTIVATE`, `DEACTIVATE`. |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Kill switch status updated |
| 401 | Unauthorized |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/killswitch \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```

---

## Kill Switch Status

`GET` https://api.dhan.co/v2/killswitch

Check whether kill switch is active for the current trading day.

The API allows you to check kill switch status for your account - whether it is active for the current trade or not.


### Response Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| killSwitchStatus | string | No | Status of Kill Switch Values: `ACTIVATE`, `DEACTIVATE`. |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Current kill switch status |
| 401 | Unauthorized |


### Example Request

```bash
curl -X GET https://api.dhan.co/v2/killswitch \
  -H "access-token: <your-access-token>"
```

---

## Configure P&L Based Exit

`POST` https://api.dhan.co/v2/pnlExit

Configure automatic exit rules based on cumulative profit or loss thresholds.

> **Note:** The configured P&L based exit remains active for the current day and is reset at the end of the trading session.


> **NOTICE:** Order Placement, Modification and Cancellation APIs requires Static IP whitelisting - [refer here](/api/v2/guides/authentication#setup-static-ip)


| Parameter | Data Type | Description |
|-----------|-----------|-------------|
| pnlExitStatus | string | P&L based exit status: `ACTIVE` `INACTIVE` |
| message | string | Status of Conditional Trigger |


### Request Body Parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| dhanClientId | string | No | User specific identification generated by Dhan |
| profitValue | float | No | Maximum portfolio-level profit to trigger exit |
| lossValue | float | No | Maximum portfolio-level loss to trigger exit |
| productType[0] | enum | No | Product types applicable (array): INTRADAY or DELIVERY Values: `INTRADAY`, `DELIVERY`. Default: `INTRADAY`. |
| enableKillSwitch | boolean | No | Indicates if kill switch is enabled (true or false) |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | P&L exit configured |
| 401 | Unauthorized |


### Example Request

```bash
curl -X POST https://api.dhan.co/v2/pnlExit \
  -H "access-token: <your-access-token>" \
  -H "Content-Type: application/json"
```

---

## Get P&L Based Exit

`GET` https://api.dhan.co/v2/pnlExit

Fetch the currently active P&L based exit configuration.

| Parameter | Data Type | Description |
|-----------|-----------|-------------|
| pnlExitStatus | string | Current status of the P&L exit operation: `ACTIVE` `INACTIVE` |
| profit | float | User-defined target profit amount for the P&L exit |
| loss | float | User-defined target loss amount for the P&L exit |
| enableKillSwitch | boolean | Indicates if the kill switch is enabled for this P&L exit |
| productType | string | Product types applicable: `INTRADAY` `DELIVERY` |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | Current P&L exit config |
| 401 | Unauthorized |


### Example Request

```bash
curl -X GET https://api.dhan.co/v2/pnlExit \
  -H "access-token: <your-access-token>"
```

---

## Stop P&L Based Exit

`DELETE` https://api.dhan.co/v2/pnlExit

Disable the active P&L based exit configuration.

> **NOTICE:** Order Placement, Modification and Cancellation APIs requires Static IP whitelisting - [refer here](/api/v2/guides/authentication#setup-static-ip)


| Parameter | Data Type | Description |
|-----------|-----------|-------------|
| pnlExitStatus | string | P&L based exit status: `ACTIVE` `INACTIVE` |
| message | string | Status of Conditional Trigger |


### Status Codes

| Code | Description |
|------|-------------|
| 200 | P&L exit stopped |
| 401 | Unauthorized |


### Example Request

```bash
curl -X DELETE https://api.dhan.co/v2/pnlExit \
  -H "access-token: <your-access-token>"
```
