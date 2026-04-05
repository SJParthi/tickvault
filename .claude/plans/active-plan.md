# Implementation Plan: Regulatory Compliance + Doc Alignment + Test Fixes

**Status:** VERIFIED
**Date:** 2026-04-05
**Approved by:** Parthiban

## Context

Deep audit of entire codebase via 6 parallel research agents revealed:
- 3 Dhan regulatory changes (MPP, rate limits, static IP) effective March/April 2026
- Forever Order endpoint URL mismatch (`/forever/all` → `/forever/orders`)
- SDK v2.2.0rc1 alignment gaps (`client-id` header, `PRE_OPEN` AMO)
- New GetIP response fields not documented (`detectedIP`, `ipMatchStatus`, `ordersAllowed`)
- Binary protocol: ALL byte offsets, packet sizes, field types confirmed correct
- All 31 gaps: TESTED (230+ tests)
- Tick durability: 3-layer defense confirmed working

## Plan Items

### Block A: Regulatory Compliance (CRITICAL)

- [x] A1. Update `docs/dhan-ref/07-orders.md` — add MPP section explaining market order auto-conversion to limit orders
  - Files: docs/dhan-ref/07-orders.md
  - Tests: existing schema_validation tests cover order types

- [x] A2. Update `docs/dhan-ref/02-authentication.md` — add GetIP new response fields + April 1 enforcement warning
  - Files: docs/dhan-ref/02-authentication.md
  - Tests: existing ip_monitor tests

- [x] A3. Fix Forever Order list endpoint from `/forever/all` to `/forever/orders`
  - Files: docs/dhan-ref/07b-forever-order.md
  - Tests: existing gap_enforcement tests

- [x] A4. Update `.claude/rules/dhan/orders.md` — add MPP handling rule
  - Files: .claude/rules/dhan/orders.md
  - Tests: N/A (enforcement rule, not code)

- [x] A5. Update `.claude/rules/dhan/authentication.md` — add April 1 enforcement severity
  - Files: .claude/rules/dhan/authentication.md
  - Tests: N/A (enforcement rule, not code)

### Block B: SDK v2.2.0rc1 Alignment (HIGH)

- [x] B1. Update `docs/dhan-ref/01-introduction-and-rate-limits.md` — note `client-id` header on all requests
  - Files: docs/dhan-ref/01-introduction-and-rate-limits.md
  - Tests: N/A (doc update)

- [x] B2. Update `docs/dhan-ref/10-live-order-update-websocket.md` — add `orderNo` alias note
  - Files: docs/dhan-ref/10-live-order-update-websocket.md
  - Tests: N/A (doc update)

- [x] B3. Update `docs/dhan-ref/16-release-notes.md` — add March/April 2026 regulatory changes
  - Files: docs/dhan-ref/16-release-notes.md
  - Tests: N/A (doc update)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Place MARKET order via API | Dhan converts to LIMIT with MPP — order book shows LIMIT type |
| 2 | Order from non-whitelisted IP after April 1 | Order REJECTED by exchange |
| 3 | GET /v2/ip/getIP response | Includes detectedIP, ipMatchStatus, ordersAllowed |
| 4 | GET /v2/forever/orders | Returns array of all forever orders |
| 5 | SDK sends client-id on all requests | No auth failures |
