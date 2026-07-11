# Implementation Plan: C2 — boot-probe shared HTTP client, no panic fallback

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — C2 task directive, this session

## Plan Items

- [x] Item 1 — Shared panic-free HTTP client module in `crates/storage`
  - Files: crates/storage/src/http_client.rs, crates/storage/src/lib.rs
  - Tests: test_client_from_build_result_maps_error_without_panic,
    test_client_from_build_result_ok_passthrough,
    test_shared_probe_client_reuses_same_instance,
    test_build_probe_client_succeeds_with_timeout,
    test_error_display_carries_code_and_message_no_secret_echo

- [x] Item 2 — boot_probe.rs uses the shared client + typed ClientBuild error
  - Files: crates/storage/src/boot_probe.rs
  - Tests: test_wait_for_questdb_ready_fails_on_unreachable_host,
    test_wait_for_questdb_ready_unreachable_returns_deadline_not_panic_with_shared_client

- [x] Item 3 — 7 remaining `unwrap_or_else(|_| Client::new())` sites in
  `crates/storage/src` replaced with match + error!(code) + counter + degrade
  - Files: crates/storage/src/instrument_fetch_audit_persistence.rs,
    crates/storage/src/shadow_persistence.rs,
    crates/storage/src/instrument_lifecycle_persistence.rs,
    crates/storage/src/tick_persistence.rs
  - Tests: no_client_new_fallback_in_storage_src (ratchet)

- [x] Item 4 — ErrorCode::HttpClient01BuildFailed ("HTTP-CLIENT-01", High,
  auto-triage-safe) + rule file
  - Files: crates/common/src/error_code.rs,
    .claude/rules/project/http-client-error-codes.md
  - Tests: test_all_list_length_matches_catalogue_size,
    test_code_str_follows_expected_prefix_pattern,
    every_error_code_variant_appears_in_a_rule_file,
    every_runbook_path_exists_on_disk

- [x] Item 5 — Source-scan ratchet guard
  - Files: crates/storage/tests/http_client_fallback_guard.rs
  - Tests: no_client_new_fallback_in_storage_src,
    no_unwrap_or_else_client_fallback,
    boot_probe_uses_shared_client,
    no_unwrap_or_else_client_fallback_in_core_or_app

- [x] Item 6 — Review Fix 1 (security MEDIUM): userinfo redaction in
  HttpClientBuildError — reqwest builder errors can embed HTTPS_PROXY URLs
  carrying Basic-Auth userinfo; `redact_userinfo` strips
  `scheme://user:pass@` → `scheme://***@`; doc comment now states the
  mechanical truth (Display string with userinfo redacted)
  - Files: crates/storage/src/http_client.rs
  - Tests: test_redact_userinfo_strips_basic_auth,
    test_redact_userinfo_passthrough_no_userinfo,
    test_redact_userinfo_password_containing_at_sign,
    test_error_message_is_redacted

- [x] Item 7 — Review Fix 2 (hostile MEDIUM): ratchet bypass holes closed —
  `Client::default()` banned (Default delegates to new(), panics
  identically), `use reqwest::Client as ` alias banned (rename blinds the
  needle scan), comment stripper no longer treats `://` (URL scheme
  separator in string literals) as a comment start, with a stripper
  self-test
  - Files: crates/storage/tests/http_client_fallback_guard.rs
  - Tests: no_client_new_fallback_in_storage_src (extended),
    no_reqwest_client_alias_in_storage_src,
    code_portion_scheme_separator_does_not_hide_banned_pattern

- [x] Item 8 — Review Fix 3 (hostile MEDIUM): BOOT-02 misattribution — a
  `BootProbeError::ClientBuild` from `wait_for_questdb_ready` now logs
  HTTP-CLIENT-01 (host fd/TLS/resolver problem, not QuestDB) instead of
  BOOT-02 at both call sites; control flow byte-identical (same halt /
  return behaviour)
  - Files: crates/app/src/main.rs, crates/core/src/pipeline/tick_processor.rs
  - Tests: N/A — log-line selection only; pinned by the existing
    tag-guard (every_error_macro_tagged_with_a_known_code_carries_code_field)

- [x] Item 9 — Review Fix 4 (hostile MEDIUM, documented — no behaviour
  change): DDL-skip duplicate-row window named explicitly in each ensure
  site's error! message + Honest-envelope paragraph in the rule file
  (halt-vs-degrade flagged as an operator follow-up decision)
  - Files: crates/storage/src/tick_persistence.rs,
    crates/storage/src/shadow_persistence.rs,
    crates/storage/src/instrument_lifecycle_persistence.rs,
    crates/storage/src/instrument_fetch_audit_persistence.rs,
    .claude/rules/project/http-client-error-codes.md
  - Tests: N/A — message/doc wording only; every_runbook_path_exists_on_disk
    covers the rule file

## Design

Eight sites in `crates/storage` built reqwest clients with the fallback
`Client::builder()...build().unwrap_or_else(|_| Client::new())`.
`Client::new()` panics on exactly the conditions that make the builder fail
(TLS backend init, resolver init, fd exhaustion), so the fallback converts a
recoverable error into a silent tokio-task death — the suspected mechanism of
the 2026-07-03 10:35 IST SLO-publisher death during a 1.13M-frame storm,
because `boot_probe::wait_for_questdb_ready` builds a client PER INVOCATION
and is called every 10s (SLO scheduler, crates/app/src/main.rs:9265) and
every 5s (pool watchdog, crates/app/src/main.rs:3929).

Fix shape:
1. New module `crates/storage/src/http_client.rs` with a pure, testable core
   `client_from_build_result` (maps builder Err → typed
   `HttpClientBuildError`, never panics), `build_probe_client(timeout_secs)`,
   and `shared_probe_client()` — a `static OnceLock<Client>` built ONCE
   (before `get_or_init`, so failure propagates as a typed error; a racing
   double-build is harmless) and O(1) (single atomic load) afterwards.
2. `boot_probe.rs` consumes `shared_probe_client()`; on Err it logs
   `error!(code = "HTTP-CLIENT-01")`, increments
   `tv_http_client_build_failed_total{site="boot_probe"}`, and returns the
   new typed `BootProbeError::ClientBuild(...)` variant. The client is built
   once per process instead of ~8,640 times per trading session.
3. The other 7 sites are cold-path/setup fns returning `()`; they keep their
   per-call builds and their exact timeouts, but the fallback becomes
   `match ... { Ok(c) => c, Err(e) => { error!(code=...); counter; return; } }`
   — the same degrade shape already used in
   `prev_day_ohlcv_persistence.rs:145` (the crate's existing precedent).
4. New `ErrorCode::HttpClient01BuildFailed` (High, runbook
   `.claude/rules/project/http-client-error-codes.md`, auto-triage-safe) with
   the `HTTP-CLIENT-` prefix registered in the prefix-pattern test.
5. Ratchet `crates/storage/tests/http_client_fallback_guard.rs` bans
   `Client::new()` and `unwrap_or_else(|_| Client` from storage src (comments
   stripped), pins `shared_probe_client` in boot_probe.rs, and extends the
   `unwrap_or_else(|_| Client` scan to crates/core + crates/app src.

## Edge Cases

- First `shared_probe_client()` call fails (fd exhaustion window): the
  `OnceLock` stays uninitialized, the typed error is returned, and the NEXT
  probe tick retries the build — a transient window never poisons the process.
- Two tasks race the first build: both build; `get_or_init` keeps exactly one
  client, the loser's client is dropped. Both callers get `Ok`.
- Builder error message content: `HttpClientBuildError` captures the
  builder's Display string with URL userinfo redacted (`redact_userinfo` —
  HTTPS_PROXY Basic-Auth credentials can never leak); it never carries
  request bodies or our own tokens.
- The 7 degrade sites are all idempotent (DDL `IF NOT EXISTS` / marker-gated
  cleanup / best-effort check), so skipping one invocation is safe and the
  next boot or next tick re-runs it.
- The existing exhaustive `match` on `BootProbeError` in boot_probe's unit
  test gains an explicit `ClientBuild` arm; external callers
  (tick_processor.rs, main.rs, app tests) use `is_err()`/`Err(e)` patterns and
  need no change (verified by workspace check).

## Failure Modes

- Client build fails on the shared path → `BootProbeError::ClientBuild`
  propagates to the probe caller: the SLO scheduler's `timeout(...).is_ok_and(
  |res| res.is_ok())` treats it as qdb-unhealthy (honest degrade, no panic);
  the pool watchdog's identical pattern likewise; boot paths log BOOT-02-class
  aborts as before. NO caller can panic on it.
- Client build fails on a DDL/ensure site → the fn logs
  `error!(code = "HTTP-CLIENT-01")` (Telegram-routable), bumps the counter
  with a STATIC `site` label, and returns — never a silent continue, never a
  panic. Schema self-heal on the next boot covers the skipped DDL.
- Rule-file drift → `every_error_code_variant_appears_in_a_rule_file` +
  `every_runbook_path_exists_on_disk` fail the build.
- Pattern regression → the ratchet guard fails the build.

## Test Plan

- Unit (crates/storage/src/http_client.rs): pure-core Err mapping (no panic),
  Ok passthrough, shared-instance pointer equality, normal-env build success,
  Display/Error-trait no-secret-echo.
- Unit (boot_probe.rs): unreachable host still returns DeadlineExceeded (not
  panic) with the shared client, both via the existing test (extended match)
  and a new shared-client-prewarmed test.
- Ratchet (crates/storage/tests/http_client_fallback_guard.rs): 4 source-scan
  tests as listed in Item 5.
- ErrorCode invariants (crates/common): all() length 118, prefix pattern,
  roundtrip, runbook-on-disk, crossref forward check.
- Scope per testing-scope.md: `cargo test -p tickvault-storage` +
  `cargo test -p tickvault-common` (common changed → escalate) +
  `cargo check --workspace` for downstream breakage.

## Rollback

Single revert of this change-set restores the previous behaviour (the panic
fallback), since no schema, config, or wire format changes: no QuestDB DDL
changed, no table/DEDUP key changed, no config key added. The new ErrorCode
variant and rule file are additive; reverting them together keeps the
crossref tests green. No data migration to undo.

## Observability

- New typed code `HTTP-CLIENT-01` (`ErrorCode::HttpClient01BuildFailed`,
  Severity::High) — every failure site emits `error!` WITH the `code` field
  (tag-guard compliant), routing through the 5-sink chain to Telegram.
- New counter `tv_http_client_build_failed_total{site}` with 8 STATIC site
  labels (boot_probe, instrument_fetch_audit_ensure, shadow_ensure_tables,
  shadow_drop_legacy, lifecycle_ensure, lifecycle_audit_ensure,
  ticks_ensure_dedup, tick_gap_check) — zero allocation at emission.
- Runbook `.claude/rules/project/http-client-error-codes.md` with triage
  (counter → `/proc/<pid>/fd` → RESOURCE-01 cross-check) and the honest
  envelope (degrades the single probe/write, never the process).
- No hot-path change: all touched sites are boot/probe/cold-path; the shared
  client REDUCES steady-state work (one TLS/resolver init per process instead
  of per 5s/10s tick).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | fd exhaustion during a frame storm hits the 10s SLO probe | `shared_probe_client` (already built at boot) is reused — no build, no panic; probe runs or times out honestly |
| 2 | fd exhaustion at the very FIRST probe build | typed `BootProbeError::ClientBuild` + HTTP-CLIENT-01 error! + counter; next tick retries the build |
| 3 | TLS backend init failure at a boot DDL site | error! + counter + DDL skipped; next boot re-runs (idempotent) — no panic, no silent skip |
| 4 | Normal operation | one client build per process for probes; per-call builds at cold DDL sites succeed as today; zero behaviour change |
| 5 | Someone re-adds `unwrap_or_else(\|_\| Client::new())` | ratchet guard fails the build |
| 6 | Someone removes `shared_probe_client` from boot_probe | ratchet guard fails the build |

## Per-Item Guarantee Matrix

Per-item guarantee matrix: cross-reference
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices
apply as written). Item-specific one-liners:

### 15-row 100% Guarantee Matrix (item specifics)

| Demand | This item's proof |
|---|---|
| 100% code coverage | 5 unit tests on http_client.rs incl. the pure Err-mapping core; boot_probe tests extended |
| 100% audit coverage | N/A — no new typed event class needing a QuestDB audit table; failures route via error!+counter (cold-path client build) |
| 100% testing coverage | unit + integration ratchet (source-scan) categories; scoped `cargo test -p tickvault-storage` + `-p tickvault-common` |
| 100% code checks | banned-pattern scanner + fmt + clippy -D warnings run in Step 6; new ratchet added |
| 100% code performance | no hot-path change; shared client strictly reduces per-tick-scheduler work (OnceLock atomic load) |
| 100% monitoring | `tv_http_client_build_failed_total{site}` counter (static labels) |
| 100% logging | every failure site: `error!` with `code = ErrorCode::HttpClient01BuildFailed.code_str()` |
| 100% alerting | Severity::High routes Telegram+SNS via the 5-sink error chain; CloudWatch alarm follow-up noted in report |
| 100% security | `HttpClientBuildError` carries the builder Display string with URL userinfo redacted (`redact_userinfo` — HTTPS_PROXY Basic-Auth can never leak); redaction + Display tests pin no-echo |
| 100% security hardening | removes a panic-class DoS amplifier (fd exhaustion → task death) |
| 100% bugs fixing | fixes the C2 audit finding (8 panic fallbacks) at the root |
| 100% scenarios covering | 6-row Scenarios table above incl. first-build failure + race |
| 100% functionalities covering | every new pub fn (build_probe_client, shared_probe_client, message) has tests + call sites |
| 100% code review | worker-session review + operator review on the diff |
| 100% extreme check | `http_client_fallback_guard.rs` ratchet fails the build on regression |

### 7-row Resilience Demand Matrix (item specifics)

| Demand | This item's proof |
|---|---|
| Zero ticks lost | no tick-drop path touched; DDL degrade leaves ring/spill/DLQ absorbing as designed |
| WS never disconnects | WS code untouched; probe death no longer kills the SLO/watchdog tasks that feed reconnect decisions |
| Never slow/locked/hanged | probe timeout unchanged (2s); shared client removes per-tick TLS/resolver init |
| QuestDB never fails | ensure-DDL degrade is idempotent; schema self-heal on next boot preserved |
| O(1) latency | shared_probe_client is O(1) after first call (single atomic load); no hot-path allocation added |
| Uniqueness + dedup | no DEDUP key or table change |
| Real-time proof | HTTP-CLIENT-01 error! + counter fire at the instant of failure (previously: silence) |

Honest 100% claim: 100% inside the tested envelope, with ratcheted regression
coverage — the typed HTTP-CLIENT-01 degrade covers every reqwest client-build
failure path in `crates/storage/src` (pinned by
`http_client_fallback_guard.rs`); it does not prevent the underlying fd/TLS
exhaustion (RESOURCE-01/02 monitors cover that); beyond the envelope, the
error!+counter chain records every occurrence as a recoverable signal.
