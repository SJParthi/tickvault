# Dhan Postback (Webhooks) Enforcement

> **Ground truth:** `docs/dhan-ref/07e-postback.md`
> **Scope:** Any file touching postback/webhook URL configuration, order update webhook receiving, or postback payload parsing.
> **Cross-reference:** `docs/dhan-ref/10-live-order-update-websocket.md` (alternative real-time method)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any postback handler: `Read docs/dhan-ref/07e-postback.md`.

2. **Postback = HTTP POST webhook from Dhan on every order status change.** JSON payload. Tied to access token — one webhook URL per token.

3. **Setup is on web.dhan.co** when generating access token. Cannot be configured via API.

4. **No localhost or 127.0.0.1.** Must be a publicly accessible URL.

5. **Triggered on:** `TRANSIT`, `PENDING`, `REJECTED`, `CANCELLED`, `TRADED`, `EXPIRED`, modification, partial fill.

6. **`filled_qty` is snake_case** — inconsistent with camelCase elsewhere in Dhan API. Use exact field name.

7. **`legName`** — populated for Super Order legs (`ENTRY_LEG`, `TARGET_LEG`, `STOP_LOSS_LEG`). Null otherwise.

8. **Timestamps are IST strings** (`YYYY-MM-DD HH:MM:SS`), NOT epoch.

9. **Prefer Live Order Update WebSocket** (`docs/dhan-ref/10-live-order-update-websocket.md`) for tickvault — no public URL needed.

## What This Prevents

- Localhost webhook URL → Dhan cannot reach it → no updates received
- Wrong `filled_qty` field name (camelCase) → deserialization failure
- Relying solely on postback without WebSocket fallback → single point of failure

## Trigger

This rule activates when editing files matching:
- `crates/core/src/postback/*.rs`
- `crates/api/src/handlers/postback.rs`
- Any file containing `PostbackPayload`, `postback_url`, `filled_qty`, `boProfitValue`, `boStopLossValue`, `legName`
