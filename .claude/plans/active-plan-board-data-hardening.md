# Implementation Plan: Board-Data Hardening — rate-limit + TTL-cache GET /api/board/data (follow-up to #1458)

**Status:** VERIFIED
**Date:** 2026-07-09
**Approved by:** Parthiban (operator) — coordinator-approved follow-up to the 2026-07-09 audit (PR #1458 flagged this HIGH residual)
**Changed crates:** api (`crates/api`)
**Guarantee matrices:** cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) — this item's specifics are carried in the PR body's guarantee-check block.

> **Residual context:** PR #1458 hardened `/api/stats` + `/api/quote` but its
> adversarial review flagged `GET /api/board/data` HIGH: the THIRD public
> QuestDB-backed route — **3 QuestDB HTTP round-trips per hit** (SHOW TABLES
> probe + 2 `SELECT count()` over today's `ticks` / `candles_1m`, fresh
> reqwest client each time, 3s timeout each) plus bounded file I/O — polled
> every ~3s by the public `/board` page and reachable by any internet client
> on the port-3001 funnel with no limit and no cache.

## Design

Reuse EVERYTHING from #1458 — no new machinery:

1. **Rate limit:** add `/api/board/data` to the EXISTING
   `limited_public_routes` sub-router in `build_router_with_auth`, behind
   the SAME shared `PublicEndpointLimiter` GCRA cell (5 req/s sustained +
   burst 10). `endpoint_label` gains a `"board"` arm (static, total —
   `/api/board` prefix) so `tv_api_rate_limited_total{endpoint="board"}`
   attributes 429s. The `HONEST RESIDUAL` comment block in
   `public_guard.rs` (which flagged this exact route) is REPLACED with the
   closure note.
2. **TTL cache:** a THIRD `SingleSlotTtlCache` in `SharedAppState`
   (`board_cache`, **TTL 2s** — `BOARD_CACHE_TTL_SECS`). The handler is
   restructured exactly like stats in #1458: `compute_board_data` extracted
   (the entire current body), `board_data` becomes the thin cache wrapper
   returning `Response` via the shared `cached_json_response` helper
   (`x-tv-cache: hit|miss`), `tv_api_cache_hits_total{endpoint="board"}` on
   hit. A cache HIT performs ZERO QuestDB round-trips and ZERO file I/O.
   Board always answers 200 with honest nulls (its locked contract), so the
   single slot caches whatever the last compute produced — bounded negative
   caching on a DB outage, same rationale as stats.

**TTL choice (2s) + budget math (verified against the page sources):**
- `/board` polls `/api/board/data` every **3s** (skipping hidden tabs) —
  `board_page.rs` `setInterval(..., 3000)` + `document.hidden` guard.
  A 2s TTL < 3s poll keeps every poll's data ≤2s stale (each single-tab
  poll typically recomputes — the cache exists for STORM protection, not
  legit-traffic dedup), and caps attack cost at one 3-query pass per 2s.
- Shared budget: dashboard tab = 1 stats req/5s (0.2 req/s) + board tab =
  1 req/3s (0.33 req/s) ≈ **0.53 req/s for one tab of each vs 5 req/s
  sustained** (~11% of budget). Even 5 simultaneous tabs of each ≈ 2.7
  req/s — still under sustained rate, never touching burst. Quote/MCP
  calls are occasional. No legit starvation; documented in code + PR.

**Unchanged (locked contracts):** `GET /api/feeds` + `GET /api/feeds/health`
public-read, `POST /api/feeds/{feed}` bearer-gated in all modes, `GET
/health` + the 4 bearer-gated `/api/debug/*` routes, the `/board` + `/dashboard`
HTML shells (static, no DB — deliberately NOT limited). The board handler's
honest-nulls/always-200 contract and its payload shape are byte-identical.

## Edge Cases

- **Single tab poll cadence (3s) vs TTL (2s):** each poll usually misses →
  fresh data; two tabs 3s apart → second tab may get a ≤2s-stale hit. Fine.
- **Board while QuestDB down:** compute degrades to nulls (existing
  contract); the null-body 200 is cached 2s (bounded probe cost — the
  uncached down-path costs 3 x 3s-timeout attempts per hit).
- **Burst then sustained:** shared cell — board + stats + quote draw from
  one budget by design (they protect the same DB); tested.
- **`/board` + `/dashboard` HTML shells:** NOT limited (static, no DB) —
  the operator can always load the page; only the JSON poll is budgeted.
- **Serialization failure on cache store:** falls through to the uncached
  `Json` path (never 500 on a cache problem, never caches a broken body) —
  same arm as stats.
- **Poisoned mutex:** `PoisonError::into_inner` recovery (existing
  `SingleSlotTtlCache` behavior, already ratchet-tested in #1458).

## Failure Modes

- **Attacker 429-starves the board poll:** possible (global cell, same
  documented #1458 trade-off) — the DB stays protected; the /board page
  already tolerates failed polls (shows last data + retries next tick).
- **Stale board data:** bounded at 2s — below the page's own 3s cadence;
  documented as the honest envelope change.
- **Cache serves nulls for 2s after a QuestDB recovery:** bounded,
  self-heals on the next miss.
- **Log amplification via 429s:** impossible — the #1458 `debug!`-only
  middleware is reused unchanged.

## Test Plan

All in `crates/api` (block-scoped per testing-scope.md):

- `public_guard.rs`: extend `test_endpoint_label_is_static_and_total` with
  the `/api/board/data` → `"board"` mapping.
- `lib.rs` router tests:
  - `test_board_data_shares_rate_limit_budget` — exhaust the shared cell
    via stats, then rapid `/api/board/data` GETs must include a 429
    (proves the route is inside the limited sub-router).
  - Extend `test_feeds_and_health_not_rate_limited_after_burst` to ALSO
    pin `/board` + `/dashboard` (HTML shells) as never-limited, alongside
    the existing `/api/feeds`, `/api/feeds/health`, `/health` pins.
  - Existing locked-contract ratchets unchanged and green
    (`test_feeds_post_requires_auth_401_without_token_in_both_modes`,
    `test_feeds_get_is_public_200_without_token`, ...).
- `board.rs` handler tests:
  - `test_compute_board_data_all_sources_down_returns_nulls_not_panics`
    (the existing null-contract test, repointed at the extracted compute —
    behavior unchanged).
  - `test_board_data_cache_hit_skips_recompute` — first call = miss
    (marker header), second call inside the TTL = hit with byte-identical
    body (multi-mock QuestDB serving exactly the first pass's 3 responses,
    so only the cache can reproduce the body — the same mock-exhaustion
    proof pattern as #1458).

## Rollback

Single-PR revert restores the exact prior router + handler — additive
cache slot, no persisted state, no schema/config/deploy change. The
`/api/board/data` payload shape is untouched, so no page change either.

## Observability

- `tv_api_rate_limited_total{endpoint="board"}` + `tv_api_cache_hits_total{endpoint="board"}`
  — the #1458 counters gain one more STATIC label value each.
- `x-tv-cache: hit|miss` header on `/api/board/data` (curl-debuggable).
- 429s land in the existing `tv_api_request_duration_ms{status="429"}`
  histogram via the outer tracing layer.
- No new ErrorCode (no `error!` emit site); no new alert (same #1458
  rationale — internet-scanner noise must not page).

## Plan Items

- [x] Item 1 — route `/api/board/data` through the existing limiter +
  add the `"board"` endpoint label + replace the residual comment.
  - Files: crates/api/src/lib.rs, crates/api/src/public_guard.rs
  - Tests: test_endpoint_label_is_static_and_total (extended),
    test_board_data_shares_rate_limit_budget,
    test_feeds_and_health_not_rate_limited_after_burst (extended)
- [x] Item 2 — `board_cache` (2s single slot) in SharedAppState + the
  compute/wrapper split in the board handler.
  - Files: crates/api/src/state.rs, crates/api/src/handlers/board.rs
  - Tests: test_board_data_cache_hit_skips_recompute,
    test_compute_board_data_all_sources_down_returns_nulls_not_panics
- [x] Item 3 — verify: cargo test -p tickvault-api + clippy + fmt + hooks;
  synchronous adversarial pass (security + hostile) on the diff; PR +
  merge watch.
  - Files: (verification only)
  - Tests: (full api-crate suite green)

## Scenarios

## Adversarial review outcome (2026-07-09, post-impl)

security-reviewer: 0 CRITICAL/HIGH/MEDIUM, 1 LOW (accepted std-Mutex
pattern, same as #1458), 5 FALSE-POSITIVE (poisoning, posture drift,
unwrap, extraction drift, secrets — all disproven with file:line).
Hostile reviewer: 1 MEDIUM + 2 LOW.

| Sev | Finding | Resolution |
|---|---|---|
| MEDIUM | "one 3-query pass per 2s" doc claim false in the DB-black-holed regime (compute ~3s > 2s TTL, no single-flight) — real bound is the limiter (≤5 computes/s) | **FIXED** — honest-bound wording in board.rs handler docs + state.rs const docs + this plan; single-flight = same flagged follow-up as stats |
| LOW | budget-test llvm-cov headroom margin | **FIXED** — exhaust margin burst+5 → burst+8 |
| LOW | 2s-stale `market_open` at the 09:15/15:30 boundary (display-only; server gates authoritative elsewhere) | **ACCEPTED** — within the documented envelope |

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Internet client hammers /api/board/data at 100 req/s | ≤5 computes/s admitted, mostly cache hits; QuestDB sees ≤1 three-query pass per 2s while healthy, ≤5 probe passes/s while black-holed (limiter-bounded — unbounded before) |
| 2 | One /board tab + one /dashboard tab open | ~0.53 req/s vs 5 req/s budget — never 429 |
| 3 | QuestDB down + board storm | one 3-probe pass per 2s max; null-body 200 served from cache |
| 4 | /board or /dashboard HTML shell requested during an attack | always 200 (shells not limited) |
| 5 | /api/feeds GET storm | never 429 — locked contract intact |
