# Implementation Plan: feed-control follow-ups (toggle / option-chain / per-feed identity)

**Status:** APPROVED
**Date:** 2026-06-23
**Approved by:** Parthiban — chat 2026-06-23 (AskUserQuestion answers below).
**Branch:** `claude/cool-noether-3liwxc`

## Consolidated queue (everything captured, nothing dropped)

| # | Item | Decision | Size | Status |
|---|---|---|---|---|
| **A** | **Feed ON/OFF not working in `/feeds` webpage** | Tokenless toggle in dev/sandbox (operator approved) | small | **THIS PR** |
| **B** | Remove `option_chain_minute_snapshot` table + its writer/scheduler ONLY | Keep the prev-day-OI fetch so candle `oi_pct_from_prev_day` survives (operator approved "remove only the snapshot table") | medium | next PR |
| **C** | Per-feed instrument identity + `feed` on data+master tables | Dedicated design PR (operator approved). **Correction:** Groww `exchange_token` max = 1,175,236 → fits u32; NO ID-width change. Per-feed native ID in `security_id`, `feed` distinguishes, `isin` bridges. | large | design PR |
| Q5 | WS-GAP-06 illiquid-SID tick gaps | investigate (likely market reality) | — | queued |
| Q6 | Telegram egress failures | environment (network) | — | queued |

## Item A — Design (THIS PR)

The `/feeds` page loads (read is public). But flipping a feed = `POST /api/feeds/{feed}`,
which is bearer-protected; the page sends no token, and on the operator's Mac auth IS
enabled (SSM token exists) → 401 `GAP-SEC-01: API auth failed — missing Authorization header`.

**Fix:** in dry-run/sandbox (no real orders), the mutating feed-toggle route is PUBLIC so
the operator flips feeds tokenless on localhost. In live trading (`dry_run=false`) it stays
bearer-protected. Threaded via a `dry_run`/`feed_toggle_public` arg into
`build_router_with_auth`; the Dhan-disable safety gate (`can_disable_dhan`) is UNCHANGED.

## Edge Cases
- dry_run=true → POST public (200, tokenless); GET already public.
- dry_run=false (live) → POST still 401 without token (security preserved).
- Dhan-disable while live trading → still 409 via existing `can_disable_dhan` gate.

## Failure Modes
- If the flag is mis-threaded false in dev → operator just can't toggle (same as today, no regression).
- No new panic/alloc; cold-path boot wiring only.

## Test Plan
- `test_feeds_post_public_200_without_token_in_dry_run` (new)
- `test_feeds_post_requires_auth_401_without_token` (live mode — keep, pass dry_run=false)
- `cargo test -p tickvault-api --lib`, `cargo clippy --workspace -- -D warnings`, `cargo fmt --check`

## Rollback
- Single api-crate change; `git revert`. Item is independent.

## Observability
- No new ERROR codes. The 401 WARN simply stops firing in dev because the route is public.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | dev: flip Dhan/Groww on page, no token | toggles, applies live |
| 2 | live: flip without token | 401 (protected) |
| 3 | live: disable Dhan with token while trading | 409 (safety gate) |

## Per-item guarantee matrix
Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`. Honest envelope: this
PR fixes one bounded auth-UX issue with regression tests both directions (dev public / live
authed); no hot-path change; O(1) route wiring.
