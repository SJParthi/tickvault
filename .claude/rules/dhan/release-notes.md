# Dhan Release Notes Enforcement

> **Ground truth:** `docs/dhan-ref/16-release-notes.md`
> **Scope:** Any file touching API version checks, feature flags, or backward compatibility handling.
> **Cross-reference:** All other `docs/dhan-ref/` files

## Mechanical Rules

1. **Read the ground truth first.** Before checking API version compatibility or adding version-dependent logic: `Read docs/dhan-ref/16-release-notes.md`.

2. **Always use v2.** v1 is deprecated. Base URL: `https://api.dhan.co/v2/`.

3. **Breaking changes to be aware of:**
   - v2.4: 24-hour token validity (SEBI), static IP mandatory for Order APIs
   - v2.3: Order API rate limit changed to 10/sec
   - v2.2: Data API rate limits changed to 5/sec, 100K/day
   - v2.0: Epoch timestamps replaced Julian time; `quantity` in modify = total qty; `symbol` → `securityId`; DH-900 error codes; new instrument CSV columns

4. **Features by version:**
   - v2.5: Conditional triggers, P&L exit, exit-all, programmatic token gen
   - v2.4: API Key login (12-month)
   - v2.3: 200-level depth, expired options data
   - v2.2: Super Orders, User Profile API
   - v2.1: 20-level depth, Option Chain API
   - v2.0: Market Quote REST, Forever Orders, Live Order Update WS, Margin Calculator

5. **When implementing a feature, verify it exists in current API version.** Don't implement features from pre-v2.0 that were removed.

## What This Prevents

- Using v1 endpoints → deprecated, may stop working
- Assuming pre-v2.0 field names → wrong field mapping
- Missing breaking change awareness → subtle bugs (e.g., Julian vs epoch timestamps)

## Trigger

This rule activates when editing files matching:
- `crates/common/src/constants.rs` (API version constants)
- Any file containing `DHAN_API_VERSION`, `api_version`, `v2/`, `breaking_change`
