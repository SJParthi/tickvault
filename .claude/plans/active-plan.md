# Implementation Plan: NTM Sub-PR #3 — rebuild the niftyindices.com constituency downloader + parser + cache

**Status:** APPROVED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06 "Lock ISIN contract, then Sub-PR #3".
**Crate(s) touched:** `core` (new `instrument/index_constituency/` module), `storage`
(hardening ratchet test). Feature-gated behind `daily_universe_fetcher`.

## Context

Builds to the §31.1 ISIN-primary mapping contract (merged #1036) and the existing
`IndexConstituent` / `IndexConstituencyMap` types (`crates/common/src/instrument_types.rs`)
+ `INDEX_CONSTITUENCY_*` constants. Mirrors the proven `csv_downloader.rs` §18
hardening. This sub-PR fetches + parses + caches the niftyindices CSV; it does NOT
touch the Dhan master (`CsvRow.isin` is added in Sub-PR #4 where the join lives) and
does NOT wire into boot (Sub-PR #5/#10).

Operator-provided source (rule §31 item 7):
`https://www.niftyindices.com/IndexConstituent/ind_niftytotalmarket_list.csv`
(= `INDEX_CONSTITUENCY_BASE_URL` + `ind_niftytotalmarket_list.csv`).

## Plan Items

**Adversarial 3-agent review (done):** hot-path = CLEAN (confirmed cold-path,
no banned unwraps, bounded). hostile = H1 (missing_isin dropped) + M1 (no
Series=EQ filter) — both FIXED below. security = (see PR body). Fixes:
`ConstituencyBuildMetadata.missing_isin` added + summed in `assemble_map` (H1);
parser now filters `Series=EQ` with `skipped_non_eq` count (M1).

- [x] Item 1 — `index_constituency/downloader.rs` — hardened HTTP client
  - Files: `crates/core/src/instrument/index_constituency/downloader.rs`, `mod.rs`
  - Mirror `csv_downloader.rs`: `redirect::Policy::none()`, body cap, 10 s connect /
    60 s read timeout, Content-Type allowlist (`text/csv`/`octet-stream`/`plain`),
    typed `ConstituencyDownloadError` enum. URL = base const + filename arg.
  - Tests: `redirect_policy_is_none`, `rejects_html_content_type`, `body_cap_enforced`
- [x] Item 2 — `index_constituency/parser.rs` — robust constituent-CSV parser
  - Files: `crates/core/src/instrument/index_constituency/parser.rs`
  - Columns `Company Name, Industry, Symbol, Series, ISIN Code` → `IndexConstituent`
    (`index_name` arg, `symbol`, `isin`, `sector=Industry`, `weight=0.0`,
    `last_updated`). Robust per §26: strip BOM, normalize CRLF/CR, quoted commas
    (`csv` crate `Trim::All`), reject non-UTF-8, skip trailing disclaimer/blank rows,
    sanitize BiDi in symbol. Reject if `< INDEX_CONSTITUENCY_MIN_INDICES`-style
    minimum constituents OR `>X%` rows missing ISIN/Symbol (fail-closed).
  - Tests: header parse, BOM strip, CRLF, quoted commas, disclaimer-row skip,
    missing-ISIN row handling, non-UTF-8 reject, proptest arbitrary-bytes either
    parse-or-reject-cleanly
- [x] Item 3 — `index_constituency/cache.rs` — JSON cache round-trip
  - Files: `crates/core/src/instrument/index_constituency/cache.rs`
  - Write/read `IndexConstituencyMap` to `data/instrument-cache/<date>/`
    `constituency-map.json` (`INDEX_CONSTITUENCY_CSV_CACHE_FILENAME`); path-traversal
    guard on the date component (reuse `instrument_snapshot::is_valid_trading_date`).
  - Tests: serde round-trip identity, path-traversal reject, missing-file → None
- [x] Item 4 — `build_constituency_map(...)` top-level (the wiring call site)
  - Files: `crates/core/src/instrument/index_constituency/mod.rs`
  - Calls downloader → parser → assembles `IndexConstituencyMap` (forward + reverse
    maps, build metadata). This is the non-test caller satisfying the pub-fn-wiring
    guard for the parser/downloader pub fns.
  - Tests: end-to-end with an in-memory CSV fixture (no network) → map has expected
    forward/reverse entries + metadata
- [x] Item 5 — storage hardening ratchet
  - Files: `crates/storage/tests/constituency_downloader_hardening_guard.rs`
  - Mirror `csv_downloader_hardening_guard.rs`: source-scan pins redirect-none, body
    cap, content-type allowlist, timeouts present.

## Design

Module tree `crates/core/src/instrument/index_constituency/{mod,downloader,parser,cache}.rs`,
all `#![cfg(feature = "daily_universe_fetcher")]`. Downloader is a faithful clone of
`csv_downloader.rs`'s `CsvDownloader` (one bounded GET attempt; retry/escalation live
in the orchestrator, Sub-PR #10). Parser is a pure `(bytes, index_name) ->
Result<Vec<IndexConstituent>, ConstituencyParseError>`. `build_constituency_map`
composes them and builds the O(1) forward+reverse `IndexConstituencyMap`. No Dhan-master
join here (Sub-PR #4); no boot wiring here (Sub-PR #5/#10) — but `build_constituency_map`
IS a real callable (called by Sub-PR #4/#10), and is exercised by Item 4's fixture test,
so this is a complete vertical slice, NOT a skeleton (Rule 14).

## Edge Cases

- niftyindices CSV has a trailing disclaimer/blank row → skipped, not parsed as data.
- A constituent row missing ISIN → kept with empty `isin` (Sub-PR #4 falls back to
  symbol+series) but counted; if `>X%` missing → reject (fail-closed).
- Empty/HTML/JSON body → downloader Content-Type allowlist rejects before parse.
- Same stock in multiple indices (future multi-index) → reverse map appends, dedup.
- Non-UTF-8 bytes → reject (no lossy parse).

## Failure Modes

- Network failure / 5xx / timeout → typed `ConstituencyDownloadError`; the orchestrator
  (Sub-PR #10) wraps with the §4 retry ladder. This module fails closed per attempt.
- Parser reject → typed `ConstituencyParseError`; caller decides (Sub-PR #4/#10).
- Feature OFF (default) → module not compiled → zero runtime surface (matches
  `csv_downloader.rs`); CI runs the tests under `--features daily_universe_fetcher`.

## Test Plan

- `cargo test -p tickvault-core --features daily_universe_fetcher index_constituency`
  (all unit + proptest green).
- `cargo test -p tickvault-storage --test constituency_downloader_hardening_guard`.
- `cargo build --workspace` green (feature on via app default).
- 3-agent adversarial review (hot-path / security / hostile) on the diff BEFORE + AFTER
  per operator-charter §E (new network path = mandatory). Fix every CRITICAL/HIGH inline.

## Rollback

- New module + one ratchet test, feature-gated + unwired → `git revert` removes it with
  zero runtime impact (nothing calls it in production until Sub-PR #5/#10).

## Observability

- Downloader/parser failures are typed errors returned upward; the runtime telemetry
  (counters, Telegram on fetch failure, `instrument_fetch_audit`-style row) attaches in
  Sub-PR #6 where the build is wired into boot. N/A for this unwired module — matches the
  `csv_downloader.rs` precedent (HTTP layer only).

## Per-Item Guarantee Matrix

| Demand | Sub-PR #3 |
|---|---|
| 100% testing | downloader hardening + parser robustness (incl. proptest) + cache round-trip + e2e fixture |
| O(1)/perf | cold-path (boot) build; `IndexConstituencyMap` gives O(1) forward+reverse lookup |
| zero-loss/WS/QuestDB | unchanged — no tick/WS/DB path; cache is a cold JSON file |
| security | redirect-none + body cap + content-type allowlist + path-traversal guard + non-UTF-8 reject; security-reviewer agent |
| uniqueness/dedup | reverse map dedups index names per stock |
| honest envelope | fetch is per-attempt fail-closed; retry/escalation + boot wiring are explicitly later sub-PRs (not faked here) |
| no half-finished (Rule 14) | `build_constituency_map` is a real callable with a fixture test + a Sub-PR #4/#10 call site; no skeleton, no false-OK counter, feature-gated like its csv_downloader sibling |
