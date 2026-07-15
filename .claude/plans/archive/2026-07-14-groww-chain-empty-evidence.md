# Implementation Plan: Groww chain Empty-arm evidence + empty-vs-leg_shape_drift split

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** coordinator relay (operator top-priority directive, 2026-07-14)

> **Per-item guarantee matrix:** every item below carries the 15-row + 7-row
> guarantee matrices by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrix;
> this plan's items are cold-path REST-leg evidence/classification changes —
> the hot-path rows are N/A by construction, the coverage/logging/testing rows
> are satisfied by the ratchet tests + coded log lines listed per item).

## Design

**The proven incident (2026-07-14):** the Groww option-chain leg
(`crates/app/src/groww_option_chain_1m_boot.rs`) classified NIFTY
(expiry_date=2026-07-14, expiry day) as `outcome=empty` every minute
14:54→15:29 IST while BANKNIFTY/SENSEX parsed fine and Groww's own chart kept
printing NIFTY option candles. Every body WAS a parseable chain envelope with
a `strikes` JSON object that yielded zero legs (`errors=0` all window — a
shape-drifted or FAILURE body would have classified Failed/parse, not empty).
The decisive discriminator — was the body ~40 B (truly empty map) or ~37 KB
(entries our leg extraction dropped)? — was STRUCTURALLY UNRECORDED: the
`tv_groww_chain1m_payload_bytes` histogram and the `strikes_kept` /
`invalid_strikes` counts are recorded ONLY on the `Found` arm; the `Empty`
arm discarded the parsed struct and captured no body evidence.

**The fix (bounded, default-safe):**

1. **Structured parse diagnostics.** `GrowwParsedChain` gains
   `strikes_seen: u32` — the RAW entry count of the `strikes` map (every map
   entry increments it before the invalid/cap/kept triage). The existing
   `strikes_kept` / `invalid_strikes` / `truncated_strikes` fields already
   cover the triage; `strikes_seen == kept + invalid + truncated` by
   construction. The `Found` arm's behavior and metrics stay byte-identical.
2. **Empty-arm evidence (the core).** A new private `ChainZeroLegEvidence`
   bundle — `payload_bytes`, `strikes_seen`, `strikes_kept`,
   `invalid_strikes`, and a BOUNDED SANITIZED `body_sample` produced by the
   EXACT house choke point the failure arms already use
   (`tickvault_common::sanitize::capture_rest_error_body` — control-char
   strip → URL/credential-param redaction → JWT-shape redaction →
   credential-JSON-field redaction → ≤300-char truncation). The
   `GrowwChainFetchOutcome::Empty` variant carries it; the fire arm emits ONE
   coded `error!(code = CHAIN-02, stage = "empty_chain", …)` per empty
   underlying per fired minute (bounded ≤3/min) carrying every evidence field
   + the body sample, and records the new
   `tv_groww_chain1m_empty_payload_bytes` histogram (unlabeled — the existing
   `tv_groww_chain1m_payload_bytes` series stays Found-only so its dashboard
   semantics never shift).
3. **Honest reclassification of drift.** The zero-legs case splits via the
   pure `zero_leg_outcome` classifier:
   - `strikes_seen == 0` (map literally empty) → stays `Empty` (a genuinely
     empty chain; `outcome="empty"`, audit `Empty`/`error_class="empty_chain"`
     — unchanged wire values).
   - `strikes_seen > 0` but zero legs extracted → the NEW
     `GrowwChainFetchOutcome::LegShapeDrift` variant: the vendor served
     entries our leg extraction couldn't read — that is an ERROR, not an
     empty chain. It flows into the minute verdict's `errors` count (never
     `empty`), fires the same coded CHAIN-02 `minute_failed` accounting, its
     own per-underlying `error!(stage = "leg_shape_drift", …)` evidence line
     + `tv_groww_chain1m_leg_shape_drift_total` counter, and writes the audit
     row with the EXISTING `RestFetchOutcome::Error` outcome +
     `error_class="leg_shape_drift"` (`error_class` is a free-form
     `&'static str` SYMBOL value — NO audit schema change).
   - **PR #1537 `UnderlyingServedTracker` interaction (verified):** the
     tracker consumes FETCH-level served verdicts where `served == Found`;
     BOTH `Empty` and any `Failed`-class verdict already push
     `served = false` (`test_underlying_not_served_error_class_counts_like_empty`
     pins that an error-class minute counts exactly like an empty one), so
     re-classifying drift from Empty→error leaves the not-served streaks,
     pages, and recoveries byte-identical. The tracker is unaffected.
4. **Rule-file edit.** `rest-1m-pipeline-error-codes.md` §2c gains a dated
   2026-07-14 paragraph (appended after the `underlying_not_served` one):
   the 14:54 evidence gap, the new evidence fields + bounded sanitized sample
   on every empty/drift classification, the empty-vs-leg_shape_drift split,
   and the honest note that today's incident class could not be discriminated
   retroactively — from this change forward it is, within one minute.

No new ErrorCode variant (CHAIN-02 stages/classes reused). No audit schema
change. No hot-path involvement (cold-path per-minute REST leg only). No
`unwrap`/`expect` in prod. Static metric labels only.

## Edge Cases

- **Empty map with wrapper noise** (`{"payload":{"underlying_ltp":…,"strikes":{}}}`)
  → Empty, `strikes_seen=0`, body_sample = the sanitized ~40-B body.
- **Entries present, every leg null/non-object** (the suspected 14:54 shape)
  → LegShapeDrift, `strikes_seen>0`, `strikes_kept>0`, `legs=0`.
- **Entries present, every key implausible** (`"-5"`, `"1e300"`, non-numeric)
  → LegShapeDrift with `strikes_kept=0`, `invalid_strikes=strikes_seen` —
  still drift (the vendor served SOMETHING our extraction dropped).
- **All entries past the strike cap with zero legs** — unreachable in
  practice (a kept strike below the cap either yields a leg or not; cap-only
  drops require `strikes_kept == MAX` first, which implies earlier kept
  entries were leg-checked) — covered by the generic `strikes_seen>0` rule.
- **Body sample bounding:** a 37-KB body truncates to ≤300 chars; a body
  carrying a JWT-shaped string or credential JSON field is redacted BEFORE
  truncation (the sanitizer's documented ordering).
- **Found arm:** byte-identical — same metrics, same rows, same anchor
  updates.
- **Parse-Failed (non-chain 2xx / FAILURE envelope):** unchanged — `None`
  from the parser still classifies `Failed(status=200, "parse")`.
- **Auth short-circuit / no-token / budget arms:** untouched.

## Failure Modes

- **The evidence log line itself cannot fail the fire:** it is a bounded
  `error!` emit (log-sink-only per §3 delivery boundary — CHAIN-02 has no
  CloudWatch filter, so no paging-storm risk; the escalation edge still owns
  the page).
- **Sanitizer returns empty** (empty body) → `body_sample=""` — honest
  (an empty-string body is itself evidence).
- **Drift misclassification risk:** a legitimately-empty-but-noisy map cannot
  reach drift (`strikes_seen` counts ONLY real `strikes` map entries); a
  drifted body cannot hide as Empty (any entry sets `strikes_seen>0`).
- **Minute-verdict impact:** drift now counts in `errors` — the escalation
  edge (`ok==0` fully-failed minutes) and the not-served tracker are
  unaffected in threshold or semantics (verified above); the only behavior
  change is the honest empty→error relabeling of drift minutes.
- **Metric cardinality:** both new series are label-free
  (`tv_groww_chain1m_empty_payload_bytes` histogram,
  `tv_groww_chain1m_leg_shape_drift_total` counter) — zero cardinality risk,
  /metrics-local, zero CloudWatch cost delta.

## Test Plan

Pure body-class → classification matrix (no I/O; in-module tests):
- (a) `strikes:{}` → Empty with `strikes_seen=0` + evidence populated
- (b) strikes with 2 valid entries but CE/PE null/non-object →
  LegShapeDrift, `strikes_seen=2`, `strikes_kept=2`, `legs=0`
- (c) strikes with all-implausible keys → LegShapeDrift
  (`strikes_seen>0`, `strikes_kept=0`, `invalid_strikes=strikes_seen`)
- (d) normal body → Found unchanged
- (e) FAILURE envelope / non-chain 2xx → parse-Failed unchanged
- (f) the body sample is length-bounded (≤300 chars) and passes through the
  sanitizer (JWT-shaped input never survives) — tested via `zero_leg_outcome`
  directly
- Existing mock tests extended: `test_fetch_bounded_found_empty_and_parse_failure_via_mock`
  (the Empty arm now asserts the evidence fields) and
  `test_fire_token_path_found_and_empty_via_mock` (a third drift target —
  the tracker sees it not-served exactly like empty).
- Gates: `cargo fmt --check`; `cargo clippy -p tickvault-app -p tickvault-core
  -p tickvault-common -- -D warnings -W clippy::perf`; `cargo test -p
  tickvault-app -p tickvault-core -p tickvault-common` (only the 2 known
  pre-existing core env-dependent SSM failures tolerated);
  `bash .claude/hooks/banned-pattern-scanner.sh`; plan-gate + plan-verify.

## Rollback

Single-module revert: `git revert` of the feature commit restores the prior
Empty-arm behavior (unit variant, no evidence) and removes the drift split;
the audit table is untouched (no schema change — `error_class` is a free-form
SYMBOL value, so rows written with `leg_shape_drift` remain queryable after a
rollback). The rule-file paragraph is dated and additive; no config flag is
needed because the change is pure classification/evidence (no new network
call, no new table, no cadence change).

## Observability

- NEW coded lines (both `error!` with `code = CHAIN-02`, so they land in
  errors.jsonl within one minute of occurrence): `stage="empty_chain"` (the
  Empty evidence line) and `stage="leg_shape_drift"` (the drift line) — each
  carrying `payload_bytes`, `strikes_seen`, `strikes_kept`,
  `invalid_strikes`, `body_sample` (bounded + sanitized), `symbol`, `minute`.
- NEW metrics: `tv_groww_chain1m_empty_payload_bytes` (histogram, Empty arm)
  + `tv_groww_chain1m_leg_shape_drift_total` (counter). Existing
  `tv_groww_chain1m_fetch_total{outcome}` label VALUES unchanged (drift
  counts as `error`); `tv_groww_chain1m_payload_bytes` stays Found-only.
- Audit: drift rows land as `outcome=error` / `error_class="leg_shape_drift"`
  in `rest_fetch_audit` (existing enum variants; NO schema change); empty
  rows stay `outcome=empty` / `error_class="empty_chain"`.
- Runbook: `rest-1m-pipeline-error-codes.md` §2c dated paragraph.

## Plan Items

- [x] Item 1 — parser diagnostics: `strikes_seen` on `GrowwParsedChain`
  - Files: crates/app/src/groww_option_chain_1m_boot.rs
  - Tests: test_parse_groww_option_chain_invalid_and_absurd_strikes_counted,
    test_parse_groww_option_chain_strike_cap_truncates_and_counts,
    test_parse_groww_option_chain_hostile_shapes_never_panic
- [x] Item 2 — `ChainZeroLegEvidence` + pure `zero_leg_outcome` classifier +
  the Empty/LegShapeDrift outcome split
  - Files: crates/app/src/groww_option_chain_1m_boot.rs
  - Tests: test_zero_leg_outcome_matrix_empty_vs_drift,
    test_zero_leg_outcome_body_sample_bounded_and_sanitized
- [x] Item 3 — fire-arm wiring: Empty evidence error line + empty payload
  histogram; drift arm → errors count + drift counter + coded line + audit
  row (`RestFetchOutcome::Error`, `error_class="leg_shape_drift"`)
  - Files: crates/app/src/groww_option_chain_1m_boot.rs
  - Tests: test_fetch_bounded_found_empty_and_parse_failure_via_mock,
    test_fetch_bounded_leg_shape_drift_via_mock,
    test_fire_token_path_found_and_empty_via_mock
- [x] Item 4 — rule-file dated paragraph (§2c, 2026-07-14 evidence-gap note)
  - Files: .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: n/a (docs; cross-ref tests stay green — no new ErrorCode)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 2xx, `strikes:{}` (~40 B) | Empty; evidence line with payload_bytes≈40, strikes_seen=0; audit empty/empty_chain |
| 2 | 2xx, ~37 KB strikes map, zero extractable legs | LegShapeDrift; errors count; evidence line + drift counter; audit error/leg_shape_drift |
| 3 | 2xx, normal chain | Found — byte-identical to today |
| 4 | 2xx, FAILURE envelope / not-a-chain | Failed(200, "parse") — unchanged |
| 5 | Body carries a JWT-shaped token | body_sample redacts it; ≤300 chars |
| 6 | NIFTY drift while siblings serve | not-served streak counts exactly as before (tracker unaffected) |
