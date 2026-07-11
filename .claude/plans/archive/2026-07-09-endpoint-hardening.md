# Implementation Plan: Public Endpoint Hardening — rate limit + TTL cache for /api/stats + /api/quote

**Status:** VERIFIED
**Date:** 2026-07-09
**Approved by:** Parthiban (operator) — 2026-07-09 audit directive (public-funnel DoS/DB-load surface)
**Changed crates:** api (`crates/api`)
**Guarantee matrices:** cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) — this item's specifics are carried in the PR body's guarantee-check block.

> **Incident context (2026-07-09 audit):** unauthenticated `GET /api/stats`
> (5 sequential QuestDB HTTP round-trips per hit: SHOW TABLES + 4
> `SELECT count()`, each with a fresh reqwest client) and
> `GET /api/quote/{security_id}` (1 `LATEST ON` query + 1 reachability probe
> on miss) are exposed on the public internet via the Tailscale funnel on
> port 3001 with NO rate limit and NO caching — a free DoS / DB-load
> amplification surface against QuestDB.

## Design

Two independent layers, both scoped to ONLY `/api/stats` +
`/api/quote/{security_id}`. Nothing else on the router changes:
`GET /api/feeds` + `GET /api/feeds/health` stay public-read (locked
contract, operator 2026-06-23), `POST /api/feeds/{feed}` stays bearer-gated
in all modes (locked 2026-07-04), `GET /health` stays untouched (infra
probes), the 4 `/api/debug/*` routes stay bearer-gated.

1. **Rate limit (checked FIRST — protects CPU + DB):** one process-global
   (per-router-instance) GCRA limiter via the EXISTING workspace `governor`
   dep (`RateLimiter<NotKeyed, InMemoryState, DefaultClock>` — the exact
   house pattern of `crates/trading/src/oms/rate_limiter.rs`).
   `Quota::per_second(5).allow_burst(10)`. Applied as an axum
   `route_layer(from_fn_with_state(...))` on a sub-router containing ONLY
   the two routes, then merged into the public router — the layer
   structurally cannot wrap `/api/feeds`, `/health`, or anything else.
   On limit: static `429 Too Many Requests` JSON body + `Retry-After: 1`
   header, `tv_api_rate_limited_total{endpoint="stats"|"quote"|"other"}`
   counter (static label values only), and a `debug!`-level log ONLY —
   deliberately NOT `error!`/`warn!` per request, so an attacker cannot use
   the limiter itself to flood errors.jsonl/CloudWatch (audit-Rule-class
   log-amplification defence; the counter is the operator signal).

   **Why GLOBAL and not per-IP (honest choice):** the Tailscale funnel
   terminates TLS upstream, so client IPs reach the app only via spoofable
   forwarded headers. A per-IP limiter keyed on attacker-chosen headers is
   trivially bypassed by header rotation; a global cap protects QuestDB
   regardless of attacker IP-spread. The cost: a determined attacker can
   starve the operator's dashboard of these two endpoints (429s) — but the
   DB stays protected, `/health` + `/api/feeds*` + the bearer-gated debug
   routes stay unlimited, and the cache means most legit traffic is served
   without DB touch anyway. Budget check: the `/dashboard` page polls
   `/api/stats` every ~5s per tab (0.2 req/s); MCP `tickvault_api` calls are
   occasional. 5 req/s sustained + burst 10 is >10x headroom.

2. **TTL response cache (checked AFTER the limiter, inside the handlers):**
   tiny in-process caches living in `SharedAppState` (fresh per router
   build → test isolation, no cross-test global statics):
   - **stats:** single-slot `Mutex<Option<(Instant, String)>>`, TTL 5s,
     caches the serialized 200 JSON body. `get_stats` ALWAYS returns 200
     (even QuestDB-down returns zeros with `questdb_reachable:false`), so
     the single slot caches whatever the last computation produced — this
     is deliberate negative-caching: a QuestDB-down probe storm otherwise
     costs 5 failed HTTP attempts x 3s timeout per hit.
   - **quote:** bounded `Mutex<HashMap<u64, (Instant, String)>>`, TTL 1s,
     hard cap 2048 entries, **caches ONLY 200 responses**. 404/503/400 are
     NEVER cached (no cache poisoning, no negative entry masking a
     just-arrived first tick). Only-200 + the cap bound the map: only SIDs
     with real tick rows (the ~250-1200 daily universe) can enter; garbage
     attacker-chosen security_ids get 404 and never consume memory. At cap,
     new keys are skip-inserted (never evict-thrash); expired entries are
     overwritten in place on the next successful compute.
     Cache key = `security_id` alone, which mirrors the endpoint's OWN key
     (`WHERE security_id = X LATEST ON ts PARTITION BY security_id` across
     ALL segments/feeds) — single-key is correct by construction per
     I-P1-11 rule 2 (documented `// APPROVED:` comment at the declaration).
   - Cache HIT returns the stored body with `content-type: application/json`
     + an `x-tv-cache: hit` marker header and touches QuestDB ZERO times.
   - `tv_api_cache_hits_total{endpoint}` counter (static labels).
   - Mutexes are std `Mutex` with `lock().unwrap_or_else(PoisonError::into_inner)`
     (poison recovery, zero unwrap/expect in prod). This is a COLD API path
     (not the tick hot path) — a mutex is correct here; no hot-path rules
     apply, and no allocation is added to any tick-processing code.

3. **Ordering decision:** rate limit BEFORE cache. Rationale: the limiter
   protects CPU + the log/metric pipeline too, and a cache-first ordering
   would let unlimited cached traffic keep a connection-level DoS cheap.
   Either order protects the DB; limit-first is the stricter envelope.

4. **Constants (no hardcoded values):** `PUBLIC_API_RATE_PER_SEC = 5`,
   `PUBLIC_API_BURST = 10` in the new `crates/api/src/public_guard.rs`;
   `STATS_CACHE_TTL_SECS = 5` in stats.rs; `QUOTE_CACHE_TTL_SECS = 1` +
   `QUOTE_CACHE_MAX_ENTRIES = 2048` in the cache module.

## Edge Cases

- **Burst then sustained:** first 10 requests pass instantly, then 5/s
  sustained; the 11th rapid request 429s (tested).
- **Two endpoints share one budget:** stats + quote draw from the same
  GCRA cell — intentional (they protect the same DB); documented.
- **QuestDB down:** stats caches the zeroed/unreachable 200 body for 5s
  (bounded probe cost); quote 503 is NOT cached (next request re-probes,
  still rate-limited).
- **Quote 404 (unknown SID):** never cached; a follow-up request after the
  first tick arrives returns fresh 200 (tested: 404-then-200 sequence).
- **Quote cache at cap:** 2049th distinct hot SID is served fresh (no
  insert), never evicts existing entries, never grows the map.
- **Poisoned mutex (a panicking thread mid-insert):** `PoisonError::into_inner`
  recovery — worst case one stale/partial slot, never a panic, never 500.
- **Non-u64 / zero security_id:** existing 400 guard unchanged, runs after
  the limiter, before any cache/DB work.
- **Stale reads:** stats ≤5s stale, quote ≤1s stale — honest envelope
  change documented in the PR body ("latest tick" becomes "latest tick,
  ≤1s old").
- **`/api/feeds`, `/api/feeds/health`, `/health`, `/dashboard`, `/board`,
  debug routes, `POST /api/feeds/{feed}`:** structurally outside the
  sub-router — regression-tested to remain un-rate-limited / unchanged.

## Failure Modes

- **Limiter denies legit operator (dashboard + MCP concurrently):** budget
  analysis above shows >10x headroom; if it ever bites, the operator sees
  429 + `Retry-After: 1` and the `tv_api_rate_limited_total` counter names
  the endpoint. Recovery: none needed — GCRA replenishes within 200ms/req.
- **Cache serves stale after a QuestDB recovery:** bounded at 5s (stats) /
  1s (quote); acceptable for an observability endpoint, documented.
- **Serialization failure on cache store:** fall through to the uncached
  response path (never 500 on a cache problem, never cache a broken body).
- **Log amplification via 429s:** impossible by design — `debug!` only,
  counter is the signal (no error!/warn! per limited request).
- **Memory DoS via cache:** impossible by design — only-200 policy + 2048
  cap + 1s TTL bound the quote map; stats is a single slot.

## Test Plan

All in `crates/api` (block-scoped per testing-scope.md):

- `lib.rs` router tests (tower::ServiceExt::oneshot house style):
  - `test_public_stats_rate_limit_429_after_burst` — burst+1 rapid GETs to
    `/api/stats` → final response 429 with `Retry-After` header.
  - `test_public_quote_shares_rate_limit_budget` — exhaust budget via
    stats, then quote GET → 429 (shared cell proven).
  - `test_feeds_get_not_rate_limited_after_burst` — exhaust the limiter,
    then 15x `GET /api/feeds` + `GET /api/feeds/health` + `GET /health`
    all 200 (locked public-read contract preserved; layer scope proven).
  - Existing locked-contract ratchets unchanged and still green:
    `test_feeds_post_requires_auth_401_without_token_in_both_modes`,
    `test_feeds_post_with_valid_token_not_401_in_both_modes`,
    `test_feeds_get_is_public_200_without_token`.
- `public_guard.rs` unit tests: allows-within-burst, denies-after-burst,
  endpoint label mapping is static + total.
- `response_cache.rs` unit tests: put/get roundtrip, TTL expiry (tiny TTL +
  sleep), single-slot overwrite, bounded-map cap skip-insert, only-present
  key overwrite at cap, poison recovery (panic while holding lock →
  subsequent get works).
- `stats.rs` handler tests: cache hit returns byte-identical body with NO
  second DB call (multi-mock server with exactly 5 responses; second
  handler call must equal the first — a real second pass would produce the
  differing unreachable/zeros body); existing field tests move to the
  extracted `compute_stats` (behavior unchanged).
- `quote.rs` handler tests: 200 cached (mock exhausted, second call
  identical 200); 404 NOT cached (404-then-200 sequence against a
  multi-mock); existing 400/404/503 tests unchanged.

## Rollback

Single-PR revert (`git revert <merge-sha>`) restores the exact prior
router + handlers — the feature has no persisted state, no schema change,
no config-file change, no cross-crate surface. The new module files are
additive; handler diffs are wrapper-shaped around the extracted compute
functions. No deploy-side or terraform change.

## Observability

- `tv_api_rate_limited_total{endpoint}` — counter, static labels
  (`stats`/`quote`/`other`), incremented once per 429.
- `tv_api_cache_hits_total{endpoint}` — counter, static labels.
- `x-tv-cache: hit|miss` response header on the two endpoints (operator
  curl-debuggable).
- `debug!` trace per limited request (deliberately below the error!/warn!
  Telegram/CloudWatch pipeline — see Design §1 log-amplification defence).
- Existing `tv_api_request_duration_ms{method,status}` histogram already
  captures the 429 status split via the outer `request_tracing` layer.
- No new ErrorCode: no `error!` emit site is added (the cross-ref +
  tag-guard tests stay untouched).
- No new alert rule: a 429 burst is attacker noise, not an operator page;
  the DB-protection outcome is the point. (CloudWatch alarm on the counter
  is a possible follow-up if the operator wants visibility — deliberately
  NOT paged by default to avoid pager fatigue from internet scanners.)

## Plan Items

- [x] Item 1 — `public_guard.rs`: GCRA limiter (governor, house pattern) +
  axum middleware + endpoint label mapping + constants.
  - Files: crates/api/src/public_guard.rs, crates/api/src/lib.rs,
    crates/api/Cargo.toml (add `governor = { workspace = true }` — already
    a pinned workspace dep, NOT a new dependency)
  - Tests: test_limiter_allows_within_burst, test_limiter_denies_after_burst,
    test_endpoint_label_is_static_and_total,
    test_public_stats_rate_limit_429_after_burst,
    test_public_quote_shares_rate_limit_budget,
    test_feeds_get_not_rate_limited_after_burst
- [x] Item 2 — `response_cache.rs`: single-slot + bounded TTL caches wired
  into `SharedAppState`.
  - Files: crates/api/src/response_cache.rs, crates/api/src/state.rs
  - Tests: test_single_slot_put_get_roundtrip, test_single_slot_ttl_expiry,
    test_bounded_cache_only_present_key_overwrites_at_cap,
    test_bounded_cache_cap_skip_insert, test_bounded_cache_ttl_expiry,
    test_poisoned_lock_recovers
- [x] Item 3 — stats handler: extract `compute_stats`, wrap with 5s cache.
  - Files: crates/api/src/handlers/stats.rs
  - Tests: test_get_stats_cache_hit_skips_questdb,
    test_get_stats_returns_unreachable_when_questdb_down (updated to
    compute_stats), test_get_stats_with_mock_questdb (updated)
- [x] Item 4 — quote handler: cache ONLY 200 bodies, 1s TTL, bounded.
  - Files: crates/api/src/handlers/quote.rs
  - Tests: test_get_quote_cache_hit_returns_identical_200,
    test_get_quote_404_is_never_cached
- [x] Item 5 — verify: cargo test -p tickvault-api, clippy -D warnings,
  fmt, banned-pattern scanner, plan-verify; adversarial 3-agent review on
  the diff; PR + merge watch.
  - Files: (verification only)
  - Tests: (full api-crate suite green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Internet client hammers /api/stats at 100 req/s | first ~10 pass (mostly cache hits), then 429; QuestDB sees ≤1 five-query pass per 5s |
| 2 | Attacker probes /api/quote/1..10^6 garbage SIDs | rate-limited to 5/s; 404s never cached; quote map never grows past real universe |
| 3 | Operator dashboard (5s poll) + MCP call concurrently | all 200, mostly cache hits, never 429 |
| 4 | QuestDB down + stats storm | one 5-probe pass per 5s max; zeroed 200 body served from cache |
| 5 | First tick arrives 200ms after a 404 probe for the same SID | next request (post-limit) returns fresh 200 — 404 was not cached |
| 6 | /api/feeds GET storm | never 429 (outside the sub-router) — locked contract intact |

## Adversarial review outcome (2026-07-09, post-impl)

security-reviewer: 0 CRITICAL, 0 HIGH, 2 LOW (documented negative-caching +
harmless TOCTOU double-compute). Hostile reviewer: 1 HIGH + 3 MEDIUM + 2 LOW.

| Sev | Finding | Resolution |
|---|---|---|
| HIGH | `GET /api/board/data` (3rd public DB-backed route, 3 queries/hit) stays unlimited | PRE-EXISTING + outside the operator-locked scope ("stats + quote ONLY — do not expand"). Documented loudly in `public_guard.rs` + PR body; FLAGGED FOLLOW-UP (needs its own budget — a shared 5/s cell would starve the /board 3s poll) |
| MEDIUM | quote cache could permanently self-disable at cap (dead keys never swept; daily SID churn) | FIXED — `put` sweeps expired entries at the cap boundary; test `test_bounded_cache_at_cap_sweeps_expired_then_inserts` |
| MEDIUM | GCRA-replenishment test flakiness on slow/llvm-cov runners | FIXED — both router tests assert "≥1 429 among burst+K rapid requests" instead of exact positioning |
| MEDIUM | thundering herd on stats miss (no single-flight; fresh client per compute) | DOCUMENTED bound in `compute_stats` docs — limiter caps it at 5 computes/s (unbounded before); single-flight = flagged follow-up |
| LOW | 10ms-TTL expiry test flake window | FIXED — 300ms TTL / 400ms sleep |
| LOW | Retry-After inline literal; serialize-fallback lacks cache marker | FIXED const `RETRY_AFTER_SECS`; fallback arm is practically unreachable and documented |
