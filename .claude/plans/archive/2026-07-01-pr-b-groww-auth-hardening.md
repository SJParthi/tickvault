# Implementation Plan: PR-B — Groww auth hardening (stop parse-error interpolation + propagate body-read error)

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** Parthiban (operator task brief 2026-06-28)
**Crate:** `core` (`tickvault-core`)
**File:** `crates/core/src/feed/groww/auth.rs`

## Design

Two small, surgical security/correctness hardenings to the native-Rust Groww
access-token auth path (`crates/core/src/feed/groww/auth.rs`), the PR-2 auth
module of the Groww second-feed sequence. No behavioural change to the wire
request (URL, Bearer scheme, `x-api-version`, body) and no public-API change.

1. **Stop interpolating the serde parse error in `parse_access_token_response`.**
   The function parses the Groww access-token JSON body into a `SecretString`.
   On a malformed body it returned
   `ApplicationError::AuthenticationFailed { reason: format!("...: {err}") }`,
   interpolating the `serde_json::Error`. Because a 2xx body carries the SECRET
   token, a serde error Display can embed a quoted fragment of that body — a
   potential token leak into a logged error string. Fix: replace the
   interpolation with a FIXED `&'static str` reason
   (`"Groww access-token response was not valid JSON"`). Error variant/type
   unchanged (`ApplicationError::AuthenticationFailed`); only the `reason` value
   is now constant. The caller (`obtain_groww_access_token`) already discards
   this error and substitutes a fixed `Rejected.body`, so this is defense-in-depth
   at the source, covering any future logging/use of the parse error.

2. **Propagate body-read errors as `Transport` in `obtain_groww_access_token`.**
   `response.text().await.unwrap_or_default()` silently turned a body-read
   failure (connection broke while reading the response body) into an empty
   string, which would then be mis-classified (e.g. a 2xx with an unreadable
   body → "empty token" Rejected, or a non-2xx with an unreadable body →
   Rejected with an empty body). A body-read failure is a TRANSPORT-class fault
   (the read leg of the connection failed), identical in nature to the send
   failure already classified `Transport` one statement above. Fix:
   `response.text().await.map_err(|err| GrowwAuthSmokeError::Transport(err.to_string()))?`.
   `obtain_groww_access_token` returns `Result<_, GrowwAuthSmokeError>`, so `?`
   composes directly. A body-read transport error carries no token, so its
   Display is safe to surface.

## Edge Cases

- Malformed 2xx body that begins as valid JSON then breaks (e.g.
  `{"token": SECRETMARKER123 broken json`) → fixed-message error, marker never
  echoed (new test).
- Empty / non-JSON body → still errors (existing `rejects_garbage` test).
- `accessToken` camelCase alias → still parses (existing test, unchanged).
- Body-read error on a 2xx response → now `Transport`, not a false "empty
  token" Rejected.
- Body-read error on a non-2xx response → now `Transport`, not a `Rejected`
  with an empty body (more accurate: we never read Groww's rejection text).

## Failure Modes

- Connection reset mid-body-read → `GrowwAuthSmokeError::Transport(<reqwest err display>)`;
  the activation watcher MUST NOT mark the api-key auth-rejected (transient).
- Malformed token JSON → `ApplicationError::AuthenticationFailed` with a fixed
  reason; surfaced by the caller as `Rejected { body: "2xx response did not
  contain a usable token" }` (unchanged caller behaviour).
- No new panic path, no new `unwrap`/`expect`, no secret in any error string.

## Test Plan

- New unit test `test_parse_access_token_response_error_does_not_echo_input_bytes`:
  feeds a body containing `SECRETMARKER123`, asserts neither the error's Display
  nor Debug contains the marker, and that the variant is still
  `AuthenticationFailed`. This is the ratchet proving the no-leak invariant.
- Existing tests retained unchanged and must still pass:
  `test_parse_access_token_response_extracts_token`,
  `..._accepts_camel_alias`, `..._missing_token_errors`,
  `..._empty_token_errors`, `..._rejects_garbage`, the `classify_*` tests, and
  the Display test.
- Body-read `Transport` path: exercised by the type system (`?` on a
  `Result<_, GrowwAuthSmokeError>`); the surrounding `obtain_groww_access_token`
  is TEST-EXEMPT (live network) per its existing annotation, and its pure
  sub-parts remain unit-tested. The happy-path parse test confirms the function
  still compiles + returns correctly.
- Verify: `cargo fmt --check`, `cargo test -p tickvault-core --lib feed::groww::auth`,
  `cargo clippy -p tickvault-core -- -D warnings -W clippy::perf`,
  `cargo build --workspace`, banned-pattern scanner.

## Rollback

Single-file, two-hunk + one-test change. `git revert <sha>` restores the prior
`format!("...{err}")` interpolation and the `unwrap_or_default()`. No schema, no
config, no migration, no wire change — rollback is instant and side-effect-free.

## Observability

No new metric / log / telemetry surface is added (these are error-string
hardenings, not new code paths). The EXISTING observability is preserved and
improved: a body-read failure now surfaces as a typed `Transport` error the
activation watcher already logs and routes, instead of being silently swallowed
into an empty string. The parse error's `reason` field is now a fixed string,
so any downstream log of it is guaranteed secret-free. No `error!`-with-code is
added (no new failure class — `ApplicationError::AuthenticationFailed` and
`GrowwAuthSmokeError::Transport` already exist with their established routing).

## Plan Items

- [x] Edit 1 — `parse_access_token_response`: replace `format!("...{err}")` with
  a fixed `&'static str` reason (no serde error interpolation)
  - Files: crates/core/src/feed/groww/auth.rs
  - Tests: test_parse_access_token_response_error_does_not_echo_input_bytes
- [x] Edit 2 — `obtain_groww_access_token`: replace
  `response.text().await.unwrap_or_default()` with
  `.map_err(|err| GrowwAuthSmokeError::Transport(err.to_string()))?`
  - Files: crates/core/src/feed/groww/auth.rs
  - Tests: type-checked via `?`; happy-path covered by existing parse tests
- [x] New test — no-leak ratchet for the parse error
  - Files: crates/core/src/feed/groww/auth.rs
  - Tests: test_parse_access_token_response_error_does_not_echo_input_bytes

## Per-Item Guarantee Matrix (PR-B)

### 15-row "100% everything" matrix

| Demand | Mechanical proof artefact | Per-item gate |
|---|---|---|
| 100% code coverage | new unit test + retained existing tests cover both edited fns' pure logic | tests added in same file |
| 100% audit coverage | N/A — no new typed event / audit table (error-string hardening only) | N/A |
| 100% testing coverage | unit tests (parse + no-leak); body-read path type-checked | `cargo test -p tickvault-core --lib feed::groww::auth` |
| 100% code checks | fmt + clippy -D warnings + banned-pattern scanner | pre-push |
| 100% code performance | N/A — cold auth path (not hot path); no allocation change of note | N/A — not hot path |
| 100% monitoring | existing Transport/AuthenticationFailed routing preserved; no new counter | unchanged |
| 100% logging | parse `reason` now fixed `&'static str` (secret-free); tracing unchanged | code review |
| 100% alerting | N/A — no new failure class; existing alert routing preserved | N/A |
| 100% security | STOPS a potential token leak in error strings; new no-leak ratchet test | security intent of PR |
| 100% security hardening | fixed-message error + propagated transport error; `Secret<T>` token unchanged | code review |
| 100% bug fixing | fixes silent body-read swallow (mis-classification) + parse-error leak | both edits |
| 100% scenarios covering | malformed-body, empty-body, camel-alias, body-read-fail scenarios covered | tests + types |
| 100% functionalities covering | no new pub fn; edited fns retain tests + call sites | pub-fn-test-guard |
| 100% code review | diff reviewed by orchestrator before push (serial-PR protocol) | pre-merge |
| 100% extreme check | no-leak ratchet test fails build if interpolation is reintroduced | new test |

### 7-row "Resilience demand" matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | N/A — auth path, no tick handling | no tick-drop path touched |
| WS never disconnects | N/A — auth path, not the WS read loop | unchanged |
| Never slow/locked/hanged | cold one-shot auth path; no new lock/alloc | no hot-path change |
| QuestDB never fails | N/A — no storage path touched | unchanged |
| O(1) latency | N/A — cold path; parse is O(body len), inherently O(N), flagged honestly | not a hot-path lookup |
| Uniqueness + dedup | N/A — no keys/collections touched | unchanged |
| Real-time proof | existing Transport/AuthenticationFailed signals preserved | code review |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
no-leak invariant for `parse_access_token_response` is pinned by
`test_parse_access_token_response_error_does_not_echo_input_bytes` (fails the
build if the serde-error interpolation returns); the body-read error path is
type-checked to flow into `GrowwAuthSmokeError::Transport` via `?`. No tick,
WS, storage, or hot-path behaviour is touched. This is a cold-path
error-string hardening, not an unbounded "never leaks" guarantee — the test
proves the specific marker-not-echoed property on the specific edited function.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Malformed 2xx body with secret marker | error returned; marker absent from Display + Debug; variant AuthenticationFailed |
| 2 | Valid `{"token":...}` body | parses to SecretString (existing test) |
| 3 | `accessToken` camelCase alias | parses (existing test) |
| 4 | Empty / non-JSON body | errors (existing test) |
| 5 | Body-read failure (connection broke) | `GrowwAuthSmokeError::Transport`, not a false empty/Rejected |
