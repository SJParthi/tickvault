# Implementation Plan: Groww feed-rejection Telegram carries the sanitized reject reason

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — 2026-07-14 demand relayed via coordinator session (the 14:59 IST Groww rejection page carried no reason)

> **Guarantee matrices:** this plan carries the 15-row "100% everything" matrix
> and the 7-row Resilience matrix BY CROSS-REFERENCE to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (per that rule's
> "or cross-reference it" clause). Item-specific deltas: cold-path
> notification code only — no hot-path allocation, no new tick-drop path, no
> DEDUP/table change, no new WS, no new ErrorCode; the per-item proofs are the
> unit + source-scan ratchet tests named in the Test Plan below.

## Design

The 2026-07-14 14:59 IST incident: the Groww live feed (the SOLE live feed)
died and the operator page said only "the feed reported an error and is
retrying" — the actual cause (`ERROR growwapi.groww.nats_client: Error:
nats: unexpected EOF`) was already captured as the FEED-REJECT-01 signature
but never reached Telegram. Operator demand: the page MUST carry the
specific captured reason.

Three changes, one PR:

1. **`crates/app/src/groww_sidecar_supervisor.rs`** — new pure helper
   `sidecar_reject_detail(line: &str) -> Option<String>`: exactly
   `sidecar_line_signature(line)` (the EXISTING sanitize choke point —
   control-char + BiDi strip, credential/JWT redaction, 160-char cap),
   `None` when the sanitized signature is empty/whitespace. The
   once-per-episode alert edge in `spawn_pipe_drain` (the Passthrough
   single-conn arm that fires `NotificationEvent::GrowwSidecarRejected`)
   threads `detail: sidecar_reject_detail(&line)` into the event. The
   fleet-coalesced `EmitSummary` arm passes `detail: None` (one child's
   line is not the cause for N connections). NEVER raw child text — only
   the choke-point output. All three alert classes (AuthRejected /
   EntitlementRejected / Error) keep their existing fixed plain-English
   explanation (`alert_reason()`) AND gain the specific sanitized reason.
2. **`crates/core/src/notification/events.rs`** — the
   `GrowwSidecarRejected` variant gains `detail: Option<String>` (field doc
   records the sanitize contract: MUST be the `sidecar_line_signature`
   choke-point output, never raw child text). Body render: when detail is
   present and non-empty, the headline becomes
   `🆘 <b>Groww live feed rejected: {detail} — retrying</b>`; the class
   explanation line + trailer are unchanged. Detail is re-capped to 160
   chars (function-local named const, mirroring
   `SIDECAR_LINE_SIGNATURE_MAX_CHARS` — core cannot import app) and
   html-escaped at the render boundary (defense-in-depth). `None`/empty
   detail renders the exact pre-change generic wording. The severity emoji
   + 🟢 GROWW badge ordering and the `feed_badge()` match arms are
   UNTOUCHED (PR #1529 owns those).
3. **`.claude/rules/project/feed-stall-watchdog-error-codes.md` §1c.1**
   (edited FIRST, dated 2026-07-14) — records the operator demand and the
   conscious dated override of Telegram commandment 2 for this one field
   (precedent: the B9 `Build:` short-SHA override in
   `deploy-provenance.md`).

## Edge Cases

- **Empty / whitespace-only sanitized signature** (e.g. a line that is all
  control/BiDi chars): `sidecar_reject_detail` returns `None`; the body
  degrades to the exact current generic wording — never a hollow
  "rejected:  — retrying".
- **Hostile line** (embedded JWT, control chars, >160 chars, BiDi
  overrides, `<script>`): reaches the body ONLY as the sanitized truncated
  signature ([REDACTED-JWT], chars stripped, ≤160 chars) and is
  additionally html-escaped + re-capped at the render boundary.
  Truncation happens BEFORE html-escape so an escape entity is never cut
  mid-sequence.
- **Fleet summary** (`fleet_summary: true`): `detail: None` — the fleet
  wording is byte-identical to today.
- **Other emit sites**: the field is `Option<String>`; the only two
  construction sites (both in the supervisor) are updated in the same PR;
  events.rs tests updated with `detail: None`.
- **Detail containing library jargon** (`growwapi`, `nats:`): allowed BY
  the dated §1c.1 commandment-2 override for this one field; the existing
  no-jargon ratchet test keeps asserting on the detail-less message only.

## Failure Modes

- **Sanitizer regression** (raw secret reaching Telegram): impossible
  without breaking `sidecar_line_signature`'s own ratchet tests
  (`test_sidecar_line_signature_caps_redacts_and_strips`) — the detail is
  DEFINED as that function's output; a new source-scan ratchet pins the
  Passthrough arm to `detail: sidecar_reject_detail(&line)`.
- **A future emit site passing an unbounded/unsanitized string**: the
  render boundary re-caps to 160 chars + html-escapes; the field doc
  states the contract.
- **Notification path failure**: unchanged — this PR does not touch the
  dispatch/coalescer/episode machinery; a Telegram send failure still
  terminates at the existing TELEGRAM-01 loudness.
- **Compile breakage of shared consumers**: `GrowwSidecarRejected` is
  Groww-only (verified: constructed only in
  `groww_sidecar_supervisor.rs`; matched elsewhere only via `{ .. }`).

## Test Plan

- `crates/app/src/groww_sidecar_supervisor.rs` (tests module):
  - `test_sidecar_reject_detail_is_sanitized_signature_or_none` — (a)
    detail == `sidecar_line_signature(line)` for a real reject line; (b)
    hostile JWT/control/overlong line → sanitized + ≤160 chars; (c)
    empty / whitespace / control-only line → `None`.
  - `test_reject_page_carries_sanitized_detail` (source-scan ratchet) —
    the Passthrough notify arm contains
    `detail: sidecar_reject_detail(&line)` and the fleet EmitSummary arm
    contains `detail: None`.
- `crates/core/src/notification/events.rs` (tests module):
  - `test_groww_sidecar_rejected_detail_renders_reason_and_retrying` —
    detail `Some("ERROR growwapi.groww.nats_client: Error: nats: unexpected EOF")`
    → body contains `live feed rejected: … nats: unexpected EOF — retrying`
    verbatim AND keeps the class explanation ("the feed reported an error
    and is retrying") AND the trailer.
  - `test_groww_sidecar_rejected_empty_detail_degrades_to_generic` —
    `None` / `Some("")` / `Some("   ")` all render the exact generic
    headline; no "rejected:" + "—" hollow form.
  - `test_groww_sidecar_rejected_detail_html_escaped_and_recapped` —
    `<script>` detail escaped; >160-char detail capped at the render
    boundary.
  - Existing tests updated with `detail: None` (assertions unchanged —
    incl. the no-jargon pin, which stays valid for the detail-less form).
- Commands: `cargo fmt --check`, `cargo clippy -p tickvault-core -p
  tickvault-app -- -D warnings -W clippy::perf`,
  `cargo test -p tickvault-core -p tickvault-app` (block-scoped per
  `testing-scope.md`; `crates/common` untouched → no workspace
  escalation).

## Rollback

Single revert of this PR restores the fixed-string-only wording (the
`detail` field and helper are additive; no schema, no config, no persisted
data, no migration). No config toggle is needed — the change is pure
notification wording; a rollback loses only the reason-in-page visibility
and re-opens the operator's 2026-07-14 complaint.

## Observability

- The FEED-REJECT-01 coded `error!` (code + class + signature) is
  UNCHANGED — errors.jsonl / CloudWatch keep the forensic record; its
  message text is updated to say the signature now also rides the Telegram
  body (no longer "Telegram wording unchanged").
- The Telegram page itself IS the observability delta: the
  `GrowwSidecarRejected` HIGH page now names the specific sanitized cause.
- No new ErrorCode, no new counter, no new alarm (the page + the existing
  FEED-REJECT-01 / stall-restart pagers already cover detection); the
  cross-ref + tag-guard test suites are unaffected.
- Rule file `.claude/rules/project/feed-stall-watchdog-error-codes.md`
  §1c.1 documents the dated override + ratchets.
