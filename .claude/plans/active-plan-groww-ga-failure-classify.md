# Implementation Plan: Groww spot 2xx GA-FAILURE envelope → error classification (G1) + ga_code forensics

**Status:** APPROVED
**Date:** 2026-07-14
**Approved by:** coordinator-relayed operator full-coverage directive 2026-07-14 (G1 build authorization)

Crate: `app` (`tickvault-app`). Files: `crates/app/src/groww_spot_1m_boot.rs`,
`crates/app/src/groww_option_chain_1m_boot.rs`,
`.claude/rules/project/rest-1m-pipeline-error-codes.md`.

Guarantee matrices: 15-row + 7-row by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (this item is a cold-path
classification fix — no hot-path involvement, no new table, no new ErrorCode;
item-specific rows below in Observability / Test Plan).

## Design

**The bug (G1, Verified):** the Groww spot per-minute REST leg
(`crates/app/src/groww_spot_1m_boot.rs`) parses a 2xx body via
`parse_groww_1m_candle_rows`, which sniffs the Groww FAILURE envelope
(`{"status":"FAILURE","error":{code,message,metadata}}`, wire codes
GA000/GA001/GA003–GA007 per `docs/groww-ref/16-orders-margins-portfolio.md`
§5 — the envelope wins over the HTTP status) at ~:574 and returns an EMPTY
candle set with zero malformed rows. The ladder then classifies the rung
`target_absent` → the minute lands `outcome="empty"` — a broker-side reject
misreported as a benign no-candle minute (a Rule-11 honesty bug).

**The fix (spot leg):**
1. New pure sniffer `parse_groww_ga_failure(body) -> Option<GrowwGaFailure>`
   (`ga_code` bounded ≤16 chars, `"none"` when the code field is absent;
   `message` redacted at the emit site via the existing
   `capture_rest_error_body`).
2. In `fetch_minute_with_ladder`'s `Ok(body)` arm, the envelope check runs
   BEFORE candle parsing: a FAILURE envelope makes the rung a FAILED attempt
   (`last_error = Some("2xx FAILURE envelope ga_code=… msg=…")`,
   `forensics.error_class = "ga_failure"`, `final_http_status = 200`) and the
   bounded ladder re-polls the next rung exactly like any non-auth HTTP
   failure. Policy NEVER branches on the GA code value — forensics only
   (no short-circuit on GA005; auth short-circuit stays HTTP-401/403-only).
3. `SymbolFetchOutcome::Failed` gains `ga_code: Option<String>` mirroring
   `last_error` semantics (a later clean 2xx or HTTP-level failure clears it,
   so the code always describes the FINAL failure).
4. `fire_one_minute` pairs the first failure sample with its ga_code;
   `record_minute_verdict` emits `ga_code = …` (`"none"` when absent) on BOTH
   coded SPOT1M-01 lines (`stage="minute_failed"` + `stage="escalation"`).
   The `rest_fetch_audit` row carries `error_class="ga_failure"`, outcome
   `Error` (via the unchanged `audit_outcome_for` — not rate_limited).
   The VIX `vix_empty` counter no longer fires for GA failures (they are
   errors, not emptiness) — automatic via the Failed arm.

**Chain leg (forensics only — classification already correct):**
`parse_groww_option_chain` returns `None` on the FAILURE envelope (no strikes
map) → already `GrowwChainFetchOutcome::Failed{status:200,…}`. The `None` arm
now sniffs the envelope via the shared `parse_groww_ga_failure` and enriches
the failure `msg` with `ga_code=… msg=<redacted>` so the existing CHAIN-02
failure log/audit context carries the GA code. No classification change.

**G9 verdict: DEFERRED (arithmetic below).** Dhan's spot ladder applies
`SPOT_1M_REST_429_EXTRA_BACKOFF_MS` (+2 s before the next rung on a 429).
Bundling the same on the Groww spot ladder does NOT trivially fit:
- Current Groww schedule bound (const-asserted): last offset 6,000 ms + one
  request timeout 5,000 ms = 11,000 ms < `GROWW_SPOT_1M_SYMBOL_BUDGET_SECS`
  14,000 ms ✓.
- With Dhan-style +2 s before each of the 4 re-poll rungs: 6,000 + 4×2,000 +
  5,000 = **19,000 ms > 14,000 ms** ✗ — the hard per-symbol timeout would cut
  a 429-storm ladder mid-flight, flipping the honest `rate_limited`
  classification into `budget_exceeded` and violating the const-assert intent
  ("the timeout only fires on genuine stalls").
- Widening the budget to ≥20 s breaks the whole-fire assert: 300 ms fire
  delay + (3 core + 1 VIX)×20,000 = **80,300 ms > 60,000 ms** ✗ (sequential
  4-target fire must finish inside the minute).
- A capped single +2 s backoff (6,000+2,000+5,000 = 13,000 < 14,000) WOULD
  fit but diverges semantically from the Dhan pattern → needs its own
  reviewed design, not a trivial bundle. DEFERRED with this arithmetic as
  evidence.

**Housekeeping (same PR, mandated):** archived the two fully-merged plans
(`questdb-partition-s3-archive` → merged #1504; `spot-1m-diagnostics` →
merged #1524) per plan-enforcement rule 7, keeping the active-plan count at
the V7 cap (4 + this plan = 5).

## Edge Cases

- FAILURE envelope with NO `error.code` field → `ga_code = "none"` (tolerated,
  tested).
- FAILURE envelope with a huge/hostile `code` string → bounded ≤16 chars;
  `message` runs through `capture_rest_error_body` (≤300 chars, secret/JWT
  redaction) — never raw body text in logs.
- GA failure on rung 1, clean 2xx target-absent on rung 2 → final verdict is
  honestly `Empty` (vendor recovered; the minute genuinely absent) and
  `ga_code` clears with `last_error`.
- GA failure on rung 1, HTTP failure on the last rung → `Failed` with the
  HTTP reason, `ga_code = None` (the code describes the FINAL failure only).
- `status:"SUCCESS"` and bare-`candles` bodies → unchanged empty/ok paths
  (existing tests keep passing untouched).
- Non-JSON / no-candles bodies → unchanged `malformed_rows` accounting.
- Budget-timeout arm constructs `Failed { ga_code: None, … }` (sentinel
  discipline unchanged).
- No-token arm: `sample_ga_code` stays `None` → `ga_code = "none"` on the
  verdict line.

## Failure Modes

- A GA-failure storm now feeds the minute_failed coalesced log + the
  3-consecutive-minutes escalation edge (the SAME page a non-2xx storm
  produces) — previously it silently rode the `empty` class and only reached
  the edge via `fully_failed` core math without an error classification or a
  correct audit outcome. No NEW page class is introduced; severity/paging
  semantics of SPOT1M-01 are unchanged (reuse, 0 new ErrorCode variants).
- Misclassification risk the other way (a genuine empty flagged as error):
  impossible by construction — the sniffer requires the literal
  `status == "FAILURE"` top-level field; SUCCESS/bare bodies never match.
- Chain leg: msg enrichment only — a sniff miss degrades to the existing
  generic "not a parseable option chain" msg (never a panic; pure serde_json
  value walking, no unwrap).
- Cold path only: no tick hot-path involvement; per-fire extra cost is one
  serde_json parse of an already-small failure body on the failure arm.

## Test Plan

Unit tests in `crates/app/src/groww_spot_1m_boot.rs` (module test style):
- [x] `test_parse_groww_ga_failure_envelope_extracts_code_and_message` — 2xx
  FAILURE body → `Some`, ga_code `GA005`, message extracted; absent code →
  `"none"`; hostile long code bounded; SUCCESS/bare/non-JSON bodies → `None`.
- [x] `test_ladder_classifies_2xx_ga_failure_as_error_never_empty` — mock
  server serving a 2xx FAILURE envelope → `SymbolFetchOutcome::Failed` with
  `ga_code = Some("GA005")`, forensics `error_class = "ga_failure"`,
  `final_http_status = 200`, attempts == full ladder (no auth short-circuit);
  `audit_outcome_for` maps it to `RestFetchOutcome::Error`.
- [x] Existing empty/ok/malformed parser tests unchanged and green
  (`test_parse_groww_1m_candles_*`).
- [x] Chain leg: `test_chain_parse_failure_msg_carries_ga_code` (in
  `groww_option_chain_1m_boot.rs` tests) — the None-parse arm's msg carries
  `ga_code=` when the body is a FAILURE envelope.
- [x] G9: NOT bundled — no backoff test (deferred; arithmetic in Design).

Gates: `cargo fmt --check`, `cargo clippy -p tickvault-app -- -D warnings
-W clippy::perf`, `cargo test -p tickvault-app` (scoped per
testing-scope.md), banned-pattern scanner, plan-gate, plan-verify.

## Rollback

Single revert of this branch's commit(s) restores the prior (buggy but
benign) `empty` classification; no schema change, no config change, no new
table/column, no new metric label family (reuses `outcome="error"` +
`error_class` SYMBOL column already free-form static). The rule-file note is
reverted with the same commit. No deploy coupling.

## Observability

- Coded SPOT1M-01 lines (`stage="minute_failed"` / `stage="escalation"`,
  `feed="groww"`) gain a `ga_code` field (`"none"` when absent) — forensic
  WHY per the operator's blame-attribution discipline; log-sink-only, no new
  pager (delivery boundary of `rest-1m-pipeline-error-codes.md` §3
  unchanged).
- `tv_groww_spot1m_fetch_total{outcome="error"}` now counts GA-failure
  minutes (was `empty`) — the honest class; `rest_fetch_audit` rows carry
  `error_class="ga_failure"` + outcome `error` (named rows, never silent).
- Chain leg CHAIN-02 failure msg carries `ga_code=` on FAILURE envelopes.
- Rule-file dated note added to
  `.claude/rules/project/rest-1m-pipeline-error-codes.md` (§1 Groww emit
  section) citing the 2026-07-14 coordinator-authorized build.

## Plan Items

- [x] Spot leg: FAILURE-envelope sniffer + ladder reclassify + ga_code field
  — Files: crates/app/src/groww_spot_1m_boot.rs — Tests:
  test_parse_groww_ga_failure_envelope_extracts_code_and_message,
  test_ladder_classifies_2xx_ga_failure_as_error_never_empty
- [x] Chain leg: ga_code forensics in the None-parse failure msg — Files:
  crates/app/src/groww_option_chain_1m_boot.rs — Tests:
  test_chain_parse_failure_msg_carries_ga_code
- [x] Rule-file dated 2026-07-14 note — Files:
  .claude/rules/project/rest-1m-pipeline-error-codes.md
- [x] G9 assessment: DEFERRED with explicit arithmetic (see Design)
- [x] Archive the two merged plans (plan-enforcement rule 7)
