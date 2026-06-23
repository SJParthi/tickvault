# Implementation Plan: Live-Feed Health SP6 — verdict badges on the /feeds page

**Status:** APPROVED
**Date:** 2026-06-23
**Approved by:** Parthiban ("yes go ahead dude")

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md` — all **15 rows** of the
100% guarantee matrix and all **7 rows** of the resilience demand matrix apply.
SP6 specifics: pure front-end change to one self-contained page string (no Rust
hot path, no new server route, no secret surface); XSS-proof by construction
(`textContent` only); the verdict colour/label come from a fixed client-side
table, never from server input.
**Honest 100% claim:** 100% inside the tested envelope, with ratcheted regression coverage — the page renders exactly the registry-backed `GET /api/feeds/health`
verdict (no false-green/false-red), serialised refresh (in-flight guard), bearer-
gated, ratcheted by the new `feeds_page::tests`.

## Design

SP1–SP5.1 made `GET /api/feeds/health` truthful per feed (Dhan + Groww: connected +
freshness + drops). SP6 renders that on the existing operator `/feeds` page so the
operator *watches* each feed's light live instead of reading JSON. Contained in
`crates/api/src/handlers/feeds_page.rs` (the self-contained HTML+JS page).

1. **Each existing feed row gains a verdict badge.** The page already renders one
   row per `Feed::ALL` (from the injected descriptor JSON) with an on/off switch.
   SP6 adds, on the same row, a coloured **verdict badge** (🟢 OK / 🟡 DEGRADED /
   🔴 DOWN / ⚪ DISABLED / 🟠 UNKNOWN) + a compact health meta line (reason +
   connected + last-tick age + ticks + drops).
2. **`refresh()` also fetches `GET /api/feeds/health`** (same bearer auth as the
   existing `/api/feeds` calls) and builds a `health[feed.key]` map, passed into
   `render(data, health)`. On 401 → the existing unauthorized path. The health
   fetch is best-effort: if it fails, the on/off rows still render (the badge
   shows a neutral "—").
3. **Auto-refresh** via `setInterval(refresh, 5000)` so the lights update live
   (the operator's "watch in real time"). A single timer; the existing manual
   Save/refresh still works.
4. **Verdict → colour** is a pure JS lookup table; rendering stays
   `createElement` + `textContent` ONLY (no `innerHTML` of any server value) —
   XSS-proof by construction, preserving the existing security guard.

**Honest scope:** the page shows the verdict + the signals that drive it
(connected, last-tick age, ticks, drops). `candles_total` is intentionally NOT
featured (Dhan's is 0 until the cosmetic seal-writer wiring lands — a tiny SP6.1
follow-up); the verdict never depends on candles, so the page is fully meaningful
without it.

## Edge Cases

- `/api/feeds/health` 401 → same "enter token" path as `/api/feeds` (badge shows
  "—" until authed).
- Health fetch fails but `/api/feeds` succeeds → rows render with a neutral badge;
  no broken page (best-effort health overlay).
- A feed present in `/api/feeds` but missing from the health payload → neutral
  badge (defensive; both iterate `Feed::ALL` so this shouldn't happen).
- Unknown verdict string from the server → falls back to the neutral badge class
  (no crash, no mis-colour).
- `last_tick_age_secs == null` → "no tick yet" (not "null s ago").
- All values via `textContent` → a hostile reason/feed string can never inject markup.

## Failure Modes

- Pure front-end change; no Rust hot path, no new server route (the endpoint
  already exists from SP3). The only server code is the page string + its tests.
- If the page JS throws, the existing `try/catch` in `refresh()` sets the status
  line; the page degrades to the on/off rows.

## Test Plan

- `crates/api` page tests (extend `feeds_page::tests`):
  - `test_feeds_page_fetches_health_endpoint` — page calls `GET /api/feeds/health`.
  - `test_feeds_page_renders_verdict_badge` — badge element + verdict text present.
  - `test_feeds_page_has_all_five_verdict_colours` — ok/degraded/down/disabled/unknown CSS classes present.
  - `test_feeds_page_auto_refreshes` — `setInterval` present for live updates.
  - existing guards still pass: no innerHTML of data, CSP, calls /api/feeds, row per Feed::ALL, fully dynamic, lane-not-running honesty.
- `cargo test -p tickvault-api --lib feeds` green; `cargo fmt --check`; banned-pattern + plan-gate + per-item-guarantee PASS.
- Adversarial review (security + hostile) on the diff — XSS/CSP/auth/false-OK-on-page. (hot-path N/A — it's a webpage.)

## Rollback

Pure additive to one static string + its tests. Revert = drop the health-fetch +
badge render + CSS + the interval. No API, schema, or behaviour change; the on/off
control is untouched.

## Observability

The page IS the observability surface — it renders the existing
`GET /api/feeds/health` verdicts (which are backed by the registry + the
SP1–SP5.1 ratchets). No new metric. The badge never shows a false green: it
renders exactly the server verdict (which is `down` on disconnect/silence,
`degraded` on drops, `unknown` if un-instrumented).

## Plan Items

- [ ] Add verdict-badge CSS (5 colours) + health-fetch + per-row badge render + 5s auto-refresh to `feeds_page.rs`
  - Files: crates/api/src/handlers/feeds_page.rs
  - Tests: test_feeds_page_fetches_health_endpoint, test_feeds_page_renders_verdict_badge, test_feeds_page_has_all_five_verdict_colours, test_feeds_page_auto_refreshes

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan live, ticks flowing | 🟢 OK badge, "last tick Ns ago · N ticks" |
| 2 | Dhan disconnected in market hours | 🔴 DOWN badge, "disconnected" |
| 3 | Dhan dropping ticks | 🟡 DEGRADED badge, "ticks dropped" |
| 4 | Groww switched off | ⚪ DISABLED badge |
| 5 | A feed enabled but un-instrumented | 🟠 UNKNOWN badge |
| 6 | Health endpoint 401 | neutral "—" badge + "enter token" |
| 7 | Hostile reason/label string | rendered as text, never markup (textContent) |
