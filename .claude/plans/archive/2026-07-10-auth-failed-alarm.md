# Implementation Plan: tv_api_auth_failed_total counter + 401-burst CloudWatch alarm

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Coordinator (operator-approved wave-2 #2 of 8; W2#1 = #1462 merged) — scope
verbatim: "add `tv_api_auth_failed_total` + a 401-burst CloudWatch alarm (ONE free-tier alarm
max)". Guarantee matrices: this plan cross-references
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) — filled per-item in the
PR body per `pr-completion-protocol.md`; the honest 100% claim carries the §F envelope
qualifier.

## Design

The bearer-auth middleware (`crates/api/src/middleware.rs::require_bearer_auth`, GAP-SEC-01)
has exactly THREE rejection arms (invalid token / malformed Authorization header / missing
Authorization header), each already logging a `warn!` with sanitized `path` + `peer` fields
(BUG-3, 2026-07-05). Today a brute-force credential sweep against the Tailscale-funnelled
port 3001 produces only warn-level log lines — no counter, no alarm, no page.

This PR adds the counter + the paging chain, mirroring the established house pattern
(`deploy/aws/terraform/feed-stall-restart-alarm.tf` — full-extraction variant — and
`seal-drop-alarm.tf`):

1. **Counter (crates/api):** a SINGLE UNLABELED `metrics::counter!("tv_api_auth_failed_total")
   .increment(1)` in each of the 3 rejection arms of `require_bearer_auth`. Unlabeled per the
   coordinator's explicit guidance ("bounded cardinality beats granularity"): the failing
   route is not cheaply classifiable into a bounded static set here (bearer-gated routes are
   the mutating surface, e.g. `POST /api/feeds/{feed}`), and per-reason/per-endpoint forensic
   attribution ALREADY exists in the pre-existing warn! lines (path + peer + arm-specific
   message). One series = one pre-registration = one derived metric = the stated cost line.
   NO new warn!/error! per 401 (log-amplification defence — same reasoning as the #1458 429
   handling); the 3 pre-existing warn!s are KEPT byte-identical (they are pinned by the BUG-3
   source-scan ratchet `test_auth_failure_warns_carry_path_and_peer_fields`).
2. **First-sample baseline pre-registration (crates/api):** `tv_api_auth_failed_total` is
   pre-registered at 0 in `crates/api/src/lib.rs::build_router_with_auth` — router
   construction, the single choke point BOTH boot paths (fast crash-recovery arm +
   `start_dhan_lane`) call. (Relocated 2026-07-10 from the originally-planned
   `crates/app/src/main.rs` site so the diff carries NO main.rs edit — coordinator
   ship-path directive; the guarantee is preserved and strengthened.) Rationale (the
   feed-stall round-5 lesson): the CW agent's prometheus pipeline drops each counter
   series' FIRST sample as its delta baseline; a 401-only counter is lazily born AT the
   first rejection, so the session's first burst sample would be eaten and the effective
   alarm threshold silently raised. Registering at router build is provably BEFORE the
   first possible 401 (the server cannot serve a request before its router exists) and
   AFTER the boot Step-3 recorder install (main.rs calls `observability::init_metrics`
   before either API-server spawn — pinned by the guard's read-only main.rs source-order
   scan), making the series DENSE from API-server start (0-delta per 60s scrape).
3. **Terraform (deploy/aws/terraform/auth-failed-alarm.tf, NEW):** the metrics log group
   delta-extraction route — `aws_cloudwatch_log_metric_filter` on
   `/tickvault/${var.environment}/metrics` with pattern
   `{ $.tv_api_auth_failed_total = * }`, metric_transformation reusing the raw name
   (full extraction, feed-stall precedent — NOT a slice, so no distinct derived name needed;
   header documents the future-EMF-allowlisting double-count caveat), namespace
   `Tickvault/Prod`, `value = "$.tv_api_auth_failed_total"`, `dimensions { host = "$.host" }`,
   no default_value. `aws_cloudwatch_metric_alarm` `tv-${var.environment}-api-auth-failed`:
   Sum >= 25 per ONE aligned 300s window, evaluation_periods=1,
   `treat_missing_data = "notBreaching"`, `dimensions = local.app_dimensions`,
   `alarm_actions = local.app_alarm_actions`, `ok_actions = local.app_alarm_ok`.
   **Threshold justification (funnel exposure):** a fat-fingered token paste on the /feeds
   page produces one 401 per manual toggle click — even frantic operator retrying stays in
   single digits per 5 minutes; a credential brute-force against the public funnel needs
   sustained request volume (hundreds+/min), so Sum >= 25/5min separates the two regimes with
   wide margins on both sides (inside the coordinator's 20-50 band). Always armed (no
   market-hours gate — the funnel is public 24/7 and probes are equally meaningful off-hours;
   dense 0-deltas keep the metric non-breaching when idle).
4. **Wiring guard (crates/app/tests/auth_failed_alarm_wiring_guard.rs, NEW):** lockstep
   source-scan ratchet per the `seal_drop_paging_wiring_guard.rs` house pattern — pins (a) the
   api-crate router-build pre-registration site + a READ-ONLY main.rs source-order scan
   (init_metrics precedes every build_router_with_auth call), (b) the 3 middleware emit sites, (c) the tf
   filter/metric/alarm shape built from the real metric literal, with comment strippers
   (Rust line comments + string-aware HCL comments) + anti-vacuity self-test.
5. **Docs:** dated 2026-07-10 append-only note in
   `.claude/rules/project/gap-enforcement.md` under "GAP-SEC-01: API Auth Middleware" (the
   bearer-auth rule home).

Cost: +1 alarm + 1 derived metric (single series) ~= $0.40/mo — inside the $35/mo pre-GST
budget ceiling. No new dependency (`metrics` is already a direct dep of tickvault-api via
#1458). No hot-path code (auth rejection is a cold error path). O(1): one atomic counter
increment per 401.

## Edge Cases

- **First 401 of a session:** without pre-registration the delta pipeline would eat it as the
  series baseline — closed by the router-build registration (item 2 above).
- **Pre-install boot-window 401:** a request rejected before the recorder install would
  increment a no-op handle — structurally impossible here: the registration runs at router
  construction and the server cannot serve a request before its router exists, while the
  recorder installs (main.rs Step 3) before either API-server spawn; honest-envelope
  documented in the tf header regardless.
- **Auth disabled (`config.enabled == false`):** the middleware passes through BEFORE any
  rejection arm — counter never increments, alarm silent. Correct: no auth surface, no auth
  failures.
- **Aligned-window straddle:** a burst split 24+24 across a 300s boundary never reaches 25 in
  either aligned window — honest residual documented in the tf header (span semantics per the
  feed-stall round-10 correction); a SUSTAINED sweep always lands >=25 in some window.
- **Boot settling OK page:** new-alarm INSUFFICIENT_DATA -> OK creation settling fires one
  benign OK page on apply evening (documented, house precedent).
- **Cumulative-vs-delta shape risk:** if the CW agent field ever proved cumulative, Sum
  over-pages (fail-loud, never a silent miss) — same stated residual as every sibling alarm.

## Failure Modes

- **Counter emit removed/renamed later:** wiring guard test (b) fails the build.
- **Pre-registration ordering broken (VOID no-op class):** wiring guard test (a) pins the
  registration inside `build_router_with_auth` (api crate) AND asserts, via a read-only
  main.rs source-order scan, that `init_metrics(` precedes every `build_router_with_auth(`
  call site.
- **Terraform pattern drifts from the Rust metric name:** wiring guard test (c) builds the
  filter needle from the same literal — a rename breaks the test, never silently blinds the
  filter.
- **Log-amplification regression (someone adds a per-401 warn!/error! beyond the 3 ratcheted
  BUG-3 warns):** out of scope to mechanically pin here; the existing tag-guard +
  BUG-3 ratchet keep the current 3 warn!s stable, and this PR adds NO new log lines.
- **Metrics log group shipping degraded (the 2026-07-06 collect_list class):** this alarm goes
  blind with it — honest envelope; the warn! lines in `/tickvault/<env>/app` remain the
  forensic fallback. notBreaching means no false page during the outage.

## Test Plan

- `cargo test -p tickvault-api` — full lib+integration suite (341+ tests); the middleware unit
  tests (401 arms, constant-time compare, peer sanitizer) must stay green byte-identical.
- `cargo test -p tickvault-app --test auth_failed_alarm_wiring_guard` — the 4 new ratchet
  tests + HCL-stripper self-test.
- `cargo clippy -p tickvault-api -p tickvault-app --no-deps -- -D warnings -W clippy::perf`,
  `cargo fmt --check`.
- Hook battery run manually against the worktree (gates auto-resolve `.cwd` to the main
  checkout): banned-pattern-scanner, secret scan, data-integrity, pub-fn guards (no new pub
  fn), plan-gate.
- Synchronous adversarial pass (security-reviewer + hostile worker) on the diff before PR.

## Rollback

Single revert of the one squash-merge commit restores: middleware without the 3 increments,
the api router constructor without the pre-registration, no tf file, no guard test, docs
note removed. No schema,
no data, no config migration — the CloudWatch filter+alarm disappear on the next terraform
apply after revert. Zero coupling into the tick path, OMS, or feeds.

## Observability

- NEW counter `tv_api_auth_failed_total` (unlabeled; one increment per bearer-auth 401).
- NEW CW log metric filter `tv-<env>-api-auth-failed-fallback` on
  `/tickvault/<env>/metrics` -> metric `tv_api_auth_failed_total` [host], namespace
  Tickvault/Prod.
- NEW CW alarm `tv-<env>-api-auth-failed` (Sum >= 25 / 300s, notBreaching) -> SNS
  `tv_alerts` -> Telegram.
- Existing (unchanged): per-401 `warn!` with path+peer (BUG-3), `tv_api_rate_limited_total`
  (429 surface, #1458). No new error!/ErrorCode (no error-level emit site is added, so the
  errcode cross-ref/tag guards are untouched).

## Plan Items

- [x] Item 1 — increment `tv_api_auth_failed_total` in the 3 rejection arms of
  `require_bearer_auth` (no new log lines, no new pub fn)
  - Files: crates/api/src/middleware.rs
  - Tests: test_auth_failed_counter_emit_sites_present_in_all_three_rejection_arms (guard)
- [x] Item 2 — pre-register the counter at 0 at router construction, post-recorder-install
  (first-sample baseline; relocated 2026-07-10 from main.rs to the api crate)
  - Files: crates/api/src/lib.rs
  - Tests: test_auth_failed_counter_is_preregistered_after_recorder_install (guard)
- [x] Item 3 — CloudWatch log metric filter + 401-burst alarm (Sum >= 25 / 300s)
  - Files: deploy/aws/terraform/auth-failed-alarm.tf
  - Tests: test_auth_failed_alarm_filter_matches_emitted_series_shape (guard)
- [x] Item 4 — lockstep wiring guard ratchet (source-order + emit-site + tf shape +
  stripper self-test)
  - Files: crates/app/tests/auth_failed_alarm_wiring_guard.rs
  - Tests: test_strip_hcl_comments_keeps_strings_drops_comments,
    test_auth_failed_counter_is_preregistered_after_recorder_install,
    test_auth_failed_counter_emit_sites_present_in_all_three_rejection_arms,
    test_auth_failed_alarm_filter_matches_emitted_series_shape
- [x] Item 5 — dated 2026-07-10 append-only note in the GAP-SEC-01 rule section
  - Files: .claude/rules/project/gap-enforcement.md
  - Tests: N/A — docs-only append (rule-file note; no code contract)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Single fat-fingered token paste (a few 401s in 5 min) | Counter increments; alarm stays OK (Sum < 25) |
| 2 | Sustained brute-force sweep on the funnel | Sum >= 25 in some aligned 300s window -> ALARM -> SNS -> Telegram within ~5-10 min |
| 3 | First 401 immediately after boot | Counted (series pre-registered dense from boot; baseline sample was the harmless 0) |
| 4 | Auth disabled (dry-run tokenless dev without token) | Pass-through; counter never increments |
| 5 | Box off / app down | Metric missing -> notBreaching -> no false page |
| 6 | Rename of the counter in Rust only | Wiring guard test (c) fails the build (tf needle built from the same literal) |
