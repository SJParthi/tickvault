# Implementation Plan: Comprehensive Operator Dashboard Page at `/dashboard`

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — 2026-06-29 ("fix everything dude", in direct
response to me listing "Part 2 — the full everything dashboard page").

> **Guarantee matrix cross-reference (MANDATORY — `per-item-guarantee-check.sh` gate):**
> This plan and its single item carry the full 15-row + 7-row guarantee matrix below
> AND cross-reference the canonical
> `.claude/rules/project/per-wave-guarantee-matrix.md`. Every one of the 15 + 7 rows
> is filled in honestly for an **api-crate cold-path web handler**: the hot-path /
> DHAT / Criterion / DEDUP / O(1)-tick rows are explicitly `N/A — <reason>` (this is a
> static HTML page handler, no hot path, no new DB table, no tick processing); the
> real proof rows are the cargo `tickvault-api` tests, the route wiring, and the
> HTML-section assertions.

---

## Design

**Goal.** One comprehensive operator dashboard webpage where the operator sees
EVERYTHING in one view: per-feed status (Dhan + Groww on/off + health + last error
reason in plain English), live tick counts per feed, latest candle timestamps,
table row counts, and overall app health — auto-refreshing.

**Before.** The app serves only the Feed-Control toggle page at `GET /feeds`
(`crates/api/src/handlers/feeds_page.rs`). There is NO comprehensive dashboard.

**After.** A new self-contained HTML page at `GET /dashboard` (and `GET /` redirects
to it) that client-side fetches the EXISTING JSON endpoints and renders a single
operator view with ~5s auto-refresh.

**Key design decision — reuse existing endpoints, add NO new backend.**
Investigation of the existing JSON surface showed every datum the dashboard needs is
already exposed:

| Need | Existing endpoint | Field(s) |
|---|---|---|
| Per-feed ON/OFF + lane running | `GET /api/feeds` | `dhan_enabled`, `groww_enabled`, `dhan_lane_running`, `groww_lane_running` |
| Per-feed health verdict + **plain-English reason** | `GET /api/feeds/health` | `verdict`, `reason`, `connected`, `auth_rejected` |
| Per-feed live tick counts + last tick age | `GET /api/feeds/health` | `ticks_total`, `last_tick_age_secs`, `drops_total` |
| Per-feed candle count + subscribe/decode proof | `GET /api/feeds/health` | `candles_total`, `subscribed_total`, `decoded_emitted`, `decoded_dropped` |
| Table row counts / DB stats | `GET /api/stats` | `tables`, `underlyings`, `derivatives`, `subscribed_indices`, `ticks`, `questdb_reachable` |
| Overall app health + subsystems | `GET /health` | `status`, `subsystems.{websocket,order_update,questdb,token,pipeline,tick_persistence}` |
| Latest candle / latest tick per SID | `GET /api/quote/{security_id}` | `timestamp`, `feed`, `last_traded_price`, `day_close` |

The **Groww "connected-but-0-ticks" / entitlement-error reason is ALREADY a first-class
field** (`/api/feeds/health` → `reason` + `auth_rejected`, both registry-backed,
`&'static str`), so the page shows the REAL reason — never hardcoded/hallucinated.
Therefore a new `GET /api/dashboard` aggregate endpoint is **NOT needed** and is
deliberately NOT added (smaller surface, no new attack surface, reuses the proven,
already-tested handlers). This is the "prefer reusing existing endpoints" path.

**Scope = api crate + this plan file only.** NO Rust hot-path, NO indicator/strategy
(`§28` frozen), NO Groww sidecar, NO new heavy backend, `/feeds` left untouched.

**Implementation:** mirror `feeds_page.rs` exactly — a `const DASHBOARD_HTML`
returned via `Html(...)`, served with the same `X-Frame-Options: SAMEORIGIN` +
Content-Security-Policy hardening. All rendered values use `textContent` (never
`innerHTML`) so the page is XSS-proof by construction. Plain-English, emoji status
(✅/⚠️/🆘), no library names / file paths / version numbers in the visible UI text
(operator-charter §D / Telegram-10-commandments spirit applied to operator-facing UI).

---

## Edge Cases

1. **An endpoint returns 401** (auth on a future live build): the page surfaces
   "enter your API token" and keeps the token in `sessionStorage`, like `/feeds`. The
   other panels still render what they can (each fetch is independent + best-effort).
2. **An endpoint is unreachable / network error**: that panel shows a neutral "—" /
   "could not read" — NEVER a false green. Honesty over false-OK (audit Rule 11).
3. **QuestDB down**: `/api/stats` returns `questdb_reachable:false` + all-zero counts;
   the page shows ⚠️ for the DB panel, not a crash.
4. **A feed has 0 ticks but is connected** (the exact Groww entitlement case): the
   health row's `reason` + `auth_rejected` are shown prominently so the operator sees
   WHY (e.g. "refresh the api-key" / "no tick yet").
5. **A future 3rd feed** is added to `Feed::ALL`: `/api/feeds/health` returns its row
   automatically; the page iterates whatever rows the API returns (no hardcoded 2-feed
   list), so it surfaces the new feed with zero page edits.
6. **Concurrent auto-refresh + manual refresh**: an in-flight guard (`refreshing`)
   serialises refreshes so a slow network can't double-render.
7. **Hidden browser tab**: polling is skipped while `document.hidden` (no needless load).
8. **`last_tick_age_secs` is null** (no tick yet): shown as "no tick yet", not "0s ago".

---

## Failure Modes

| Failure | Behaviour | Operator signal |
|---|---|---|
| `/health` 5xx / unreachable | health panel shows "—" | neutral, never false-green |
| `/api/feeds` 401 | "enter API token" prompt | actionable |
| `/api/feeds/health` error | per-feed badge shows "—" (health unavailable) | tooltip explains |
| `/api/stats` `questdb_reachable:false` | DB panel ⚠️, zero counts | honest |
| handler panic | impossible — handler is a pure `Html(const)` return, no `unwrap`/`expect`/IO | n/a |
| XSS via a server string (e.g. a future runtime `reason`) | impossible — `textContent` only, no `innerHTML` | n/a |
| clickjacking | blocked — `X-Frame-Options: SAMEORIGIN` + CSP `frame-ancestors 'self'` | n/a |

The handler itself has **no failure path**: it returns a compile-time-constant HTML
string with static security headers. There is no IO, no `Result`, no `unwrap`,
no `expect`, no `.clone()` on a hot path (it is a cold-path web route hit at most a
few times per operator session).

---

## Test Plan

`cargo test -p tickvault-api` (scoped per `testing-scope.md` — api crate only):

1. `test_dashboard_page_is_an_html_document` — `DASHBOARD_HTML` starts with
   `<!DOCTYPE html>` and has the title.
2. `test_dashboard_page_uses_no_innerhtml_for_rendered_data` — renders via
   `textContent`; no `innerHTML` assignment of fetched values (XSS-proof).
3. `test_dashboard_page_csp_blocks_clickjacking` — CSP carries `frame-ancestors 'self'`.
4. `test_dashboard_page_fetches_all_required_endpoints` — page references `/health`,
   `/api/feeds`, `/api/feeds/health`, `/api/stats` (guards page↔API drift).
5. `test_dashboard_page_has_all_section_markers` — the body contains the structural
   section/metric markers: Feeds, Overall health, Database, ticks, candles, last tick,
   underlyings, derivatives. (Feed NAMES `dhan`/`groww` are deliberately NOT hardcoded —
   see `test_dashboard_page_does_not_hardcode_feed_names`; the page iterates whatever
   feed rows `/api/feeds/health` returns so a future 3rd feed appears with zero edits.)
6. `test_dashboard_page_surfaces_feed_reason` — page renders the per-feed `reason`
   field (the Groww last-error / "0 ticks" cause).
7. `test_dashboard_page_auto_refreshes` — `setInterval(... refresh ...)` present.
8. Router-level (in `lib.rs` tests):
   - `test_build_router_dashboard_endpoint_returns_200_html` — `GET /dashboard` →
     `200` + `Content-Type: text/html`, ships `X-Frame-Options: SAMEORIGIN`.
   - `test_build_router_root_redirects_to_dashboard` — `GET /` → 3xx redirect to
     `/dashboard`.

No new `/api/dashboard` JSON endpoint was added, so no JSON-shape unit test is
needed (reuses already-tested `/api/feeds`, `/api/feeds/health`, `/api/stats`,
`/health`).

---

## Rollback

- **Pure additive change.** Remove the `GET /dashboard` + `GET /` routes from
  `crates/api/src/lib.rs` and delete `crates/api/src/handlers/dashboard_page.rs`
  (and its `pub mod` line) to fully revert. `/feeds` and every existing route are
  untouched, so removal cannot regress any existing behaviour.
- No config flag, no DB migration, no schema change, no state change — nothing to
  un-migrate. `git revert <sha>` of the single PR is a complete rollback.

---

## Observability

- This is a read-only operator-facing web page composing existing instrumented
  endpoints. The underlying data already carries the project's 7-layer telemetry:
  `/api/feeds/health` is registry-backed (the same registry the feed lanes update +
  the `tv_feed_*` counters), `/api/stats` queries QuestDB live, `/health` reflects
  the live `SystemHealthStatus` atomics. The page is a VIEW over that telemetry —
  it adds no new counter, no new error path, no new audit row (correct: a static
  HTML route has nothing to instrument). The route is covered by the existing
  request-tracing middleware (`request_tracing`) applied to all routes in `lib.rs`.

---

## Plan Items

- [ ] **Item 1 — Comprehensive operator dashboard page at `/dashboard`**
  - Files:
    - `crates/api/src/handlers/dashboard_page.rs` (NEW — `const DASHBOARD_HTML`
      + `dashboard_page()` handler + `root_redirect()` handler + tests)
    - `crates/api/src/handlers/mod.rs` (add `pub mod dashboard_page;`)
    - `crates/api/src/lib.rs` (wire `GET /dashboard` + `GET /` redirect; router tests)
  - Tests: `test_dashboard_page_is_an_html_document`,
    `test_dashboard_page_uses_no_innerhtml_for_rendered_data`,
    `test_dashboard_page_csp_blocks_clickjacking`,
    `test_dashboard_page_fetches_all_required_endpoints`,
    `test_dashboard_page_has_all_section_markers`,
    `test_dashboard_page_surfaces_feed_reason`,
    `test_dashboard_page_auto_refreshes`,
    `test_build_router_dashboard_endpoint_returns_200_html`,
    `test_build_router_root_redirects_to_dashboard`

---

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Operator opens `/dashboard` | 200 HTML, all panels render, auto-refresh starts |
| 2 | Operator opens `/` | 3xx redirect → `/dashboard` |
| 3 | Groww connected but 0 ticks (entitlement) | Groww panel shows `reason` + ⚠️, real number `0 ticks` |
| 4 | QuestDB down | DB panel ⚠️, zero counts, no crash |
| 5 | Dhan healthy + streaming | Dhan panel ✅ + live tick count + last tick age |
| 6 | A future 3rd feed added | its row appears automatically (page iterates API rows) |
| 7 | Network blip on one fetch | that panel "—", others still render |

---

## The 15-row "100% Everything" Guarantee Matrix (per `per-wave-guarantee-matrix.md`)

| Demand | Mechanical proof / honest N/A for this item |
|---|---|
| 100% code coverage | New handler + tests added; coverage gate runs post-merge over the api crate. Page handler is a pure `Html(const)` return — fully exercised by the 200/HTML router test + the const-content tests. |
| 100% audit coverage | N/A — read-only web VIEW; no typed lifecycle event, no new `<event>_audit` table (a static HTML route has nothing to audit). |
| 100% testing coverage | Unit (const-content assertions) + integration (router `oneshot` 200/HTML + redirect) categories apply; declared in Test Plan. |
| 100% code checks | banned-pattern + pub-fn-test + plan-verify + secret-scan run pre-push; no `.clone()`/`unwrap`/`println` in the handler. |
| 100% code performance | N/A — cold-path web route, hit a few times per operator session; no hot path. No DHAT/Criterion needed (no per-tick work). |
| 100% monitoring | Existing `request_tracing` middleware covers the route; the data shown is already 7-layer-instrumented upstream. No new counter (nothing new to count). |
| 100% logging | No new error path (pure const return); upstream endpoints log via `error!`+`code`. |
| 100% alerting | N/A — the page raises no new failure mode; the underlying feeds/QuestDB already have their alert rules. |
| 100% security | `X-Frame-Options: SAMEORIGIN` + CSP `frame-ancestors 'self'`; XSS-proof via `textContent`-only rendering; no secrets in the HTML shell (token entered client-side, kept in `sessionStorage`); reviewed by security-reviewer agent. |
| 100% security hardening | CSP `base-uri 'none'; form-action 'none'`; no `innerHTML`; no reflected input. |
| 100% bugs fixing | Adversarial review of the diff (hot-path N/A, security, hostile) before PR. |
| 100% scenarios covering | 7-row Scenarios table above + the Edge Cases section. |
| 100% functionalities covering | Every new pub fn (`dashboard_page`, `root_redirect`) has a test AND a call site (wired in `lib.rs`). |
| 100% code review | adversarial agent pass on the diff before AND after impl. |
| 100% extreme check | The `test_dashboard_page_fetches_all_required_endpoints` + `test_dashboard_page_has_all_section_markers` ratchet tests FAIL the build if the page drifts from the API contract or drops a panel. |

## The 7-row "Resilience Demand" Matrix

| Demand | Honest envelope / N/A for this item |
|---|---|
| Zero ticks lost | N/A — read-only VIEW; introduces no tick path, touches no ring/spill/DLQ. |
| WS never disconnects | N/A — no WebSocket; the page only DISPLAYS the existing feeds' state. |
| Never slow/locked/hanged | Handler returns a compile-time-constant string — O(1), no allocation per request beyond the static-str copy; no hot-path concern. |
| QuestDB never fails | N/A here — the page READS `/api/stats` (which absorbs QuestDB-down as `questdb_reachable:false`); it adds no new QuestDB write path. |
| O(1) latency | The handler is O(1) (returns a `const &str` via `Html`); N/A for per-tick latency (no tick processing). |
| Uniqueness + dedup | N/A — no new QuestDB table, no DEDUP key (read-only page). |
| Real-time proof | The page auto-refreshes every ~5s and renders REAL registry-backed values (feed verdict/reason/ticks) + live QuestDB counts — never hardcoded numbers. |

---

## Honest 100% claim (envelope-qualified per `wave-4-shared-preamble.md` §8)

This dashboard page is "100% inside the tested envelope, with ratcheted regression
coverage": the handler returns a compile-time-constant HTML document (no IO, no
`unwrap`/`expect`, no failure path), served with `X-Frame-Options: SAMEORIGIN` + a
`frame-ancestors 'self'` CSP, rendering every fetched value via `textContent`
(XSS-proof by construction). It composes ONLY the existing, already-tested JSON
endpoints (`/health`, `/api/feeds`, `/api/feeds/health`, `/api/stats`,
`/api/quote/{id}`) and shows their REAL registry-backed / live-QuestDB values —
NEVER hardcoded or hallucinated numbers; an unreachable endpoint degrades that one
panel to a neutral "—" rather than a false green. The page↔API contract is pinned by
ratchet tests (`test_dashboard_page_fetches_all_required_endpoints`,
`test_dashboard_page_has_all_section_markers`) that fail the build on drift. Beyond
the tested envelope (e.g. a browser that blocks `fetch`), the page still renders its
static shell + the honest "could not read" panels. No claim is made that the page
itself prevents a feed disconnect or a QuestDB outage — it is a truthful VIEW over
those conditions, not a remedy for them.
