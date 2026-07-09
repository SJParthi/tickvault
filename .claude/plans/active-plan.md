# Implementation Plan: Fast-Boot Cached-Token Validation (AUTH-GAP-06)

**Status:** VERIFIED
**Date:** 2026-07-08
**Approved by:** Parthiban (operator directive 2026-07-08 — third morning cached-token outage, 08:32–09:06 IST on 2026-07-07)
**Changed crates:** core (`crates/core`), app (`crates/app`), common (`crates/common`)
**Guarantee matrices:** cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row) — this item's specifics are in the PR body's Zero-Loss charter §3 block.

> **Incident context (2026-07-07, third occurrence):** the FAST crash-recovery
> boot arm (`crates/app/src/main.rs`) trusted yesterday's CACHED Dhan token
> blindly — Dhan had killed it (one active token at a time), so the feed sat
> dead from 08:32 to 09:06 IST until manual intervention. The fast arm spawns
> the WebSocket pool from the cached token with ZERO validation; the
> mid-session watchdog (AUTH-GAP-05) only self-heals ~30 minutes into the
> session. The fix: ONE `GET /v2/profile` validation of the cached token
> BEFORE any WebSocket spawn; a prefix-anchored 401/403 rejection forces a
> re-mint through the EXISTING TokenManager machinery.

## Plan Items

- [x] Item 1 — New `ErrorCode::AuthGap06FastBootCachedTokenValidation` (`AUTH-GAP-06`, Severity::High, runbook `wave-4-error-codes.md`) + rule-file mention (crossref test enforces both directions).
  - Files: crates/common/src/error_code.rs, .claude/rules/project/wave-4-error-codes.md
  - Tests: test_all_variants_have_unique_code_str, test_code_str_roundtrip_via_from_str, every_error_code_variant_appears_in_a_rule_file, every_runbook_path_exists_on_disk, test_all_list_length_matches_catalogue_size

- [x] Item 2 — Pure classifier in `crates/core/src/auth/fast_boot_validation.rs`: `classify_probe_result(&Result<(), ApplicationError>) -> CachedTokenVerdict { Proceed | Remint | ProceedDegraded }`. Prefix-anchored on the SHARED `get_user_profile` wrapper constants (PROFILE_HTTP_RESPONSE_WRAPPER / PROFILE_SEND_LEG_WRAPPER / PROFILE_PARSE_ERROR_WRAPPER from token_manager.rs — the AUTH-GAP-05 producer-owned literals); reuses `reason_core` + `http_response_status` from mid_session_watchdog (made pub(crate)); NEVER substring-scans server-controlled body text. 401/403 → Remint; every other failure shape (5xx/429/400, parse, send-leg, unknown) → ProceedDegraded (a mint on ambiguity can destroy a healthy token — the F12 lesson).
  - Files: crates/core/src/auth/fast_boot_validation.rs, crates/core/src/auth/mid_session_watchdog.rs, crates/core/src/auth/mod.rs
  - Tests: classify_probe_result_ok_is_proceed, classify_probe_result_http_401_is_remint, classify_probe_result_http_403_is_remint, classify_probe_result_http_5xx_is_proceed_degraded, classify_probe_result_http_400_and_429_are_proceed_degraded, classify_probe_result_send_leg_is_proceed_degraded, classify_probe_result_parse_error_is_proceed_degraded, classify_probe_result_unknown_shape_is_proceed_degraded, classify_probe_result_hostile_401_body_cannot_forge_degraded, reason_core_strips_optional_display_prefix

- [x] Item 3 — Orchestrator `validate_cached_token_at_fast_boot(...) -> Option<Arc<TokenManager>>` in the same module: builds a redirect-none bounded reqwest client, probes `/v2/profile` ONCE with the cached token; Remint → construct TokenManager via existing `initialize_deferred` (bounded by TOKEN_INIT_TIMEOUT_SECS; passes `None` for the dual-instance lock flag per `dual-instance-lock-2026-07-04.md` §3 — documented residual, UNCHANGED) then `force_renewal()` (RESILIENCE-03 in-flight tripwire + Dhan mint semantics preserved); ProceedDegraded → ONE retry after `FAST_BOOT_PROFILE_RETRY_DELAY_SECS`, then loud `error!(code = AUTH-GAP-06)` and PROCEED with the cached token (fail-open — boot never hangs; a failed remint also falls through loudly to existing fast-arm semantics). Counter `tv_fast_boot_token_validation_total{outcome}` (static labels).
  - Files: crates/core/src/auth/fast_boot_validation.rs
  - Tests: retry_delay_constant_is_short, ratchet_validation_helper_calls_profile_endpoint_and_remint_machinery (the network-bound orchestrator + probe carry TEST-EXEMPT justifications in-source; the decision surface is the Item 2 unit-test truth table)

- [x] Item 4 — main.rs fast-arm wiring: cached-token load → `validate_cached_token_at_fast_boot(...)` → `wait_out_persisted_ws_rate_limit_cooldown()` → `create_websocket_pool(...)`. Exactly ONE validation call per boot. The deferred-auth background arm REUSES the validation-built TokenManager when the remint path constructed one (no duplicate SSM fetch / duplicate manager).
  - Files: crates/app/src/main.rs
  - Tests: ratchet_fast_boot_validation_precedes_pool_create_and_cooldown (Item 5)

- [x] Item 5 — Non-vacuous ratchets in `crates/app/tests/fast_boot_token_validation_wiring_guard.rs`: (a) comment-stripped, production-region (pre-`#[cfg(test)]`) source-order scan pinning exactly one `validate_cached_token_at_fast_boot(` call in main.rs that precedes BOTH the first cooldown wait and the first `match create_websocket_pool(`; (b) stub-guard that the core helper module's production region actually calls the profile endpoint (`DHAN_USER_PROFILE_PATH`) and the existing remint machinery (`force_renewal(`); (c) stripper self-test (`://` is not a comment).
  - Files: crates/app/tests/fast_boot_token_validation_wiring_guard.rs
  - Tests: ratchet_fast_boot_validation_precedes_pool_create_and_cooldown, ratchet_validation_helper_calls_profile_endpoint_and_remint_machinery, stripper_self_test_url_scheme_is_not_a_comment

## Design

The fast crash-recovery arm currently goes cached-token → WS pool with no
validation; the TokenManager is only constructed later in a background
`tokio::join!` arm. The fix inserts a validation step between the
instrument-load block and the WS-GAP-08 cooldown wait:

1. **Probe** — one `GET /v2/profile` with the cached JWT via a dedicated
   redirect-none, request-timeout-bounded reqwest client (no SSM on the
   happy path — crash-recovery latency preserved; the probe produces the
   SAME failure-reason wrapper strings as `get_user_profile` so one
   classifier covers both).
2. **Classify (pure)** — `CachedTokenVerdict`: 2xx → Proceed; prefix-anchored
   HTTP 401/403 → Remint (the broker rejected OUR login — the 2026-07-07
   incident shape); everything else → ProceedDegraded (send-leg/network,
   REST-surface 400/429/5xx, parse, unknown — none is evidence the TOKEN is
   bad; minting on ambiguity destroys a possibly-healthy token and risks the
   cross-host mint ping-pong).
3. **Act** — Remint: build the TokenManager with the EXISTING
   `initialize_deferred` (SSM fetch, client_id check) and call
   `force_renewal()` (RenewToken → generateAccessToken fallback; the
   RESILIENCE-03 in-flight tripwire stays; the fast arm passes `None` for
   the lock flag — the §3 documented residual, deliberately unchanged).
   The renewed token lands in the SHARED TokenHandle the WS pool reads.
   ProceedDegraded: retry the probe once after a short named-constant bound,
   then proceed with the cached token + loud AUTH-GAP-06 `error!`.
4. **Reuse** — if the remint path built a TokenManager, the later deferred
   background arm reuses it instead of a second `initialize_deferred`.

## Edge Cases

- Cached token empty/absent at probe time (cannot happen on the fast arm by
  construction — the arm is gated on a valid cache): degrade loudly, proceed.
- HTTP client build failure (fd/TLS pressure): degrade loudly, proceed —
  never a panic, never a block (HTTP-CLIENT-01 lesson).
- Hostile 401 body embedding transient needles / wrapper literals: the
  status is parsed from the fixed position after OUR wrapper prefix
  (SEC-R1-3) — the body cannot forge a downgrade to ProceedDegraded, and a
  degraded shape cannot forge an escalation to Remint.
- 3xx: the shared client pins `redirect::Policy::none()` → ordinary non-2xx
  response → ProceedDegraded (never a send-leg transient downgrade).
- Transient blip on probe 1, clean on retry → Proceed (no noise beyond a
  warn!). Transient then 401 on retry → Remint.
- Remint succeeds but Dhan is still propagating: WS connect uses the fresh
  token; existing reconnect machinery covers any residual failure.

## Failure Modes

- `initialize_deferred` fails or exceeds TOKEN_INIT_TIMEOUT_SECS → loud
  AUTH-GAP-06 `error!` (outcome=remint_unavailable), proceed with the cached
  token; the background arm retries `initialize_deferred` exactly as before.
- `force_renewal()` fails (network, RESILIENCE-03 in-flight refusal, Dhan
  reject) → loud AUTH-GAP-06 `error!` (outcome=remint_failed), proceed —
  fast-arm failure semantics unchanged (WS will fail-and-reconnect; the
  AUTH-GAP-05 watchdog remains the mid-session self-heal backstop). Boot is
  NEVER hung: every leg is timeout-bounded.
- Both probes degraded (REST surface down, WS possibly healthy) → proceed
  with cached token + AUTH-GAP-06 error! — exactly the 2026-06-10 incident
  class (REST dead, WS healthy) must NOT block boot or trigger a mint.

## Test Plan

- Unit truth table for `classify_probe_result` — every verdict branch, incl.
  hostile-body forgery attempts (Item 2 test list).
- `cargo test -p tickvault-core --lib auth` + `cargo test -p tickvault-app`
  (ratchet file) + `cargo test -p tickvault-common --lib error_code`.
- Ratchets: production-region source-order scan (validation precedes cooldown
  wait and pool create; exactly one call), helper stub-guard
  (DHAN_USER_PROFILE_PATH + force_renewal referenced in code, not comments),
  stripper self-test.
- fmt + clippy -D warnings on core+app.

## Rollback

Single revert of this commit restores the prior fast arm byte-identically:
the wiring is one call site in main.rs plus one Option reuse in the deferred
arm; the new module is additive; the ErrorCode variant is additive (removing
it requires deleting the rule-file section in the same revert). No schema,
no config, no hot-path change — rollback is `git revert` with no data
migration.

## Observability

- `error!` / `warn!` / `info!` with `code = ErrorCode::AuthGap06FastBootCachedTokenValidation.code_str()`
  on every non-Proceed branch (tag-guard satisfied); routes stdout → app.log
  → errors.log → errors.jsonl → Telegram chain.
- Counter `tv_fast_boot_token_validation_total{outcome}` with static labels
  (`proceed` / `degraded` / `remint_triggered` / `remint_ok` /
  `remint_failed` / `remint_unavailable`).
- Runbook: `.claude/rules/project/wave-4-error-codes.md` §AUTH-GAP-06 (dated
  2026-07-08) — triage steps + honest envelope.
- Cold path only: one HTTP round-trip (+ optional retry) per fast boot;
  nothing touches the tick hot path.
