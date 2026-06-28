# Implementation Plan: Groww log-hygiene hardening (2 LOW findings)

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** operator (follow-up task to the Groww adversarial security review)

Small hardening follow-up. The adversarial security review of the Groww work
flagged 2 LOW log-hygiene items. This PR verifies each and applies the correct
outcome (fix vs documented false-positive). Changed crate: `tickvault-core`.

## Plan Items

- [x] LOW 1 — sanitize CSV-derived `exchange`/`segment` in the Groww
      shared-master unknown-exchange `warn!`
  - Files: `crates/core/src/feed/groww/shared_master_writer.rs`
  - Tests: `test_unknown_exchange_warn_sanitizes_csv_derived_values`

- [x] LOW 2 — verify the Groww auth-rejection `error!(body = %body)` body is
      already secret-redacted; document as FALSE POSITIVE (no change)
  - Files: (none — investigation only)
  - Evidence: `crates/core/src/feed/groww/auth.rs:213-216` routes the non-2xx
    body through `capture_rest_error_body`; `auth.rs:226-229` uses a fixed
    `&'static str`. Existing redaction ratchet:
    `crates/common/src/sanitize.rs::test_capture_rest_error_body_redacts_jwt`.

## Design

LOW 1: `groww_segment_label`'s defensive unknown-exchange fallthrough logs
`exchange = %other` and `segment = %entry.segment` via tracing `Display`. Both
fields originate from the Groww master CSV (`WatchEntry.exchange` /
`.segment`), so a crafted master row could embed `\n`/`\r`/control/BiDi chars to
forge log lines that flow into CloudWatch/Telegram (log injection). Fix: wrap
both with the existing `tickvault_common::sanitize::sanitize_audit_string`
(strips control/newline/BiDi, caps length) before logging. The message stays a
`&'static str`. The import is unconditional (the warn path is NOT feature-gated).

LOW 2: the `error!(body = %body)` at `groww_activation.rs:285` logs
`GrowwAuthSmokeError::Rejected { body }`. Tracing every construction site of that
variant shows the body is ALWAYS pre-redacted (`capture_rest_error_body` on the
non-2xx path; a fixed string on the 2xx-unparseable path). No raw credential can
reach the log → FALSE POSITIVE, no change.

## Edge Cases

- Hostile `exchange` with embedded newline/CR/control → sanitized, label still
  the safe `&'static str` fallback `"NSE_EQ"`, no panic.
- Clean exchange (`NSE`/`BSE`) never hits the fallthrough → zero behaviour change
  on the hot/common path.
- Empty exchange string → `sanitize_audit_string("")` returns `""` (safe).
- Feature `daily_universe_fetcher` ON and OFF: the warn path + import are
  unconditional, so both builds compile identically.

## Failure Modes

- The change is log-only; it cannot affect tick capture, persistence, the feed,
  or any order/recovery path. `groww_segment_label` still returns the same
  `&'static str` for every input.
- No new allocation on any hot path — `groww_segment_label` runs once per daily
  master entry (cold path), and the sanitizer only runs on the impossible
  unknown-exchange branch.

## Test Plan

- New unit test `test_unknown_exchange_warn_sanitizes_csv_derived_values`:
  (1) hostile exchange still maps to `NSE_EQ` without panic;
  (2) `sanitize_audit_string` strips `\n`/`\r` from the hostile value;
  (3) source-scan ratchet pins both `sanitize_audit_string(other)` and
      `sanitize_audit_string(&entry.segment)` in the `groww_segment_label` body.
- Existing `groww_segment_label` permutation tests stay green (no behaviour
  change for known exchanges).
- LOW 2: no new test — cite the existing
  `test_capture_rest_error_body_redacts_jwt` redaction ratchet.

## Rollback

Pure log-line change behind a defensive branch. Revert the single commit to
restore the prior `%other` logging; no data migration, no schema, no config.

## Observability

No new metric/event/ErrorCode. The existing unknown-exchange `warn!`
(`tracing::warn`) is preserved verbatim except its two CSV-derived fields are now
sanitized. No counter/alert change.

## Per-Item Guarantee Matrix

This plan item carries the project-wide guarantee contract by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row "100% everything"
matrix + the 7-row resilience matrix), mirroring
`.claude/plans/active-plan-groww-second-feed.md`. Item-specific proof:

- 100% code coverage: new branch covered by
  `test_unknown_exchange_warn_sanitizes_csv_derived_values`.
- 100% testing coverage: unit + source-scan ratchet for the changed crate
  (`tickvault-core`), per `testing-scope.md` (block-scoped default).
- 100% security: CSV-derived untrusted input now sanitized before logging
  (LOW 1); LOW 2 confirmed already-redacted with grep evidence.
- 100% logging: message stays a `&'static str`; tracing macros only; no secret
  ever logged (the whole point of the change).
- O(1) / zero-alloc: no hot-path allocation introduced; the sanitizer runs only
  on the impossible unknown-exchange branch (cold path).
- Uniqueness/dedup: unchanged (no DEDUP key touched).
- Extreme check: the source-scan ratchet fails the build if a future refactor
  drops the sanitizer.

Honest 100% claim: 100% inside the tested envelope, with a ratcheted source-scan
guard that fails the build on regression. The change is log-line-only and cannot
affect ticks, persistence, the feed, or any order path.
