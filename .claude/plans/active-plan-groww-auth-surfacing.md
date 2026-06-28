# Implementation Plan: Groww Auth Failure Self-Diagnosis & Feed-Control Surfacing

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** Parthiban (operator task brief, 2026-06-28)

## Design

Today, when Groww rejects our api-key during the boot/activation auth smoke-check
(HTTP 400 "Token key not found or inactive"), the operator gets only a bare
`DOWN`/`DEGRADED` on the Feed Control page and the log hint prints a literal
`<env>` placeholder. The failure is not self-diagnosing — it does not name the
resolved SSM environment, the exact SSM paths read, or Groww's verbatim
(already-redacted) error.

This change makes the Groww auth failure self-diagnosing WITHOUT changing the
HTTP request (URL, Bearer scheme, `x-api-version`, body — verified correct vs the
official SDK). Five crates change:

1. **common** (`feed_health.rs`) — add an `auth_rejected: bool` lane signal. The
   verdict engine's `classify()` gains a HIGH-priority branch: an enabled feed
   with `auth_rejected` is `Down` with the fixed `&'static str`
   "auth rejected — refresh the Groww SSM api-key". The `FeedHealthRegistry`
   gains an `AtomicBool` per feed + a `set_auth_rejected(feed, bool)` setter; the
   `snapshot()` reads it into `FeedHealthInput`.

2. **core** (`feed/groww/auth.rs`) — `run_groww_auth_smoke_check` returns a typed
   `Result<(), GrowwAuthSmokeError>` whose `Err` distinguishes three outcomes:
   `Rejected { status, body }` (Groww returned a non-2xx — credential rejected),
   `Transport(String)` (could not reach Groww), `Credentials(String)` (SSM fetch
   failed). A pure classifier `classify_groww_auth_failure(status, body) ->
   GrowwAuthSmokeError` maps the HTTP status to `Rejected` (it is the body-carrying
   variant). The HTTP request is UNCHANGED. The api-key/totp NEVER enter any
   returned/logged string — only Groww's response body, already redacted by
   `capture_rest_error_body`.

3. **app** (`groww_activation.rs`) — the smoke-check failure block builds the hint
   with the ACTUAL resolved environment (`resolve_environment()`) + the ACTUAL
   SSM paths (`build_ssm_path` for api-key + totp-secret) instead of `<env>`. On a
   `Rejected` outcome it calls `feed_health.set_auth_rejected(Feed::Groww, true)`
   + appends the one-line operator hint (TOTP-token vs API-Key + Trading-API
   subscription). On success it clears the flag. Transport errors do NOT set the
   flag (don't blame the key for a network blip). This requires threading the
   `FeedHealthRegistry` into `run_groww_activation_watcher` → `activate_groww_lane`.

4. **app** (`groww_sidecar_supervisor.rs`) — replace its `<env>` placeholder in the
   SSM credential-fetch hint with the resolved env + actual path.

5. **api** (`handlers/feeds.rs`) — thread `auth_rejected` from the registry
   snapshot into the `FeedHealthInput` row; ALL `FeedHealthRow` string fields stay
   `&'static str` (the security contract). The verdict + reason already flow
   through; `auth_rejected` is read by `snapshot()` so no new public string field
   is needed on the API row.

Changed files:
- `crates/common/src/feed_health.rs`
- `crates/core/src/feed/groww/auth.rs`
- `crates/app/src/groww_activation.rs`
- `crates/app/src/groww_sidecar_supervisor.rs`
- `crates/app/src/main.rs` (pass `feed_health` Arc into the Groww activation watcher)
- `crates/api/src/handlers/feeds.rs` (read `auth_rejected` into the snapshot path — via registry)
- `crates/core/src/auth/secret_manager.rs` (one-time warn on env/path edge cases)

## Edge Cases

- `resolve_environment()` neither `TV_ENVIRONMENT` nor `ENVIRONMENT` set → one-time
  `warn!` naming `DEFAULT_SSM_ENVIRONMENT`; returned value UNCHANGED.
- `build_ssm_path()` called with empty env/service/secret → `warn!` (NOT
  debug_assert/panic — an existing test calls it with empty strings); same string
  returned.
- Groww transient network blip → `Transport` → feed stays whatever it was, NOT
  forced to auth-rejected (avoids a sticky false-red after recovery).
- Auth recovers on a later activation cycle → `set_auth_rejected(false)` clears the
  flag; verdict resolves back via the normal path.
- `auth_rejected=true` but feed later disabled → `Disabled` branch wins (highest
  priority in `classify`), so the page shows the intentional off-state, not red.
- `auth_rejected` precedence: placed AFTER `!enabled` (Disabled) and `!lane_running`
  (Degraded "not started at boot") and `!instrumented` (Unknown) so a feed that was
  never started doesn't show a misleading auth-rejected; placed BEFORE the
  drops/market-open/connected/last-tick checks so a confirmed rejection wins over
  generic "disconnected"/"no ticks".

## Failure Modes

- A secret value leaking into a log/error/page string → PREVENTED: api-key/totp
  are `Secret<String>`, never formatted; only `capture_rest_error_body` output
  (redacted) and the SSM PATHS (never values) appear.
- A `String` reason widening the `&'static str` security contract on
  `FeedHealthRow`/`classify` → PREVENTED: the new auth-rejected message is a fixed
  literal; the env/path detail goes only to the structured `error!` log, NOT to the
  page reason.
- `set_auth_rejected` race (concurrent activation + API read) → SAFE: `AtomicBool`
  `Relaxed`, same advisory-health pattern as every other registry signal.
- Stuck-true after recovery → PREVENTED: success path clears it; a `Rejected` is
  re-evaluated every activation cycle.

## Test Plan

- `feed_health.rs` unit tests: `classify` returns `Down` + the fixed message when
  `enabled && auth_rejected`; does NOT trigger when `auth_rejected=false`; Disabled
  still wins over auth_rejected; registry `set_auth_rejected` + `snapshot` round-trip.
- `auth.rs` unit tests: `classify_groww_auth_failure(400, body)` → `Rejected` with
  the status + redacted body; transport string → `Transport`; the classifier never
  echoes a secret.
- `cargo build --workspace`; `cargo test -p tickvault-common -p tickvault-core
  -p tickvault-app -p tickvault-api`; common changed → escalate to
  `cargo test --workspace`.
- Adversarial 3-agent review (hot-path + security + hostile) on the diff.

## Rollback

Pure additive/diagnostic change. To roll back: `git revert` the single commit.
`auth_rejected` defaults `false` everywhere, so reverting restores the exact prior
behaviour (bare DOWN/DEGRADED + `<env>` literal). No schema, no migration, no config
flag, no data path touched — the live tick path, dedup, and mapping are untouched.

## Observability

- New per-feed `FeedHealthRegistry.auth_rejected` signal → surfaced on
  `GET /api/feeds/health` via the verdict/reason (Down + fixed message) for the
  Feed Control page.
- The structured `error!` at the activation smoke-check failure site now carries the
  resolved `env`, the api-key + totp-secret SSM `path`s, and (for Rejected) the HTTP
  `status` + redacted `body` — so the log is self-diagnosing.
- `secret_manager.rs` one-time `warn!` on env-default + empty-path edge cases.
- No new Prometheus counter / audit table (this is a diagnosis-surfacing change on an
  existing boot-time best-effort smoke-check, not a new tick/order/persist path).

---

## Per-Item Guarantee Matrix

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | new branches in `classify` + `classify_groww_auth_failure` have unit tests; `quality/crate-coverage-thresholds.toml` | post-merge llvm-cov | tests added in this PR |
| 100% audit coverage | N/A — diagnosis-surfacing on an existing best-effort smoke-check; no new typed event/table | `mcp__tickvault-logs__questdb_sql` | declared N/A |
| 100% testing coverage | unit (classify, registry round-trip, classifier) | `cargo test --workspace` green | unit tests declared |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan | pre-push mandatory | all gates green |
| 100% code performance | N/A — cold control-plane (feed toggle + boot smoke-check), not hot path; setters are O(1) `Relaxed` atomics | `cargo bench` | declared N/A (cold path) |
| 100% monitoring | structured `error!` with env+paths+status+body; registry signal on `/api/feeds/health` | `mcp__tickvault-logs__tail_errors` | log + health row |
| 100% logging | every failure path uses `error!`/`warn!`; no secret in any string | hourly errors.jsonl | enforced |
| 100% alerting | the smoke-check `error!` routes via the 5-sink pipeline; Feed page Down verdict | operator page | existing route reused |
| 100% security | api-key/totp `Secret<T>`, never formatted; only redacted body + SSM paths logged; security-reviewer agent | `cargo audit` | security-reviewer run |
| 100% security hardening | `&'static str` page-string contract preserved; no value-leak | post-deploy verify | contract preserved |
| 100% bugs fixing | adversarial 3-agent review on the diff | pre-PR + post-impl | 3 agents run |
| 100% scenarios covering | reject vs transport vs credentials vs recover vs disabled-wins covered in tests | test suite | scenarios in tests |
| 100% functionalities covering | every new pub fn has a test + a call site | pre-push 6+11 | tests + call sites |
| 100% code review | adversarial 3-agent before AND after impl | per-PR | both passes |
| 100% extreme check | unit tests fail the build if the auth-rejected branch / classifier regress | every commit | ratchet tests |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | unchanged — no tick path touched; ring→spill→DLQ intact | no new tick-drop path |
| WS never disconnects | unchanged — `SubscribeRxGuard` + pool watchdog untouched | no WS path change |
| Never slow/locked/hanged | cold path only; `set_auth_rejected` is an O(1) `Relaxed` atomic store | no hot-path alloc |
| QuestDB never fails | unchanged — no persistence path touched | self-heal intact |
| O(1) latency | registry setter/reader are O(1) array-index `Relaxed` atomics | per-feed array index |
| Uniqueness + dedup | unchanged — no DEDUP key touched | N/A — no storage key |
| Real-time proof | `/api/feeds/health` reflects auth_rejected the instant the activation cycle sets it | health snapshot |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the Groww
auth-rejection is now surfaced (Down + a fixed operator message) on the Feed Control
page and named in the structured log (resolved env, both SSM paths, Groww's redacted
status+body) via unit-tested pure functions (`classify`, `classify_groww_auth_failure`)
and an O(1) `AtomicBool` registry signal. This is diagnosis-surfacing on an existing
best-effort boot-time smoke-check — it does NOT change the live tick path, the HTTP
request, dedup, or mapping, so the zero-tick-loss + reconnect + O(1) hot-path
guarantees are untouched. No secret value can reach any log/error/page string (only
the redacted response body + the SSM paths appear).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww rejects api-key (HTTP 400) | activation logs env+paths+status+redacted-body; `set_auth_rejected(Groww,true)`; page shows Down "auth rejected — refresh the Groww SSM api-key" |
| 2 | Groww unreachable (network) | `Transport`; auth_rejected NOT set; page shows existing verdict |
| 3 | SSM fetch fails | `Credentials`; logged; auth_rejected NOT set |
| 4 | Auth recovers next cycle | `set_auth_rejected(Groww,false)`; verdict resolves normally |
| 5 | auth_rejected true but feed disabled | page shows Disabled (highest priority), not red |
| 6 | env vars unset | one-time warn names default `dev`; SSM prefix unchanged |
