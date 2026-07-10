# Implementation Plan: Groww native shadow — socket-token mint HTTP 307 fix

**Status:** APPROVED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator) — Monday-critical fix directive with live 15:10–15:12 IST evidence, this session

> Guarantee coverage: cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`
> (the 15-row + 7-row matrices apply as written; this is a cold-path auth fix — no
> hot-path row is touched).

## Design

**Live evidence (operator run, 2026-07-04 15:10–15:12 IST):**
`GROWW-NATIVE-02: socket-token mint failed — groww socket-token: http status 307`
— 6 expo-backoff retries, then blocked. The native Rust shadow client
(`crates/core/src/feed/groww/native/socket_token.rs`) uses
`reqwest::redirect::Policy::none()` (the §18-class hardening) and correctly
refuses to follow, so the mint never succeeds. The Python sidecar succeeds
against the SAME endpoint because `requests` follows redirects by default
(307 preserves POST + body).

**Root-cause comparison (growwapi-1.5.0 wheel, verified this session):**
- SDK URL (`growwapi/groww/client.py:117` + `growwapi/groww/feed.py:125`):
  `https://api.groww.in/v1/api/apex/v1/socket/token/create/` — **byte-identical**
  to our `GROWW_SOCKET_TOKEN_URL` (`crates/common/src/constants.rs:659`).
- SDK headers (`client.py::_build_headers` + `generate_socket_token`):
  `x-request-id` (uuid4), `Authorization: Bearer`, `Content-Type: application/json`,
  `x-client-id: growwapi`, `x-client-platform: growwapi-python-client`,
  `x-client-platform-version: 1.5.0`, `x-api-version: 1.0` — identical to ours.
- SDK body: `{"socketKey": <nkey public>}` — identical.

**Conclusion:** no client-side diff exists. The 307 is SERVER-SIDE
canonicalization we cannot see from the request (load-balancer / gateway
redirect the SDK silently follows). Per the operator directive, since step-1
found no diff, we implement a **bounded SAME-HOST redirect follow for THIS
request only**:

1. `crates/core/src/feed/groww/native/socket_token.rs`:
   - Follow at most **1 hop**, ONLY for method-preserving codes **307/308**,
     ONLY when the Location target stays `https://api.groww.in` (pure fn
     `same_host_redirect_target(base_url, location) -> Option<String>` —
     resolves relative Locations against the request URL, refuses cross-host,
     refuses http downgrade, refuses empty). The hop re-sends the SAME method
     + headers + body per 307 semantics and is logged (`info!` with sanitized
     Location).
   - Any OTHER 3xx (301/302/303 mutate POST→GET), a second redirect, a
     cross-host/downgrade Location, or a missing Location → new typed error
     `SocketTokenError::Redirect { status, location }` whose Display carries
     the **sanitized** Location (query + fragment stripped via pure fn
     `sanitize_location`) so any future redirect is self-explaining in the
     GROWW-NATIVE-02 error line.
   - The client keeps `redirect::Policy::none()` — reqwest never auto-follows;
     the ONE validated hop is explicit application code (§18 hardening kept).
2. `crates/common/src/constants.rs`: URL unchanged (already canonical vs SDK).

## Edge Cases

- Location is relative (`/v1/...`) → resolved against the request URL (same host by construction) → followed.
- Location is absolute same-host https → followed (query preserved on the wire, stripped only in logs).
- Location is cross-host (`evil.example`) → refused, error carries sanitized target.
- Location downgrades to `http://` → refused.
- Location empty / missing / non-ASCII header value → refused with empty sanitized location.
- Second consecutive 307 (redirect loop) → hop budget (1) exhausted → typed Redirect error, backoff ladder in `client.rs` proceeds unchanged.
- 301/302/303 → never followed (method-mutating), typed Redirect error.
- 401/403 after the followed hop → existing `is_auth_status` SSM re-read path unchanged; `Redirect` is never auth-class.

## Failure Modes

- Groww later removes the redirect → hop code is simply never taken; direct 2xx path unchanged.
- Groww redirects to a different host permanently → mint fails loudly with the sanitized target named in GROWW-NATIVE-02 (operator sees WHERE it wants to go; protocol notes updated then).
- Hostile/poisoned Location (DNS-poison class) → same-host + https validation refuses; no token ever sent off `api.groww.in`.
- Transport failure on the hop → existing `Transport` error + caller backoff (unchanged ladder).

## Test Plan

Unit tests in `socket_token.rs::tests` (pure fns; no live HTTP):
- `test_socket_token_url_matches_sdk_wheel_canonical_form` — pins the constant to the SDK-verified byte-exact form (wheel paths cited in comment).
- `test_same_host_redirect_target_accepts_absolute_same_host`
- `test_same_host_redirect_target_resolves_relative_path`
- `test_same_host_redirect_target_refuses_cross_host`
- `test_same_host_redirect_target_refuses_http_downgrade`
- `test_same_host_redirect_target_refuses_empty`
- `test_sanitize_location_strips_query_and_fragment`
- `test_redirect_error_display_includes_sanitized_location`
- `test_is_auth_status` extended: `Redirect` is not auth-class.

Scoped run: `cargo test -p tickvault-core --lib groww` + `cargo fmt` + clippy on touched crate.

## Rollback

Single-commit revert (`git revert <sha>`) restores the refuse-all-redirects behaviour; no schema, no config, no cross-crate surface. The feature is default-OFF anyway (`groww_native_shadow = false`), so prod behaviour is unchanged until the flag is flipped.

## Observability

- The followed hop logs `info!(status, location = <sanitized>, "groww socket-token: following one same-host redirect hop")`.
- Any refused 3xx surfaces through the EXISTING `GROWW-NATIVE-02` `error!` in `client.rs` — the error Display now names the sanitized redirect target, making the failure self-explaining (the operator's 15:10 line would have read `… http status 307 redirect to <target> (not followed)`).
- Existing counters unchanged (`tv_groww_native_reconnects_total` etc.); no new metric needed for a one-hop cold-path auth call.

## Plan Items

- [x] One-hop same-host 307/308 follow + sanitized-Location diagnostics
  - Files: crates/core/src/feed/groww/native/socket_token.rs
  - Tests: test_socket_token_url_matches_sdk_wheel_canonical_form, test_same_host_redirect_target_refuses_cross_host, test_sanitize_location_strips_query_and_fragment, test_redirect_error_display_includes_sanitized_location

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Server 307s to same-host canonical URL | One logged hop, mint succeeds Monday |
| 2 | Server 307s cross-host | Refused; GROWW-NATIVE-02 names sanitized target |
| 3 | Redirect loop (307 → 307) | Hop budget 1 → typed error, backoff ladder |
| 4 | No redirect (direct 2xx) | Behaviour byte-identical to today |
