# Implementation Plan: Feed page + boot REST storm + candle-table schema cleanup

**Status:** APPROVED
**Date:** 2026-06-23
**Approved by:** Parthiban (operator) — chat 2026-06-23: "go ahead with the plan", scope confirmed via AskUserQuestion (public-read feeds page; candles = tables-from-ticks, no seconds).
**Branch:** `claude/cool-noether-3liwxc`

## Context / single source of truth

`ticks` is the ONE source of truth. The candle engine (`#1189`, merged) folds ticks
O(1) per tick and seals `candles_1m … candles_1d` as REAL TABLES (Engine-B,
`crates/storage/src/shadow_persistence.rs`). No materialized views. This plan does
NOT change that engine — it removes three things that fight it / mislead the operator.

## Plan Items

- [ ] **Item 1 — candle schema script: tables-from-ticks only (no matviews)**
  - The standalone `scripts/questdb-init.sh` Phase 3 creates `candles_1m…1M` as
    materialized views, colliding with Engine-B's regular tables → HTTP 400 (the
    `ensure-ready.sh` "13 FAIL"). Engine-B owns every candle table from `ticks`.
  - Remove `create_materialized_views()` + its call + the dead `candles_1s` base
    table (Engine-B drops it). Fix the Phase-4 table-count (it greps the column
    header → always "1"; parse the dataset rows instead).
  - Files: scripts/questdb-init.sh
  - Tests: scripts/questdb-init.selftest.sh (new — asserts no `CREATE MATERIALIZED VIEW` + no `candles_1s` remain, and the count parser counts rows)

- [ ] **Item 2 — feed page opens: public read, authed toggle**
  - `GET /api/feeds` + `GET /api/feeds/health` are read-only and carry NO secrets
    (`FeedHealthRow` is `&'static str` only — verified) → move them to the PUBLIC
    router. `POST /api/feeds/{feed}` (the flip) STAYS bearer-protected. Update the
    `/feeds` page copy so it stops claiming "blank in dev" and explains the token
    is only needed to flip.
  - Files: crates/api/src/lib.rs, crates/api/src/handlers/feeds_page.rs
  - Tests: test_get_feeds_public_200_without_auth, test_get_feeds_health_public_200_without_auth, test_post_feed_still_requires_auth (crates/api/src/lib.rs tests)

- [ ] **Item 3 — boot REST storm: right limiter, no false "order" alarm**
  - `prev_day_ohlcv_boot` uses the OMS `OrderRateLimiter` (logs `order rate limit
    hit — SEBI max orders/sec exceeded` on every spin) as its data gate → thousands
    of misleading WARNs + busy-wait amplification. Replace with a quiet
    `governor`-direct Data-API limiter (no order WARN) at a conservative rate that
    leaves headroom for the other boot REST callers (option-chain + profile canary)
    so Dhan stops returning HTTP 429. On 429, bounded backoff-retry the symbol.
  - Files: crates/app/src/prev_day_ohlcv_boot.rs
  - Tests: test_data_api_limiter_emits_no_order_warn (source-scan: no OrderRateLimiter import), existing previous_trading_day / coverage tests stay green

## Design

- Item 1: deletion of dead DDL; the app's idempotent boot DDL is the sole candle-table owner. Common-runtime safe (same script Mac=AWS).
- Item 2: route-layer move only; security model unchanged for the mutating path. Read endpoints expose only operator-safe `&'static str` + counters.
- Item 3: swap the limiter type; keep the loop sequential + bounded; add 429-aware backoff.

## Edge Cases

- Item 1: QuestDB fresh (no tables) vs warm (Engine-B tables exist) — script no longer touches candles either way. Phase-4 count with 0 rows / many rows.
- Item 2: no Authorization header (read OK, toggle 401); malformed token (toggle 401); CORS preflight unchanged.
- Item 3: Dhan 429 mid-loop; token refresh mid-loop; empty universe; a symbol with no prior-day candle (skipped, not failed).

## Failure Modes

- Item 1: if a future deployment still has a squatting matview, Engine-B already `DROP MATERIALIZED VIEW IF EXISTS`-es before `CREATE TABLE` — unaffected.
- Item 2: if SSM token is empty, read still works, toggle is open (dev) — matches existing `enabled=false` passthrough.
- Item 3: limiter starvation → bounded spins then skip symbol (fail-soft, boot never blocks); 429 storm prevented by conservative rate + backoff.

## Test Plan

- `cargo test -p tickvault-api` (feed route auth split)
- `cargo test -p tickvault-app --lib` (prev_day limiter + coverage)
- `bash scripts/questdb-init.selftest.sh`
- `cargo clippy -p tickvault-api -p tickvault-app -- -D warnings`
- 3-agent adversarial review (hot-path + security + hostile) on the diff.

## Rollback

- Each item is independent. `git revert` the commit. Item 1 is script-only (no
  runtime effect — the app owns the tables regardless). Item 2 reverts the route
  move. Item 3 reverts to the prior limiter.

## Observability

- Item 3 keeps the data-API allow/deny counters (renamed off the misleading
  `tv_rate_limiter_*` order labels to a data-api label) so the rate gate stays
  measurable. Feed health endpoint is now operator-reachable without a token,
  improving real-time visibility. No new ERROR codes required.

## Per-item guarantee matrix

Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row).
Honest envelope: this PR fixes 3 bounded issues with ratcheted regression tests;
it does NOT claim "all permutations covered" — the candle-from-ticks O(1) fold is
already bench-gated by the existing engine; this PR adds no hot-path allocation.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `ensure-ready.sh` on warm QuestDB | all OK, no candle matview FAIL |
| 2 | Open `/feeds` with no token | page loads, feeds visible |
| 3 | Flip a feed with no token | 401 (still protected) |
| 4 | Boot prev-day fetch | no "order rate limit" WARN, no 429 storm |

## ISSUE QUEUE — from live evidence 2026-06-23 (every observed issue, nothing dropped)

Evidence sources: QuestDB console screenshots, `errors.summary.md`, `live_ticks`,
`option_chain`, `candles`, `app.log`, `errors.jsonl`.

| # | Issue (evidence) | Root cause | Priority | Maps to | Status |
|---|---|---|---|---|---|
| Q1 | Feed page "Unauthorized" (screenshot) | `/api/feeds` GET behind SSM-token auth; page says "blank in dev" | **P0 blocker** | Item 2 | building |
| Q2 | `prev-day OHLCV coverage EMPTY — no yesterday candles fetched` + `final flush failed` (errors.summary) | every prev-day REST fetch got Dhan **429** → 0 rows → yesterday's close missing → `close_pct_from_prev_day` unreliable | **P0 data** | Item 3 (expanded: 429-aware backoff-retry so coverage is NOT empty) | building |
| Q3 | `order rate limit hit — SEBI max orders/sec exceeded` flood (app.log, hundreds) | prev_day uses the OMS **order** limiter + 50× busy-wait spins logging each denial | **P1 noise** | Item 3 | building |
| Q4 | `candles_1m…1M HTTP 400` in `ensure-ready.sh` (13 FAIL) | stale script creates candles as **matviews**; engine owns them as tables-from-ticks (QuestDB shows **0 matviews**, candles_1m **12 real rows** — proven) | **P1 cosmetic** | Item 1 | building |
| Q5 | `WS-GAP-06 tick-gap — instruments silent ≥30s` (14×) | a few illiquid SIDs in the universe genuinely don't tick every 30s near close; NOT a code fault | **P2 investigate** | NEW Item 4 (confirm illiquid-SID expectation; market-hours-gate already present) | queued |
| Q6 | `Telegram chunked-send failures` + `TELEGRAM-01` (6×) | egress to api.telegram.org blocked on this network | **P2 environment** | NEW Item 5 (confirm Mac egress; not a code change) | queued |
| ✅ | option-chain fetch (NIFTY 232 strikes, BANKNIFTY 411, SENSEX 220; self-rate-limited ~3s) | — | verified HEALTHY | none | no action |
| ✅ | `tick conservation OK — every tick accounted (residual flat)` (12×), 761 batch flushes | — | verified HEALTHY | none | no action |
| ✅ | candles_1m = real TABLE from ticks, 0 matviews (screenshot) | — | verified HEALTHY | none | no action |

**This PR delivers Q1–Q4** (the actionable code/script fixes). **Q5 + Q6** are
queued as investigate-items (likely no code change — illiquid-SID reality + a
network egress check), tracked here so they are not lost. Each will get its own
serial PR only if a code change is warranted.
