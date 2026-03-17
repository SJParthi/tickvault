# Implementation Plan: Fix All Audit Discrepancies Between Dhan Reference Docs and Code

**Status:** VERIFIED
**Date:** 2026-03-17
**Approved by:** Parthiban

## Summary

The comprehensive audit found zero bugs but identified discrepancies where code naming/coverage doesn't exactly match `docs/dhan-ref/08-annexure-enums.md`. This plan fixes every discrepancy while preserving O(1) hot-path guarantees.

## Plan Items

- [x] **1. Fix disconnect code constant names (808/809 swap) + add 800,804,810-814**
  - Files: `crates/common/src/constants.rs`
  - Rationale: Reference doc says 808="Authentication Failed", 809="Access token invalid", 810="Client ID invalid". Current code has 808=InvalidClientId and 809=AuthFailed — names are swapped vs reference.
  - Add constants for all 12 Data API error codes from annexure Section 11: 800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814
  - Tests: compile-time usage in DisconnectCode

- [x] **2. Add missing feed/interval constants**
  - Files: `crates/common/src/constants.rs`
  - Add: `FEED_REQUEST_CONNECT = 11` (annexure Section 2, code 11=Connect Feed)
  - Add: `DHAN_CANDLE_INTERVAL_25MIN = "25"` (reference doc 05-historical-data.md lists "25" as valid interval)
  - Tests: none needed (constants only)

- [x] **3. Update DisconnectCode enum with all 12 reference doc variants**
  - Files: `crates/core/src/websocket/types.rs`
  - Add named variants: InternalServerError(800), InstrumentsExceedLimit(804), ClientIdInvalid(810), InvalidExpiryDate(811), InvalidDateFormat(812), InvalidSecurityId(813), InvalidRequest(814)
  - Fix naming: 808→AuthenticationFailed, 809→AccessTokenInvalid
  - Reconnectability: 800,807=reconnectable; all others=NOT reconnectable
  - Only 807 requires token refresh (unchanged)
  - Tests: update all unit tests in types.rs

- [x] **4. Add documentation comments on SDK-derived / practical values**
  - Files: `crates/common/src/constants.rs`, `crates/common/src/order_types.rs`
  - RESPONSE_CODE_MARKET_DEPTH=3: clarify "Not in annexure Section 3 (gap between Ticker(2) and Quote(4)); documented in DhanHQ Python SDK"
  - OrderStatus::Confirmed: clarify "Not in annexure Section 5; observed in Dhan WS order update responses"
  - OrderStatus::Expired: clarify "Not in annexure Section 5; used in Order Book API (docs/dhan-ref/07-orders.md)"
  - ProductType::Co, Bo: clarify "Not in annexure Section 4; from WS order update single-char Product codes (V=CO, B=BO)"
  - Tests: none (doc comments only)

- [x] **5. Update gap enforcement rule to match reference doc**
  - Files: `.claude/rules/project/gap-enforcement.md`
  - Fix WS-GAP-01: codes 801,802,803 don't exist in annexure Section 11. Correct list is 800,804,805,806,807,808,809,810,811,812,813,814 (12 codes)
  - Fix reconnectable/non-reconnectable classification to match reference doc semantics

- [x] **6. Update gap enforcement tests**
  - Files: `crates/core/tests/gap_enforcement.rs`
  - Update `roundtrip_all_known_codes` to use correct codes from reference doc
  - Update reconnectable/non-reconnectable tests to match new named variants
  - Tests: `roundtrip_all_known_codes`, `reconnectable_codes`, `non_reconnectable_codes`, `only_807_requires_token_refresh`

- [x] **7. Fix CLAUDE.md market status packet size**
  - Files: `CLAUDE.md`
  - Change "MarketStatus (code 7) = **10 bytes**" to "**8 bytes**" (matches SDK `<BHBI>` = 8 bytes, code constant, and parser)

- [x] **8. Update disconnect parser to use new constant names**
  - Files: `crates/core/src/parser/disconnect.rs` (if it references renamed constants)
  - Ensure all imports/usages of renamed disconnect constants compile

## O(1) Impact Assessment

**ZERO hot-path impact.** All changes are:
- Constant definitions (compile-time)
- Cold-path enum construction (WebSocket disconnect handling — not per-tick)
- Documentation comments
- Test files
- Rule files

No changes to tick parsing, candle aggregation, ring buffers, dedup, or any data path.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Disconnect code 808 received | Maps to AuthenticationFailed (was InvalidClientId) — non-reconnectable |
| 2 | Disconnect code 809 received | Maps to AccessTokenInvalid (was AuthenticationFailed) — non-reconnectable |
| 3 | Disconnect code 810 received | Maps to ClientIdInvalid (was Unknown) — now non-reconnectable (was reconnectable) |
| 4 | Disconnect code 800 received | Maps to InternalServerError (was Unknown) — reconnectable |
| 5 | Disconnect code 814 received | Maps to InvalidRequest (was Unknown) — now non-reconnectable (was reconnectable) |
| 6 | Unknown code (e.g., 999) | Still maps to Unknown — reconnectable |
| 7 | All existing tests pass | No behavioral regression after test updates |
