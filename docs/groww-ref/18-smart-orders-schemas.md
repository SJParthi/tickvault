# 18 — Smart Orders (GTT / OCO) — FULL Recovered Schemas + Day-0 Probe Checklist

> **Source:** the operator's bruteX repo, 2026-07-03 lossless official-docs capture —
> `SJParthi/bruteX` @ `349b1a9`, `docs/groww/trade-api-2026-07-03/17-BONUS-REST-smart-orders.md`
> (the REST/curl page, 526 lines — cited below as **[R:Lnn]**) and
> `docs/groww/trade-api-2026-07-03/03-smart-orders.md` (the python-sdk page, 645 lines —
> cited as **[P:Lnn]**). Original pages: `groww.in/trade-api/docs/curl/smart-orders` +
> `…/python-sdk/smart-orders`, both captured 2026-07-03 with scripted 100%-match
> verification (headings/tables/code blocks vs live HTML — see the capture's `00-INDEX.md`).
> **Compiled/Recovered:** 2026-07-14. **⚠ INTERNAL RECOVERY — NO NEW CRAWL.** `groww.in`
> remains 403-blocked at the sandbox egress proxy; this file recovers the capture content
> that the 2026-07-13 full-coverage session read only to line 120 (see
> `16-orders-margins-portfolio.md` §1 row 14 and the session record). Everything below the
> **Verified-capture** tier is verbatim-grounded in those two files.
> **Related:** `16-orders-margins-portfolio.md` (endpoint inventory + GA codes),
> `15-rate-limits-and-capacity.md` (families), `13-annexures-enums.md` (shared enums),
> `99-UNKNOWNS.md` (U-numbering; the residual smart-order unknowns live in §9 below).

> **⚠ NOT USED BY TICKVAULT.** TickVault places NO Groww orders — the Groww feed is
> capture + candles + cross-verification only
> (`.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §1/§33: no strategy
> wiring, no order path, `dry_run = true`). This file exists for DOCUMENTATION
> COMPLETENESS so a future authorized session needs zero re-research. Any code consuming
> this surface requires a fresh dated operator quote FIRST.

---

## 0. What this file resolves (vs the 2026-07-13 state of knowledge)

The 2026-07-13 session had read the REST smart-orders capture **only to line 120** —
everything from the OCO body example onward was missing internally. Recovered here,
all **Verified-capture**:

| Previously missing | Now | Section |
|---|---|---|
| OCO create body schema (target/SL leg fields) + 201 response | **RESOLVED** | §2 |
| Modify smart order — modifiable-field lists, request schemas, 202 response | **RESOLVED** | §3 |
| Cancel / Get / List request + response schemas | **RESOLVED** | §4–§5 |
| Smart-order status lifecycle enum + SDK constants | **RESOLVED** (6 values) | §6 |
| Smart Order Response (GTT/OCO) full schemas | **RESOLVED** | §7 |
| `child_legs` structure · expiry prose · reference_id window · OCO partial-fill/locus · rate-limit family · push-stream events · post-trigger linkage | **STILL UNKNOWN** — probe list | §8–§9 |

GTT create (request + 201 response) was already Verified internally
(`16-orders-margins-portfolio.md`; capture [R:L19–L101], [P:L19–L110]) and is not
repeated here except where the full response schema (§7) supersedes the truncated view.

## 1. Family recap (Verified-capture)

- Two types: **GTT** ("Triggers a single order when price crosses your trigger") and
  **OCO** ("Places target and stop-loss together; execution of one cancels the other")
  [R:L12–L15].
- "The `COMMODITY` segment is not supported for Smart Orders. OCO orders for `CASH`
  segment are currently not supported." [R:L17] — **but see the intra-doc contradiction
  in §8.1** (the same REST page later says CASH OCO supports `MIS` only, and the
  python-sdk page's note omits the CASH-OCO ban entirely [P:L17]).
- Endpoints (base `https://api.groww.in`, standard headers — `Authorization: Bearer` +
  `Accept: application/json` + `X-API-VERSION: 1.0`; POST/PUT add `Content-Type`):
  create `POST /v1/order-advance/create` [R:L21]; modify
  `PUT /v1/order-advance/modify/{smart_order_id}` [R:L194]; cancel
  `POST /v1/order-advance/cancel/{segment}/{smart_order_type}/{smart_order_id}` [R:L325];
  get `GET /v1/order-advance/status/{segment}/{smart_order_type}/internal/{smart_order_id}`
  [R:L370]; list `GET /v1/order-advance/list` [R:L405].

## 2. Create OCO — request + response (Verified-capture, [R:L103–L190], [P:L112–L220])

Body example [R:L121–L136]:

```json
{
  "reference_id": "sref-unique-456",
  "smart_order_type": "OCO",
  "segment": "FNO",
  "trading_symbol": "NIFTY25OCT24000CE",
  "quantity": 50,
  "net_position_quantity": 50,
  "transaction_type": "SELL",
  "target": {"trigger_price": "120.50", "order_type": "LIMIT", "price": "121.00"},
  "stop_loss": {"trigger_price": "95.00", "order_type": "SL_M", "price": null},
  "product_type": "MIS",
  "exchange": "NSE",
  "duration": "DAY"
}
```

Request schema (verbatim; `*` = required) [R:L140–L159]:

| Field | Type | Description (verbatim) |
|---|---|---|
| reference_id `*` | string | "User-provided alphanumeric string (8-20 characters) that serves as an idempotency key, with at most two hyphens (-) allowed." |
| smart_order_type `*` | string | "Set to `OCO` to create a One-Cancels-Other smart order (target + stop-loss)." |
| segment `*` | string | Examples: `FNO`, `CASH`. |
| trading_symbol `*` | string | Trading Symbol as defined by the exchange. |
| quantity `*` | integer | "Total quantity for both legs. Must be ≤ `abs(net_position_quantity)`. Example: `50`." |
| net_position_quantity `*` | integer | "Your current net position in this symbol. Used to derive leg directions and validate quantity." |
| transaction_type `*` | string | "Direction of protection/exit for your position." `BUY`/`SELL`. |
| target.trigger_price `*` | string | "Take-profit trigger price (decimal string)." |
| target.order_type `*` | string | "Order type for the target leg. Examples: `LIMIT`, `MARKET`." |
| target.price | string | "Target leg limit price (required if `target.order_type` = `LIMIT`)." |
| stop_loss.trigger_price `*` | string | "Stop-loss trigger price (decimal string)." |
| stop_loss.order_type `*` | string | "Order type for the stop-loss leg. Examples: `SL`, `SL_M`." |
| stop_loss.price | string | "Stop-loss leg limit price (required if `stop_loss.order_type` = `SL`)." (`null` for `SL_M` — body example) |
| product_type `*` | string | "Product for the OCO. Note: For OCO in cash segment, only `MIS` is supported currently." |
| exchange `*` | string | Example: `NSE`. |
| duration `*` | string | "Validity for both legs. Example: `DAY`." |

Key semantics (verbatim):

- "`quantity` must be ≤ `abs(net_position_quantity)`." · "If a leg executes, the other
  cancels automatically." [R:L189–L190]
- "**OCO orders are meant to exit an existing position.** In the **F&O** segment you must
  already hold the contract. In the **CASH** segment, OCO is restricted to intraday
  (`MIS`) positions." [P:L178–L182]
- "The leg directions are derived from your net position." [R:L461]

201 response [R:L163–L183]: `{"status":"SUCCESS","payload":{…}}` with
`smart_order_id: "oco_a12bc3"`, `smart_order_type: "OCO"`, `status: "ACTIVE"`, the echoed
`target`/`stop_loss` objects, `quantity`, `product_type`, `duration`, `created_at`,
**`expire_at: null`** (contrast GTT's +1-year example), `triggered_at: null`,
`updated_at`. The python-sdk twin additionally shows `is_cancellation_allowed` /
`is_modification_allowed` [P:L186–L212]. Note the OCO create response does NOT carry
`net_position_quantity` or `transaction_type` back.

## 3. Modify smart order (Verified-capture, [R:L192–L321], [P:L222–L364])

"Modify contracts differ by flow. Only the fields listed below are honoured; everything
else is ignored or rejected. Use cancel + create when you need changes outside of these
lists." [R:L196–L197]

**Modifiable — GTT** [R:L199–L206]: `quantity`, `trigger_price`, `trigger_direction`,
`order.order_type`, `order.price` ("required for `LIMIT`/`SL` types; set to `null` for
`MARKET`/`SL_M`"), `child_legs` ("all child leg fields are modifiable if provided").
`order.transaction_type` is "required but not modifiable" [R:L250].

**Modifiable — OCO** [R:L208–L214]: `quantity`, `duration`, `product_type` ("e.g.,
MIS ↔ NRML for FNO; CASH OCO only supports MIS"), `target.trigger_price`,
`stop_loss.trigger_price`. Leg `order_type`/`price` are **NOT modifiable** [P:L327–L331].

Both modify bodies also carry the routing-required `smart_order_type *` + `segment *`
[R:L243–L244, L283–L284]; the SDK adds `smart_order_id *` as a parameter [P:L275].
OCO modify body example [R:L268–L277] — legs may be sent partially:
`"target": {"trigger_price": "122.00"}, "stop_loss": {"trigger_price": "97.50"}`.

**Response (202)** [R:L293–L300] — a PARTIAL echo, not the full object:
`{"status":"SUCCESS","payload":{"smart_order_id":"oco_a12bc3","smart_order_type":"OCO","status":"ACTIVE","quantity":40}}`.

Modify-vs-cancel-create matrix (verbatim table [R:L306–L318]): quantity ✅/✅ · trigger
price ✅/✅(both legs) · trigger direction ✅/N-A · order type ✅/❌ · limit price ✅/❌ ·
duration ❌/✅ · product ❌/✅ · symbol/exchange/segment/type ❌/❌ (cancel + create).

## 4. Cancel + Get (Verified-capture)

- **Cancel** [R:L323–L366]: `POST /v1/order-advance/cancel/{segment}/{smart_order_type}/{smart_order_id}`
  (path params only, no body). Response **202**:
  `{"status":"SUCCESS","payload":{"smart_order_id":"gtt_91a7f4","smart_order_type":"GTT","status":"CANCELLED"}}`
  [R:L350–L355]. Response schema: status = "`CANCELLED` on success" [R:L361–L366].
- **Get** [R:L368–L401]: `GET /v1/order-advance/status/{segment}/{smart_order_type}/internal/{smart_order_id}`
  (note the literal `/internal/` segment). Response **200** = the FULL smart-order object
  per the §7 schemas [R:L392–L401]; python-sdk example shows the complete GTT object incl.
  `ltp`, `remark`, `display_name`, `child_legs: null` [P:L444–L475].

## 5. List (Verified-capture, [R:L403–L455], [P:L479–L546])

`GET /v1/order-advance/list?segment=&smart_order_type=&status=&page=&page_size=&start_date_time=&end_date_time=`

| Param | Notes (verbatim highlights) |
|---|---|
| segment | optional; `FNO`/`CASH` |
| smart_order_type | "Examples: `OCO`, `GTT`. Default: `OCO`." [R:L424] |
| status | "Current state filter (live vs past). Examples: `ACTIVE`, `CANCELLED`, `COMPLETED`. Default: `ACTIVE`." [R:L425]; python-sdk adds `TRIGGERED` as a filter example [P:L512] |
| page | "starting from 0 … Min: `0`, Max: `500`. Default: `0`." |
| page_size | "Min: `1`, Max: `50`. Default: `10`." |
| start_date_time | ISO8601 `YYYY-MM-DDThh:mm:ss`, inclusive; "Defaults to start of today (server timezone)." [R:L428] |
| end_date_time | inclusive; "Defaults to start of next day (server timezone)." [R:L429] |

Validations [R:L431–L434]: `end_date_time` ≥ `start_date_time`; **"Date range … must not
exceed one month."** Response 200: `{"status":"SUCCESS","payload":{"orders":[…]}}` — each
row a full GTT/OCO object per §7 [R:L436–L455] (the python-sdk example shows a trimmed row
[P:L528–L541]). Tip: "`ACTIVE` only returns live, untriggered smart orders." [P:L523–L524]

## 6. Status lifecycle enum + SDK constants (Verified-capture, [P:L548–L575])

```
groww.SMART_ORDER_TYPE_GTT / _OCO            # "GTT" / "OCO"
groww.TRIGGER_DIRECTION_UP / _DOWN           # "UP" / "DOWN"
groww.SMART_ORDER_STATUS_ACTIVE      # "ACTIVE"    - Order is monitoring trigger conditions
groww.SMART_ORDER_STATUS_TRIGGERED   # "TRIGGERED" - Trigger condition met, order placed
groww.SMART_ORDER_STATUS_CANCELLED   # "CANCELLED" - User cancelled the order
groww.SMART_ORDER_STATUS_EXPIRED     # "EXPIRED"   - Order expired due to time/date expiry
groww.SMART_ORDER_STATUS_FAILED      # "FAILED"    - Order placement or trigger failed
groww.SMART_ORDER_STATUS_COMPLETED   # "COMPLETED" - Order successfully completed
```

Six lifecycle values total. This is DISTINCT from the 12-value regular-order enum
(`13-annexures-enums.md`). TRIGGERED-vs-COMPLETED boundary prose is not given (Unknown —
plausibly TRIGGERED = leg placed, COMPLETED = leg executed; do not contract on this).

## 7. Smart Order Response schemas — full field tables (Verified-capture)

**GTT** [R:L465–L493], [P:L586–L613]: `smart_order_id` (str), `smart_order_type`,
`status`, `trading_symbol`, `exchange`, `quantity` (int), `product_type`, `duration`,
`order.order_type`, `order.price`, `order.transaction_type`, **`ltp` (number — the only
non-string price)**, `trigger_direction`, `trigger_price` (str), `segment`, `remark`
("Remark or status message"), `display_name`, **`child_legs` (object)**,
`is_cancellation_allowed` (bool), `is_modification_allowed` (bool), `created_at`,
`expire_at`, `triggered_at`, `updated_at` (all ISO 8601 strings, no timezone suffix).

**OCO** [R:L495–L519], [P:L615–L638]: `smart_order_id`, `smart_order_type`, `status`,
`trading_symbol`, `exchange`, `quantity`, `product_type`, `duration`,
`target.trigger_price` / `target.order_type` / `target.price`,
`stop_loss.trigger_price` / `stop_loss.order_type` / `stop_loss.price`,
`is_cancellation_allowed`, `is_modification_allowed`, `created_at`, `expire_at`,
`triggered_at`, `updated_at`. The OCO schema carries **no `ltp`/`remark`/`display_name`/
`segment`/`child_legs` rows** (GTT-only fields) — and, critically, **NO leg
`groww_order_id` field appears in EITHER schema** (verified absence — post-trigger
order linkage is undocumented, §9 P5).

## 8. Recovered semantics + contradictions (Verified-capture)

1. **CASH-OCO contradiction (intra-capture):** the REST page's header note says "OCO
   orders for `CASH` segment are currently not supported" [R:L17], while the SAME page's
   OCO schema says "For OCO in cash segment, only `MIS` is supported currently" [R:L155]
   and the modify list says "CASH OCO only supports MIS" [R:L212]; the python-sdk page
   drops the ban and says CASH OCO "is restricted to intraday (`MIS`) positions"
   [P:L17, L182]. Treat CASH-OCO availability as CONTRADICTED → probe (P9).
2. **OCO auto-cancel wording (verbatim, the only statement):** "If a leg executes, the
   other cancels automatically." [R:L190] — cancel LOCUS (exchange-side vs Groww-server
   monitoring) and PARTIAL-fill behavior are stated NOWHERE in the capture (grep-verified
   over all 26 pages: only match for "partial" is a changelog versioning sentence).
3. **GTT expiry:** still NO prose anywhere; the +1-year `expire_at` remains
   example-derived ([R:L93–L94]: created 2025-09-30 → expires 2026-09-30). The EXPIRED
   status ("expired due to time/date expiry" [P:L572]) confirms the mechanism exists;
   OCO create shows `expire_at: null` [R:L178].
4. **`reference_id`:** idempotency-key wording identical on GTT and OCO; "Use a unique
   `reference_id` per new smart order to avoid accidental duplicates." [R:L459].
   Uniqueness WINDOW/scope (per-day vs forever; GA007 trigger conditions) — still
   stated nowhere.
5. **Rate-limit family:** the official 3-family table (Orders 10/s·250/min, etc.)
   enumerates only "Create, Modify and Cancel Order" — `/v1/order-advance/*` appears in
   NO family row in either intro page (capture `01-introduction.md:156–160`,
   `15-BONUS-REST-introduction.md:281–285`). Unchanged Unknown (U-4 class).
6. **Push stream:** the feed page (capture `07-feed.md`) contains ZERO smart-order
   mentions (grep-verified) — smart-order state changes on the order-update stream
   remain undocumented.
7. **All prices are decimal strings** on the smart-order surface (except response `ltp`);
   "All prices should be passed as decimal strings" [P:L581].

## 9. Day-0 live-probe checklist (the residual Unknowns)

Built from the 2026-07-14 recovery: the 2026-07-13 12-item unknown list MINUS items
resolved above. Every probe is ONE bounded call (or one support question) on the box,
inside the Orders 10/s·250/min envelope, `dry_run`-class instruments, and requires the
dated operator authorization for ANY order-path call FIRST (scope rule §33/§38.8 — no
smart-order call is authorized today).

| # | Probe | How (one bounded call) | Expected evidence |
|---|---|---|---|
| P1 | `child_legs` object structure (GTT bracket legs) | Support question to Groww API team; optionally ONE `POST /v1/order-advance/create` (GTT) with a deliberately-shaped `child_legs: {"target":{…},"stop_loss":{…}}` on a 1-share CASH symbol far from trigger, then immediate cancel — read the validation error/echo | The accepted/rejected field names in the error body or the create echo |
| P2 | Rate-limit family of `/v1/order-advance/*` (+ does one OCO create bill 1 or 2 requests?) | Re-read the LIVE rate-limit table from the box (unblocked network) for a 4th row; else a bounded 11-req/s burst of `GET /v1/order-advance/list` and observe which family 429s | 429 correlation with Orders vs Non-Trading budget |
| P3 | OCO cancel locus (exchange vs Groww-side) + PARTIAL-fill-of-one-leg behavior | Support question (no safe bounded probe — needs a real position + fill) | Written statement; else one supervised live OCO round-trip when order-path is authorized |
| P4 | GTT validity prose — is `expire_at` always create+1y? Is it modifiable? | ONE GTT create (far-from-trigger, then cancel); read `expire_at` delta | `expire_at − created_at` on a real order |
| P5 | Post-trigger linkage — does GET/list expose the fired leg's `groww_order_id` after TRIGGERED? | ONE armed GTT that triggers (supervised), then `GET /v1/order-advance/status/...` + `GET /v1/order/list` | New field(s) appearing post-trigger, or correlation via `remark`/reference_id |
| P6 | Smart-order events on the order-update push stream | Same armed-GTT session with `subscribe_*_order_updates` open | An event at trigger time (or its absence) |
| P7 | `order_status: "OPEN"` vs the 12-value annexure enum (regular orders) | ONE resting LIMIT order far from market, `GET /v1/order/status/{id}` | The literal wire enum value |
| P8 | Regular modify `quantity` semantics (total vs remaining) | ONE partially-fillable order + ONE modify; compare `remaining_quantity` | Whether modify quantity replaces total or remaining |
| P9 | CASH-OCO availability (the §8.1 contradiction) + CNC-on-FNO rejection | ONE OCO create on CASH/MIS expected to succeed-or-GA-reject; ONE GTT create with `product_type: CNC` + `segment: FNO` | GA code + message for each |
| P10 | `reference_id` uniqueness window (GA007 trigger conditions) | Re-submit yesterday's reference_id once (after its order is terminal) | GA007 (window ≥ cross-day) or success (per-day/terminal-scoped) |

Resolved off the old list and REMOVED from probing: OCO create schema (old #1), modify/
get/list schemas (#2), status enum + constants (#3) — §2–§7 above. Old #4 (`child_legs`)
→ P1; #5 → P2; #6 → P3; #7 → P4+P5; #8 → P6; #9 → P7; #10 → P8; #11 → P9; #12 → P10.
