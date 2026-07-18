# Implementation Plan: PR #1562 Post-Merge Audit Follow-Ups

**Status:** APPROVED
**Date:** 2026-07-17
**Approved by:** coordinator dispatch 2026-07-17 (post-merge hostile audit of PR #1562's delta — finders + refuters; execution authorized by the coordinator session, not a fresh operator quote: every change is a mechanical fix or a dated doc truth-sync, no new semantics)

> **Per-item guarantee matrix:** cross-reference
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply to this PR's single item set; hot-path rows are N/A — no
> hot-path code is touched; the mark tap / DHAT / Criterion surfaces are
> comment-only edits).

## Design

The post-merge hostile audit of PR #1562's delta confirmed 9 findings.
This PR fixes every MECHANICAL finding and doc-flags every ARCHITECTURAL
one (no invented semantics), on branch `claude/pr1562-postmerge-followups`:

1. **HTTP-CLIENT-01 panic fallback (F3/F6, `crates/app`):**
   `oms_wiring.rs::build_oms_http_client()` ended in
   `.build().unwrap_or_default()` — `reqwest::Client::default()` ==
   `Client::new()` == PANIC under the exact fd/TLS/resolver pressure that
   makes the builder's Err arm reachable; with `panic = "abort"` a
   whole-process abort at every order-runtime (re)spawn. Fix: the builder
   returns `Result<reqwest::Client, HttpClientBuildError>` through the
   storage crate's `client_from_build_result` core (promoted `pub` in
   `crates/storage/src/http_client.rs` — one HTTP-CLIENT-01
   implementation, one proxy-credential redaction path), emits
   `error!(code = "HTTP-CLIENT-01", site = "oms_wiring")` +
   `tv_http_client_build_failed_total{site="oms_wiring"}` (static label).
   Callers degrade loudly: `order_runtime.rs::run_order_runtime` returns
   (the supervisor's escalating-backoff respawn retries);
   `trading_pipeline.rs` exits before OMS construction with its own coded
   consequence `error!`. New app-crate ratchet
   `crates/app/tests/http_client_fallback_guard.rs` mirrors the storage
   guard on the 3-file OMS wiring surface.
2. **engine.rs reconcile downward traded_qty copy (F1, `crates/trading`):
   DOCUMENT ONLY** — in-code ⚠ MANDATORY PRE-LIVE FOLLOW-UP comment at
   the copy site + a dated 2026-07-17 §3 entry in
   `order-runtime-dryrun.md` (no dry_run=false flip without a fix).
3. **Doc truth-syncs (F2/F4/F7/F8, dated 2026-07-17 in-place notes):**
   stale "per-tick"/"groww_bridge"/"~770-sid"/"next tick" mark-source
   claims → the per-minute REST reality (#1581 retired the live bridge;
   marks re-homed 2026-07-16) across order_runtime.rs comments,
   order-runtime-dryrun.md, benchmark-budgets.toml, order_gate.rs bench,
   dhat_mark_forward.rs, ci.yml, dhan_rest_stack.rs, config.rs; Phase
   5a→5b corrections in lib.rs/main.rs and the dhan-order-surface plan.
4. **groww_contract_1m_boot.rs doc/#[must_use] splice (F5):** restore
   `select_candle_row`'s doc + attribute; give `contract_segment_code`
   its own.
5. **Plan hygiene (F9):** tick A1–A3/A5–A8 (merged via #1562), annotate
   A4 + design item 1 as CUT 2026-07-14 (forbidden WAL/socket leg), and
   archive 2 completed plans (plan-enforcement rule 7 / design-first-wall
   V7 cap).

## Edge Cases

- A failed OMS client build at order-runtime spawn: the actor returns
  BEFORE OMS construction — no half-open paper book, no panic; the
  supervisor's clean_exit classification respawns with 5s→300s backoff,
  so a transient fd/TLS squeeze self-heals and a permanent one is a
  bounded loud loop (counter + coded error per attempt), never silent.
- The dormant trading_pipeline twin (Dhan-lane-gated, dead on prod's
  dhan-off boots): same builder, its own exit-before-OMS arm — a future
  dhan-on boot cannot resurrect the panic path.
- Guard scoping: test modules legitimately use `reqwest::Client::new()`
  (exit_execution.rs / trading_pipeline.rs / groww_contract_1m_boot.rs
  test regions) — the new guard scans PRODUCTION regions of exactly the
  3 wiring-surface files; `order_runtime.rs:1116`'s benign Vec
  `unwrap_or_default()` is outside the banned needle set (the
  oms_wiring-only `unwrap_or_default` ban covers the regression shape).
- `infra.rs::liveness_client` carries the SAME class of fallback
  (pre-existing, `|err|` binding form the storage needle misses) — NOT a
  confirmed finding; flagged in the runbook's 2026-07-17 note as a
  follow-up, deliberately not fixed here.

## Failure Modes

- Builder failure at boot vs at respawn: both routes emit the coded
  error + counter once per attempt; paging stays alarm-driven (no new
  alarm — HTTP-CLIENT-01 is log-sink + counter per its runbook).
- A regression re-introducing `unwrap_or_default`/`Client::new()` on the
  wiring surface fails the new build-failing ratchet (4 tests, incl. a
  stripper self-test so the scanner can never go vacuous).
- The engine.rs hazard stays live-mode-only (dry-run returns before the
  broker reconcile) — the comment + §3 entry make the pre-live gate
  mechanical for reviewers; no behavior change means no new failure mode
  is introduced in dry-run.

## Test Plan

- `cargo fmt --check` clean.
- `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf` clean.
- `cargo test -p tickvault-app` (new guard + existing order_runtime /
  wiring / dhat suites) green.
- `cargo test -p tickvault-trading` (engine.rs comment-only touch —
  reconcile + fill-delta suites unchanged) green.
- `cargo test -p tickvault-storage` (http_client.rs pub promotion — the
  storage guard + unit tests) green.
- `cargo test -p tickvault-common` (config.rs comment-only touch) green.
- `bash .claude/hooks/banned-pattern-scanner.sh` on the changed files.
- New tests: `http_client_fallback_guard.rs` — 
  no_panic_class_client_fallback_on_oms_wiring_surface,
  no_unwrap_or_default_in_oms_wiring,
  oms_wiring_keeps_typed_degrade_shape,
  code_portion_scheme_separator_does_not_hide_banned_pattern.

## Rollback

Single revert of the squash-merge commit restores the pre-fix state
(including the panic fallback — rolling back re-opens the abort-at-spawn
hazard, so a rollback should be paired with disabling `[order_runtime]`
if taken during a resource-pressure incident). Doc/plan edits are
annotation-only and carry their own dated provenance; reverting them
loses only the truth-sync. No schema, no config-default, no wire-format
change anywhere in this PR.

## Observability

- New static-label counter series value:
  `tv_http_client_build_failed_total{site="oms_wiring"}` (existing
  counter name, new site label — the runbook's §1 table gains the row).
- New coded emit sites: `error!(code = "HTTP-CLIENT-01",
  site = "oms_wiring")` in oms_wiring.rs + the trading_pipeline
  consequence line (tag-guard compliant: both carry
  `ErrorCode::HttpClient01BuildFailed.code_str()`).
- Ratchet: `crates/app/tests/http_client_fallback_guard.rs` fails the
  build on regression (the 100%-extreme-check row).
- No new ErrorCode variant, no new NotificationEvent, no new alarm —
  delivery boundary unchanged per `http-client-error-codes.md`.

## Plan Items

- [x] Fix F3/F6 — panic-free `build_oms_http_client` + both caller degrades + storage `client_from_build_result` pub promotion
  - Files: crates/app/src/oms_wiring.rs, crates/app/src/order_runtime.rs, crates/app/src/trading_pipeline.rs, crates/storage/src/http_client.rs
  - Tests: oms_wiring_keeps_typed_degrade_shape, no_panic_class_client_fallback_on_oms_wiring_surface, no_unwrap_or_default_in_oms_wiring, test_build_oms_http_client_constructs
- [x] Add app-crate HTTP-CLIENT-01 ratchet
  - Files: crates/app/tests/http_client_fallback_guard.rs
  - Tests: code_portion_scheme_separator_does_not_hide_banned_pattern (+ the 3 above)
- [x] Document F1 — engine.rs reconcile downward-copy pre-live hazard (comment + §3 rule entry)
  - Files: crates/trading/src/oms/engine.rs, .claude/rules/project/order-runtime-dryrun.md
  - Tests: N/A — comment/doc only (no semantic change by design)
- [x] Doc truth-syncs F2/F4/F7/F8 (per-tick→per-minute REST; Phase 5a→5b)
  - Files: crates/app/src/order_runtime.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/app/src/dhan_rest_stack.rs, crates/common/src/config.rs, quality/benchmark-budgets.toml, crates/app/benches/order_gate.rs, crates/app/tests/dhat_mark_forward.rs, .github/workflows/ci.yml, .claude/rules/project/order-runtime-dryrun.md
  - Tests: N/A — comments/docs only
- [x] Fix F5 — groww_contract_1m_boot.rs doc/#[must_use] splice
  - Files: crates/app/src/groww_contract_1m_boot.rs
  - Tests: existing suite (attribute restoration; clippy clean)
- [x] Runbook update — http-client-error-codes.md §1 site row + 2026-07-17 note + trigger paths
  - Files: .claude/rules/project/http-client-error-codes.md
  - Tests: N/A — rule file
- [x] Plan hygiene F9 — tick A1–A8 with CUT annotation on A4/design-item-1; archive 2 completed plans
  - Files: .claude/plans/active-plan-dhan-order-surface.md, .claude/plans/archive/2026-07-14-cadence-scheduler.md, .claude/plans/archive/2026-07-16-ci-gate-integrity.md
  - Tests: N/A — plan files

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Builder Err at order-runtime spawn (fd exhaustion) | coded error + counter, actor returns, supervisor backoff respawn — no panic, no abort |
| 2 | Builder Err at trading-pipeline spawn (dhan-on, future) | coded consequence error, pipeline exits before OMS; lane restart retries |
| 3 | Regression: `unwrap_or_default` re-added to oms_wiring.rs | `no_unwrap_or_default_in_oms_wiring` fails the build |
| 4 | Regression: `Client::new()` fallback added to a caller's production region | surface scan fails the build |
| 5 | Live-mode reconcile downward copy (future) | pre-live gate: §3 MANDATORY entry + in-code ⚠ block block the dry_run flip in review |
