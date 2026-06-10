# Implementation Plan: DHAN-REST-400 — root-cause visibility + false-clean guard + REST canary

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator directive 2026-06-10 — task DHAN-REST-400 + addendum 1b; the task message IS the approval per the directive "6-section APPROVED plan first")

Incident: every `api.dhan.co` REST call returned HTTP 400 on 2026-06-10 (profile +
getIP at 08:45; ALL 776/776 cross-verify intraday fetches at 15:33). WS feed
unaffected. Jun 4's cross-verify CSV is byte-identical header-only → REST likely
broken since ~Jun 4 and nobody could tell, because (a) error paths drop Dhan's
response body, (b) a 776/776-failure day produces a CSV indistinguishable from a
perfect day, (c) there is no intraday REST-health probe, (d) the "final audit
flush failed" alarm at `cross_verify_1m_boot.rs:567` fires even with ZERO pending
rows, and (e) no error log captures the final request URL (1b: a trailing-slash
base-URL override producing `…/v2//path` would 400 every REST endpoint at once
while the WS — separate URL construction — stays healthy).

## Design

Five independent fixes, one PR (single incident, shared helpers):

1. **Bounded secret-redacted body + final-URL capture** (`crates/common/src/sanitize.rs`):
   new `REST_BODY_CAPTURE_MAX_CHARS = 300`, `redact_jwt_like()` (replaces any
   `eyJ…`-prefixed JWT-shaped run with `[REDACTED-JWT]`), and
   `capture_rest_error_body()` = url-param redaction → JWT redaction → control-char
   strip → 300-char UTF-8-safe truncation. Applied at every REST error path:
   `token_manager.rs::get_user_profile/acquire_token/try_renew_token`,
   `ip_verifier.rs::get_ip/set_ip/modify_ip`,
   `cross_verify_1m_boot.rs::dhan_intraday_fetch`, and the new canary. Each of
   those error logs/reasons also carries the **exact final request URL** passed
   through `redact_url_params` (1b).
2. **URL-join normalization** (`crates/common/src/url_join.rs`, new):
   `join_api_url(base, path)` trims trailing `/` from base and guarantees exactly
   one `/` between base and path — `"…/v2/" + "/ip/getIP"` can never produce
   `v2//ip/getIP`. All Dhan REST URL constructions in `token_manager.rs`,
   `ip_verifier.rs`, `cross_verify_1m_boot.rs`, and the canary switch to it.
3. **False-clean guard (audit Rule 11)** (`crates/app/src/cross_verify_1m_boot.rs`):
   new `RunStatus { Blind, Degraded, Pass }` + `classify_run_status(summary)`
   (compared == 0 → BLIND; degraded-fraction breach with compared > 0 → DEGRADED;
   else PASS) + `csv_status_line()` written as the FIRST line of the per-day CSV
   (`# status=BLIND compared=0 …`). New typed
   `NotificationEvent::CrossVerify1mSummary` (BLIND/DEGRADED → Severity::High,
   PASS → Severity::Info) emitted unconditionally from the boot wiring after every
   run — never silent.
4. **REST-health canary** (`crates/app/src/rest_canary_boot.rs`, new): one cheap
   `GET /v2/profile` at 09:05 + 12:00 + 15:25 IST on trading days. Non-2xx →
   `error!(code = "REST-CANARY-01")` (new `ErrorCode::RestCanary01ProbeFailed`,
   Severity::High → Telegram via the 5-sink pipeline) with status + final URL +
   captured body. Send-leg failure retries once after 30s before paging (mirrors
   the mid-session watchdog's transient-network lesson, 2026-04-26).
5. **Flush false-alarm fix** (`cross_verify_1m_boot.rs`): root cause — flush is
   called unconditionally; on a zero-mismatch day the buffer is empty and a
   disconnected ILP sender still errors "not connected", logging "rows may be
   unpersisted" when pending = 0. New `final_flush(&mut writer) -> FlushOutcome`
   skips when `pending() == 0` and, on a real failure, logs the error chain +
   exact pending row count.

## Edge Cases

- Body capture: empty body; body containing the full JWT (echoed `access-token`);
  body containing `dhanClientId=`/`pin=`/`totp=`; multi-byte UTF-8 at the 300-char
  boundary; control chars / newlines in the body; HTML WAF block page (>300 chars).
- URL join: base with/without trailing `/` × path with/without leading `/`;
  scheme `https://` double-slash must be preserved; empty path.
- Run status: compared == 0 with fetch_failures == 0 (forced run on a quiet day)
  → still BLIND (zero vouching, Rule 11 — never report clean on an empty compare
  set); exactly-at-threshold degraded fraction; instruments_checked == 0.
- Canary: boot after 15:25 IST → zero probes today, no page; boot at 10:00 →
  probes at 12:00 + 15:25 only; non-trading day → skip silently; token absent at
  probe time → page (REST cannot be healthy without a token); transient DNS blip
  → retry once, only then page.
- Flush: pending == 0 + disconnected (today's false alarm) → skip, no error;
  pending > 0 + disconnected → error with count; pending > 0 + connected → Ok.

## Failure Modes

- Redaction misses a secret → ratchet test feeds a real-shaped JWT through every
  capture path and asserts no 20+-char token substring survives; `redact_url_params`
  composition keeps the existing pin/totp/clientId guarantees.
- Canary itself spams → max 3 probes/day, each pages at most once; send-leg retry
  damps transient blips; market-calendar gate stops weekend/holiday noise.
- Telegram summary fails to send → the same data is in the CSV status line, the
  `cross_verify_1m_audit` table, and the structured `info!/error!` logs (5 sinks).
- join_api_url cannot fix a wholly-wrong base URL (e.g. typo host) → the captured
  final URL in the error log makes that diagnosable in one glance.
- New ErrorCode breaks meta-ratchets → `all()` list, count test (106 → 107),
  prefix test (`REST-CANARY-`), and runbook cross-ref file are all updated in the
  same commit.

## Test Plan

- `crates/common/src/sanitize.rs` — `test_capture_rest_error_body_redacts_jwt`
  (ratchet: no token substring may ever appear), `…_truncates_to_300_chars`,
  `…_utf8_boundary_safe`, `…_preserves_dhan_error_fields` (errorType/errorCode/
  errorMessage survive), `…_strips_control_chars`, `…_composes_url_param_redaction`,
  `test_redact_jwt_like_*`.
- `crates/common/src/url_join.rs` — `test_join_api_url_trailing_slash_base_no_double_slash`,
  `…_all_four_slash_combinations`, `…_preserves_scheme_double_slash`,
  `…_real_dhan_path_constants_never_double_slash` (uses DHAN_GET_IP_PATH etc.).
- `crates/common/src/error_code.rs` — existing meta-ratchets (count → 107, prefix,
  runbook-exists, crossref via new rule file).
- `crates/app/src/cross_verify_1m_boot.rs` — `test_classify_run_status_blind_when_zero_compared`,
  `…_blind_even_with_zero_fetch_failures`, `…_degraded_when_fraction_breached`,
  `…_pass_when_clean`, `test_csv_status_line_format`, CSV-assembly test (status
  line first, header second), `test_final_flush_skips_when_no_pending_rows`,
  `test_final_flush_errors_with_pending_count_when_disconnected`.
- `crates/app/src/rest_canary_boot.rs` — `test_canary_schedule_times_pinned`
  (09:05/12:00/15:25 IST), `test_next_probe_*` (before-all / between / after-all),
  `test_classify_canary_response_*` (2xx pass, 400/401/500 fail).
- `crates/core/src/auth/token_manager.rs` — mock-server test: profile 400 with a
  Dhan error body containing a planted JWT → error reason contains errorCode text
  and final URL, and NOT the JWT.
- `crates/core/src/notification/events.rs` — `test_cross_verify_1m_summary_*`
  (BLIND message says BLIND loudly + High severity; PASS is Info; counts render).
- Scope: `crates/common` touched → `cargo test --workspace` (escalation rule).

## Rollback

- Single squash-merge PR → `git revert <merge-sha>` restores prior behaviour
  entirely; no schema change (CSV gains a leading comment line only; the audit
  table is untouched), no config change, no new dependency.
- The canary is an additive background task; reverting removes it cleanly.
- `join_api_url` is behaviour-preserving for the checked-in (no-trailing-slash)
  config; reverting restores plain concatenation.

## Observability

- New `error!` sites all carry `code = ErrorCode::X.code_str()` (tag-guard):
  REST-CANARY-01 (new), CROSS-VERIFY-1M-01/02 (now with `sample_failure`, final
  URL, captured body, and exact pending counts).
- New counters: `tv_rest_canary_probes_total{outcome}`,
  `tv_cross_verify_1m_runs_total{status}`.
- Telegram: `CrossVerify1mSummary` typed event after every run (High on
  BLIND/DEGRADED, Info on PASS); canary failures page High via the error pipeline.
- CSV first line is the at-a-glance status artefact; the audit table remains the
  durable record; runbook `.claude/rules/project/dhan-rest-canary-error-codes.md`
  documents REST-CANARY-01 triage (satisfies the error-code cross-ref ratchet).

## Plan Items

- [x] Item 1 — body/URL capture helpers (crates/common/src/sanitize.rs::capture_rest_error_body + redact_jwt_like)
  - Files: crates/common/src/sanitize.rs
  - Tests: test_capture_rest_error_body_redacts_jwt, test_capture_rest_error_body_truncates_to_300_chars, test_redact_jwt_like_replaces_token
- [x] Item 2 — URL-join normalization (crates/common/src/url_join.rs::join_api_url)
  - Files: crates/common/src/url_join.rs, crates/common/src/lib.rs
  - Tests: test_join_api_url_trailing_slash_base_no_double_slash, test_join_api_url_real_dhan_path_constants_never_double_slash
- [x] Item 3 — REST-CANARY-01 error code + runbook (ErrorCode::RestCanary01ProbeFailed, count 106→107)
  - Files: crates/common/src/error_code.rs, .claude/rules/project/dhan-rest-canary-error-codes.md
  - Tests: test_all_list_length_matches_catalogue_size (107), test_code_str_follows_expected_prefix_pattern
- [x] Item 4 — token-manager + ip-verifier error paths capture body + URL, join via join_api_url
  - Files: crates/core/src/auth/token_manager.rs, crates/core/src/network/ip_verifier.rs
  - Tests: test_profile_400_error_includes_body_and_url_but_never_jwt
- [x] Item 5 — cross-verify fetch capture + status line + summary event + flush fix (RunStatus, csv_status_line, final_flush, NotificationEvent::CrossVerify1mSummary)
  - Files: crates/app/src/cross_verify_1m_boot.rs, crates/core/src/notification/events.rs, crates/app/src/main.rs
  - Tests: test_classify_run_status_blind_when_zero_compared, test_csv_status_line_format, test_final_flush_skips_when_no_pending_rows, test_cross_verify_1m_summary_blind_message_is_loud
- [x] Item 6 — REST-health canary (crates/app/src/rest_canary_boot.rs::run_rest_canary, spawned in spawn_post_market_tasks)
  - Files: crates/app/src/rest_canary_boot.rs, crates/app/src/lib.rs, crates/app/src/main.rs
  - Tests: test_canary_schedule_times_pinned, test_next_probe_sleep_secs_before_all_selects_0905, test_classify_canary_status_2xx_passes, test_probe_is_fresh_rejects_stale_and_midnight_wrap

## Per-Item Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md` — the 15-row "100%
everything" matrix and the 7-row resilience matrix from
`operator-charter-forever.md` §C apply to every item above and are
cross-referenced here rather than duplicated. Item specifics: all changes are
COLD-path (REST error paths, once-daily post-market run, 3 probes/day) — no
hot-path allocation is introduced (7-row "never slow" row); no tick-drop path is
touched (ring→spill→DLQ unchanged); no DEDUP key changes; every new pub fn has a
call site (boot wiring / shared call sites) + matching tests; every new `error!`
carries a `code =` field; the new ErrorCode has a runbook + crossref mention;
ratchet tests fail the build on regression of the JWT-redaction, the 300-char
bound, the no-double-slash join, the BLIND classification, and the
flush-skip-when-empty behaviour.

Honest 100% claim: 100% inside the tested envelope, with ratcheted regression
coverage — bounded ≤300-char secret-redacted body capture (ratcheted by the JWT
ratchet tests), no-double-slash URL join over all four slash combinations
(ratcheted), BLIND/DEGRADED/PASS classification over the boundary cases
(ratcheted), 3-probe canary schedule pinned by tests. Beyond the envelope, the
5-sink error pipeline still records every failure as recoverable text.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan 400 with error JSON body on profile | error! carries errorType/errorCode/errorMessage (≤300 chars) + final URL; JWT never appears |
| 2 | 776/776 intraday fetch failures | CSV first line `# status=BLIND …`; Telegram High "BLIND"; CROSS-VERIFY-1M-02 carries sample failure body+URL |
| 3 | Zero mismatches, QuestDB ILP never connected | NO flush error (skip on pending==0) |
| 4 | Mismatches buffered, ILP dead | error! with pending count + error chain |
| 5 | Base URL env-override gains trailing slash | join_api_url still produces single-slash URL; REST keeps working |
| 6 | REST dies at 08:45 | 09:05 canary pages High with status+body+URL — operator knows by 09:05, not 15:33 |
| 7 | Transient DNS blip at canary time | retry once after 30s; page only if still failing |
| 8 | Clean day | CSV `# status=PASS …`; Telegram Info summary |
