# Implementation Plan: TickVault "Live Board" operator webpage (`/board` + `/api/board/data`)

**Status:** APPROVED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator, verbatim 2026-07-05: "as of now just go ahead let us check or change later" — after approving the animated dark-terminal design mockup with the beginner-plain-English labels; the animated/attractive/beginner demands from the same session apply)

> Guarantee matrices: this plan cross-references the canonical
> per-wave-guarantee-matrix.md (15-row + 7-row) instead of inlining it —
> this is a cold-path, read-only HTML/JSON view in `crates/api`; no hot-path,
> no new tick-drop path, no new table, no new WS connection.

## Design

A real (non-dummy) operator "Live Board" webpage, mirroring the approved design
mockup (dark terminal aesthetic, animated, beginner plain-English labels,
7 sections), in `crates/api`:

- `GET /board` — self-contained HTML page (include_str-style `const` embedded
  template like `/feeds` and `/dashboard`), PUBLIC route (same auth posture as
  the `/feeds` + `/dashboard` HTML shells: no secrets in the shell). SAMPLE
  badge removed; a "LOCAL" badge replaces it. All animations client-side.
  Files: `crates/api/src/handlers/board_page.rs`.
- `GET /api/board/data` — JSON snapshot the page polls every ~3s. PUBLIC route
  (numbers + fixed/sanitized short tags only, mirroring the public
  `/api/feeds/health` read posture). Aggregates REAL data, cold-path only,
  best-effort with honest nulls (never a faked number):
  - status: uptime (boot_epoch_secs), build sha (`build_info` short),
    market open/closed (`is_trading_session_now`), RSS memory from
    `/proc/self/status` when available (null on macOS). CPU omitted (no cheap
    dependency-free source — honest omission).
  - feeds: reuse the existing `get_feeds_health` aggregation verbatim
    (per-feed conns/subscribed/ticks/last-tick age) — no forked logic.
  - rust shadow: `data/groww/rust-live-ticks.ndjson` file bytes + mtime age +
    capped line count (≤ 8 MiB read; larger → lines null, honest).
  - connections: scale runs read `data/groww/shards/c*/groww-status.json`;
    normal runs the single `data/groww/groww-status.json`; sanitized event
    tag + counts + age per connection.
  - race/latency: newest `data/groww/parity-<date>.tsv` (the PR-R2 comparer
    output) parsed into a numeric metric map; absent → null → the page shows
    "race pending (needs live ticks)". Dhan latency card carries the
    whole-second-clock caveat flag.
  - db: ticks-today + candles_1m-sealed-today counts via the existing QuestDB
    HTTP `/exec` helper (`stats::query_count`, made `pub(crate)`), 3s bounded
    timeout, null on failure.
  - problems: NOT duplicated onto the public route — the page client-side
    fetches the EXISTING bearer-gated `GET /api/debug/logs/jsonl/latest`
    (2026-07-04 security trim respected) with the sessionStorage token pattern
    from `/dashboard`; without a token on an auth-enabled deployment the
    section shows a lock hint. Local dev (auth passthrough) works tokenless.
  - telegram mirror: OMITTED in v1 (no recent-notifications buffer exists in
    state — honest omission, noted as follow-up in the PR body).
  Files: `crates/api/src/handlers/board.rs`, route wiring `crates/api/src/lib.rs`,
  module registration `crates/api/src/handlers/mod.rs`, `stats.rs` visibility.

## Edge Cases

- All sources down (QuestDB unreachable, no groww files, boot epoch 0) →
  200 JSON with nulls/empty arrays, never a panic, never a fabricated value.
- Shard dirs present but a `groww-status.json` torn/absent/malformed → that
  connection row is skipped (the sidecar writes temp+rename, but read races
  are absorbed).
- `rust-live-ticks.ndjson` larger than the 8 MiB line-count cap → bytes+age
  reported, `lines: null`.
- Parity TSV with unknown/extra keys → parsed generically (metric→i64 map,
  key sanitized + row-capped) so future comparer columns don't break the page.
- Non-Linux host → `mem_rss_bytes: null` (no /proc).
- Status-file `event` strings sanitized (ascii alnum/`_`, ≤ 32 chars) before
  they hit a public JSON route.
- Page fetch failures → each panel degrades independently to "—" (the
  `/dashboard` getJson pattern), auto-refresh continues.

## Failure Modes

- QuestDB down → `db.reachable=false`, counts null; page shows amber vault
  panel. No retry storm (one bounded 3s query per poll).
- Filesystem errors (permission, missing dir) → `None`/empty results; a
  single `warn!` is NOT emitted per poll (would spam every 3s) — degradation
  is visible on the page itself; no new ErrorCode is introduced (no existing
  code fits a read-only view; per instruction plain behaviour, no error!).
- Debug-logs endpoint 401 → page problems panel shows the token hint; no
  auth bypass is added.
- Handler blocking I/O is bundled into `spawn_blocking`; a JoinError degrades
  to the all-null snapshot.

## Test Plan

- `cargo test -p tickvault-api` (scoped per testing-scope.md).
- Handler tests: `board_data` with all sources down → 200 + nulls (no panic);
  `board_page` returns 200, contains the 6 section markers, the LOCAL badge,
  no SAMPLE badge, fetches `/api/board/data` + `/api/debug/logs/jsonl/latest`,
  renders via textContent (no `.innerHTML`), CSP blocks clickjacking.
- Pure-fn unit tests: parity TSV parser (valid/garbage/cap), shard status
  parser (valid/malformed/sanitization), shard-dir scanner (multi-shard +
  single-file fallback + none), capped line counter (small file / over-cap),
  VmRSS line parser, IST-midnight literal formatter, newest-parity picker.
- fmt + clippy clean.

## Rollback

Pure additive view: revert the single PR (two new handler files + 3-line route
wiring + one `pub(crate)` visibility change). No schema, no config, no hot-path,
no persisted state — reverting restores the exact previous surface.

## Observability

- The page IS an observability surface (operator view). Server-side it reuses
  existing instrumented components (feeds health registry, QuestDB /exec).
- Request tracing middleware already wraps all routes (`request_tracing`).
- No new metrics/alerts: read-only cold-path view; failures self-surface on
  the page as nulls. No new ErrorCode (cross-ref test unaffected).

## Plan Items

- [x] `crates/api/src/handlers/board.rs` — `/api/board/data` aggregation + pure helpers
  - Files: crates/api/src/handlers/board.rs, crates/api/src/handlers/stats.rs
  - Tests: test_board_data_all_sources_down_returns_nulls_not_panics, test_parse_parity_tsv_extracts_numeric_metrics, test_scan_connections_prefers_shards_dir, test_count_lines_capped_respects_cap
- [x] `crates/api/src/handlers/board_page.rs` — `/board` HTML page (approved design, LOCAL badge)
  - Files: crates/api/src/handlers/board_page.rs
  - Tests: test_board_page_is_html_with_local_badge_and_no_sample, test_board_page_has_all_section_markers, test_board_page_fetches_board_data_and_debug_logs
- [x] Route wiring + module registration
  - Files: crates/api/src/lib.rs, crates/api/src/handlers/mod.rs
  - Tests: test_board_routes_are_public_200_without_token

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fresh boot, nothing running | 200 JSON, nulls everywhere, page renders with "—" |
| 2 | Normal single-sidecar Groww run | one connection row from groww-status.json |
| 3 | Scale run (shards/c00..c09) | one row per shard, sorted by conn id |
| 4 | Parity TSV present | race panel shows matched/missing + p50/p99 latency |
| 5 | QuestDB paused | db.reachable=false, counts null, page amber |
| 6 | Auth-enabled prod, no token | problems panel shows token hint, rest renders |
