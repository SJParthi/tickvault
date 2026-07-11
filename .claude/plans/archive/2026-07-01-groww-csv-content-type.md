# Implementation Plan: Groww CSV downloader Content-Type allowlist (§18 parity)

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — standing "cover all cases + fix everything, make PR + merge + go ahead with the plan" directive this session, closing the LOW-MEDIUM security finding from the 2026-07-01 adversarial security review.

## Design

The 2026-07-01 security review found the Groww CSV/constituent downloader
(`crates/core/src/feed/groww/instruments.rs::fetch_text_hardened`) has no-redirect
+ HTTPS-only + bounded timeouts + body-size cap, but — unlike the audited Dhan
downloader (`csv_downloader.rs`, hardening §18) — **omits the `Content-Type`
allowlist check**. A WAF block page or misconfigured CDN response (HTTP 200,
`text/html`, valid UTF-8) would pass status + body-cap and be handed to the CSV
parser. Blast radius today is a clean parse-error (no `unwrap` in the parse path),
but it deviates from the documented §18 pattern.

Fix: mirror Dhan's `validate_content_type` — extract a PURE, unit-testable
`validate_groww_content_type(header: Option<&HeaderValue>) -> Result<(), WatchBuildError>`
that (a) allows a MISSING header (static CDNs omit it — same as Dhan), (b) strips
the `; charset=…` suffix, lowercases, and (c) allows only `text/csv`,
`application/octet-stream`, `text/plain`, rejecting `text/html`/`application/json`.
Call it in `fetch_text_hardened` after the status check, BEFORE reading the body.

## Edge Cases
- Missing `Content-Type` header → **accept** (Groww/niftyindices static CDNs may omit it; upper-layer row/column validation still guards). Matches Dhan.
- `text/csv; charset=utf-8` → accept (charset suffix stripped).
- `TEXT/CSV` (uppercase) → accept (lowercased).
- `text/html` (WAF block page) / `application/json` (error body) → **reject** with `FetchFailed`.
- Non-UTF-8 header bytes → reject (cannot trust it).

## Failure Modes
- Reject path returns `WatchBuildError::FetchFailed` — the SAME error the caller
  already handles with the infinite-retry / degrade logic (NTM-CONSTITUENCY-01 for
  the constituent list; the master fetch retries). No new failure class, no panic.
- A legitimately-typed response is never rejected (allowlist covers the 3 real CSV
  content-types + missing).

## Test Plan
- `crates/core/src/feed/groww/instruments.rs` unit tests (pure fn, no network):
  `test_content_type_missing_is_accepted`, `test_content_type_text_csv_accepted`,
  `test_content_type_charset_suffix_stripped`, `test_content_type_uppercase_accepted`,
  `test_content_type_html_rejected`, `test_content_type_json_rejected`.
- `cargo test -p tickvault-core feed::groww::instruments` green.
- `cargo build -p tickvault-core` clean.

## Rollback
`git revert` — additive pure fn + one call site + tests. No schema, no data, no
behaviour change beyond rejecting non-CSV content-types (which today parse-fail
anyway). Reverting restores the pre-check behaviour exactly.

## Observability
- No new metric/event: the reject reuses the existing `WatchBuildError::FetchFailed`
  → the existing fetch-error logging + (constituent path) NTM-CONSTITUENCY-01 degrade
  alert. The error message names the rejected content-type (no secret — a MIME string).

## Plan Items
- [x] Add `validate_groww_content_type` pure fn + `ALLOWED_GROWW_CSV_CONTENT_TYPES` const, call it in `fetch_text_hardened` (6 unit tests green; `cargo build -p tickvault-core` clean)
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_content_type_missing_is_accepted, test_content_type_text_csv_accepted, test_content_type_charset_suffix_stripped, test_content_type_uppercase_accepted, test_content_type_html_rejected, test_content_type_json_rejected

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 200 + `text/csv` | accepted, body read |
| 2 | 200 + no Content-Type | accepted (static CDN) |
| 3 | 200 + `text/html` (WAF page) | rejected → FetchFailed → retry/degrade |
| 4 | 200 + `application/json` (error body) | rejected → FetchFailed |
| 5 | 200 + `text/csv; charset=utf-8` | accepted |
