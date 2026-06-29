# Implementation Plan: Honest Groww feed status — market-closed idle ≠ "auth rejected"

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session (the
Feed Control page showed a CONNECTED+SUBSCRIBED-but-market-closed Groww feed as
"auth rejected — refresh the Groww SSM api-key · not connected · DOWN", sending
the operator to chase a non-existent SSM-key problem at 22:30 IST).

## Design

The `/feeds` page health line is assembled by `feeds_page.rs` JS from
`GET /api/feeds/health`, which serialises the `FeedHealthRegistry` snapshot
(`crates/api/src/handlers/feeds.rs`). The snapshot's verdict + reason come from
the pure classifier `classify()` in `crates/common/src/feed_health.rs`. Two
independent fields combine into the operator's contradictory line:

1. **`auth_rejected=true` wins over market-closed.** `classify()` checks
   `auth_rejected` (→ `Down("auth rejected — refresh the Groww SSM api-key")`,
   `feed_health.rs:173`) BEFORE the market-closed gate (`:189`). The flag is set
   `true` by the sidecar-stderr text classifier
   (`groww_sidecar_supervisor.rs:511` via `classify_sidecar_line`) — a heuristic
   substring match, not a real HTTP non-200. So a benign sidecar line latches the
   flag, and once latched outside market hours nothing clears it (the
   `record_ticks`-clears-on-recovery path can't fire when no tick can flow).
   Result: a feed that authed (`auth OK`) + subscribed (767) is shown DOWN with
   "refresh the SSM api-key" at 22:30 IST, market closed.

   **Fix (this plan):** move the **market-closed gate ABOVE the `auth_rejected`
   check** in `classify()`. Outside market hours no auth is attempted and no tick
   can flow, so a latched `auth_rejected` is *unverifiable stale state*, not a
   live confirmed rejection — surfacing it then is a false-RED (the mirror of the
   no-false-OK rule). This is identical in spirit to the existing C1 fix that
   already placed the market-closed gate above the `connected`/`last_tick` checks.
   During market hours the `auth_rejected` check still fires exactly as before
   (every existing market-hours auth-rejected test uses `market_open: true`).

2. **"not connected · subscribed 767" self-contradiction.** `connected` reflects
   STREAMING (`groww_bridge.rs:1070` sets it to `streaming_observed`), which is
   only true once a tick flips the sidecar status to `streaming`. With the market
   closed no tick arrives, so `connected=false` while `set_subscribed(767)` fired.
   The page JS (`feeds_page.rs:244`) prints a blunt "not connected" next to
   "subscribed 767".

   **Fix (this plan):** in the page JS, when a feed has a subscribe proof
   (`subscribed_total > 0`) but is not yet streaming (`!connected`), render
   "connected · awaiting first tick" instead of "not connected" — a successful
   subscribe IS proof the socket was connected (you cannot subscribe without a
   live socket). "not connected" is shown only when there is NO subscribe proof
   AND `!connected`. No new field, no API change — derived from the two existing
   `connected` + `subscribed_total` fields already in the health row.

Net operator-facing result for the reported scenario (Groww authed + subscribed
767, market closed):
- Before: `DOWN` · "auth rejected — refresh the Groww SSM api-key · not connected
  · subscribed 767 · no tick yet · 0 ticks".
- After: `LIVE` (verdict Ok, market-closed idle) · "market closed — idle is normal
  · connected · subscribed 767 · awaiting first tick · 0 ticks".

"auth rejected — refresh the Groww SSM api-key" now appears ONLY during market
hours (when auth is actually exercisable), never as a default for market-closed
silence.

## Edge Cases

- **Market OPEN + real auth rejection:** unchanged — `auth_rejected` check still
  runs (it is now second, after the market-closed gate which is false during
  market hours), so a genuine rejection during trading still shows the actionable
  Down. All existing market-hours auth-rejected tests (`market_open: true`) pass.
- **Disabled feed:** `Disabled` still wins (checked first, before market-closed).
- **Not started / not instrumented:** still win (checked before market-closed and
  before auth_rejected — order preserved for those; only the auth_rejected vs
  market-closed pair swaps).
- **Drops present + market closed:** the `drops_total > 0 → Degraded` check stays
  ABOVE the market-closed gate (a real drop today is surfaced post-close by
  design); only the auth_rejected check moves below it.
- **Page: connected + subscribed:** shows "connected" (subscribe branch only
  rewrites the `!connected` case).
- **Page: not connected + no subscribe proof:** still shows "not connected"
  (e.g. Dhan OFF: "switched off by operator · not connected · no tick yet").
- **Page: subscribed but disconnected DURING market hours:** verdict is the
  real Down (auth-rejected or "disconnected — reconnecting" / "no ticks yet"),
  and the line reads "<down reason> · connected · awaiting first tick" — the
  "connected" here means "did subscribe (socket was up)"; the DOWN reason already
  tells the operator it is not streaming, so no false-OK (the badge is RED).

## Failure Modes

- Pure classifier reorder + pure page-JS string derivation — no I/O, no new
  state, no schema, no config, no allocation on any hot path. The classifier is
  O(1) and called only from the cold `/api/feeds/health` read path.
- If the market-hours helper ever misreports (e.g. clock skew), worst case is the
  same as today's behaviour for that window (it already gates `connected` checks);
  no new failure introduced.
- Page JS is XSS-safe by construction (textContent only) — the new branch adds
  only static strings.

## Test Plan

`cargo test -p tickvault-common feed_health` (classifier):
- NEW `test_market_closed_idle_wins_over_stale_auth_rejected` — `auth_rejected:
  true, market_open: false` → verdict `Ok`, reason "market closed — idle is
  normal" (the operator's exact scenario). Pins the new ordering.
- Existing `test_auth_rejected_is_down_with_refresh_message`,
  `test_auth_rejected_wins_over_disconnected_during_market` (both `market_open:
  true`) still pass — auth-rejected during market hours unchanged.
- Existing `test_ok_when_market_closed_and_silent`,
  `test_disabled_wins_over_auth_rejected` still pass.

`cargo test -p tickvault-api feeds_page` (page render):
- NEW `test_feeds_page_subscribed_implies_connected_when_not_streaming` — the
  rendered HTML carries the "awaiting first tick" wording + the
  `subscribed_total` / `connected` derivation, so a subscribed-but-not-streaming
  feed is not shown a blunt "not connected".

## Rollback

Two contained changes (a statement reorder in `feed_health.rs::classify` + a
string-derivation branch in the `feeds_page.rs` HTML JS) + tests. Revert the
single commit (`git revert <sha>`) restores the exact prior behaviour. No schema,
no config, no migration, no data, no API contract change. No feature flag needed
(a bug fix that makes the existing fields honest).

## Observability

No new counter / log / Telegram. The fix's EFFECT is already observable — the
`/feeds` page re-reads the live snapshot each 5s refresh and the verdict + reason
recompute. The existing `GrowwSidecarRejected` Telegram event (the SET edge) is
untouched, so a REAL market-hours rejection still pages exactly as before. The
`auth_rejected` AtomicBool still latches/clears via the existing set + recovery
paths; this only changes how a latched flag is PRESENTED when the market is
closed.

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all 15 rows of the
100% guarantee matrix and all 7 rows of the resilience demand matrix apply.
Specifics:
- 100% code coverage: new unit tests cover the new ordering branch + the page
  derivation; existing tests pin the unchanged market-hours behaviour.
- 100% testing coverage: unit (classifier) + render-string (page).
- 100% code performance / O(1): classifier stays a branch sequence over a
  `Copy` struct — O(1), zero-alloc; the page change is static-string assembly.
- Uniqueness + dedup: untouched (per-feed slot by `Feed::index()`).
- Real-time proof: `/feeds` snapshot recomputes the verdict each refresh.
- Zero ticks lost / WS / QuestDB / hot path: NONE touched — this is cold
  control-plane status presentation only.

### Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: outside
market hours a (possibly stale) `auth_rejected` flag no longer produces a
false-RED "refresh the SSM api-key" Down — the market-closed idle verdict wins,
proven by `feed_health` unit tests; during market hours a genuine rejection still
shows the actionable Down unchanged (existing tests, `market_open: true`); a
subscribed-but-not-yet-streaming feed reads "connected · awaiting first tick"
instead of a self-contradictory "not connected · subscribed 767". Beyond the
envelope (a clock-skew that misreports market hours), behaviour degrades to
today's existing market-hours-gated behaviour for that window — no new failure
mode.
