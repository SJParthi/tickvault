# Plan: WebSocket disconnect/reconnect FULL structured logging → CloudWatch + redaction hardening

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "whenever disconnect and reconnect it should always
alert and display the precise failure message ... everything should be entirely logged
monitored tracked captured ... go ahead with this")
**Branch:** claude/nice-cerf-hemuvn
**Changed crates:** `core` (connection.rs disconnect/reconnect logging + Dhan-code threading),
`common` (sanitize.rs redactor scheme/param hardening)

## Why (operator request, 2026-06-12)
The operator's directive REJECTS "reduce alert noise": every WebSocket disconnect AND reconnect
must ALWAYS alert with the precise failure message AND be entirely logged/monitored/tracked/
captured/identified/visualised. Audit found the 4 disconnect + 2 reconnect emit sites in
`crates/core/src/websocket/connection.rs` only called `notify()` (Telegram) — they emitted NO
structured tracing log, so disconnects/reconnects never reached CloudWatch/app.log. This slice
closes that gap so the events are durable in CloudWatch, carry the PRECISE source (Dhan code
threaded through the classifier), and leak NO secret.

## Design
1. `record_disconnect(reason, dhan_code: Option<u16>)` is the SINGLE choke point for all 4
   disconnect emit sites. It now `warn!("WebSocket disconnected", reason=<redacted>, source=<label>)`
   so EVERY disconnect lands in CloudWatch — with the PRECISE source from
   `classify_disconnect_cause(reason, dhan_code)`. The authoritative Dhan code (805/807/…) is
   threaded from the two Dhan-coded branches (`Some(code.as_u16())`); transport errors pass `None`.
2. The reconnect emit site `info!("WebSocket reconnected", reason=<redacted>, source, down_secs,
   attempts)`. The Dhan code captured at disconnect is stored in a new `AtomicU32` slot
   (`u32::MAX` = none) and returned by `take_disconnect_context()` so the reconnect log classifies
   the SAME precise source the disconnect log did.
3. SECURITY: a TLS/handshake `reason` can embed the live-feed URL
   `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2`. ALL log sites redact
   via `tickvault_common::sanitize::redact_url_params`. The redactor itself is hardened:
   `redact_urls` now recognizes `wss://`/`ws://` (was http(s) only — the JWT would have leaked),
   and `redact_url_params` adds `token=`/`clientId=`/`authType=` standalone-param fallbacks.

## Edge Cases
- Multi-step disconnect (807 → retry fail → reconnect): latest reason + latest Dhan code win;
  first-disconnect timestamp preserved for `down_secs`. Covered by the updated unit test.
- Transport error with no Dhan code → `None` → classifier uses string heuristic (reset/network).
- `reason` containing a bare `token=...` with no URL scheme → standalone param fallback redacts it.
- First successful connect (`reconnection_count == 0`) → no reconnect log (by design; not a reconnect).
- Empty reason / empty input → `redact_url_params("")` returns `""`.

## Failure Modes
- Redactor scheme gap (the exact bug found): `wss://` JWT leaks to CloudWatch → FIXED + ratcheted
  by `test_redact_url_params_redacts_wss_feed_url_with_jwt`.
- Raw `error = %err` at the two transport call-site warns leaked unredacted → now redacted.
- Lock poison on `last_disconnect_reason` → `if let Ok(..)` guards (no panic). Atomics are lock-free.
- A future edit removing the logs → source-scan guard `ws_disconnect_logging_guard.rs` fails the build.

## Test Plan
- `crates/core/tests/ws_disconnect_logging_guard.rs` (3 source-scan tests): both log strings present
  + `classify_disconnect_cause` used ≥ 2× (disconnect + reconnect).
- `crates/core/src/websocket/connection.rs::tests::test_record_disconnect_then_take_returns_context_and_resets`
  updated to the 4-tuple signature + asserts Dhan-code threading (807 round-trips, latest-wins, reset).
- `crates/common/src/sanitize.rs::tests`: 4 new tests — wss feed-URL JWT redaction (no 20-char window
  survives), plain `ws://`, standalone `token=`/`clientId=`, and an https no-regression test.
- Scoped: `cargo test -p tickvault-common` + `-p tickvault-core` (common change escalates to the
  downstream consumers); fmt + clippy clean; 3-agent adversarial review BEFORE+AFTER.

## Rollback
Pure additive logging + a backward-compatible redactor widening. Revert = `git revert` of the single
commit; no schema, no migration, no data path touched. The `record_disconnect` signature change is
internal to `connection.rs` (one new param) — no cross-crate API surface changes.

## Observability
- Layer 4 (logs): `warn!("WebSocket disconnected")` + `info!("WebSocket reconnected")` with
  `code`-free structured fields → CloudWatch `/tickvault/prod/app`.
- Layer 5 (Telegram): unchanged `WebSocketDisconnected/Reconnected/OffHours` events (already carry
  the "Likely source:" line from #1108) — every disconnect/reconnect still pages with precise source.
- Layer 1 (counters): existing `tv_ws_reconnect_total` + `tv_ws_reconnect_gap_seconds_total` preserved.
- No new counter/table needed for THIS slice (a dedicated WS-event AUDIT TABLE is the NEXT slice).

## Per-Item Guarantee Matrix
This item carries the mandatory 15-row + 7-row guarantee matrix by reference — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. Specifics for this slice:
- 100% testing: source-scan guard + unit (Dhan-code threading) + 4 redaction regression tests.
- 100% security: the wss:// JWT-leak hole found by the security-reviewer agent is FIXED + ratcheted.
- 100% logging: every disconnect/reconnect now reaches CloudWatch (was Telegram-only).
- Honest envelope: redaction is bounded to recognized URL schemes + the named credential params;
  a novel secret param name in a future Dhan URL would need a fallback addition (ratchet would catch
  the wss case; a brand-new param name is the known boundary).

## Plan Items
- [x] Thread authoritative Dhan code through `record_disconnect` + `take_disconnect_context`
  - Files: crates/core/src/websocket/connection.rs
  - Tests: test_record_disconnect_then_take_returns_context_and_resets
- [x] `warn!`/`info!` structured logs at disconnect choke point + reconnect site, redacted + sourced
  - Files: crates/core/src/websocket/connection.rs
  - Tests: ws_disconnect_logging_guard (3 tests)
- [x] Redact raw `err` at the 2 transport call-site warns (close pre-existing leak)
  - Files: crates/core/src/websocket/connection.rs
- [x] Harden `redact_url_params`/`redact_urls` for `wss://`/`ws://` + `token=`/`clientId=`/`authType=`
  - Files: crates/common/src/sanitize.rs
  - Tests: test_redact_url_params_redacts_wss_feed_url_with_jwt, _plain_ws_scheme,
    _standalone_token_and_client_id, _still_redacts_https

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | 807 token-expired disconnect | warn log `source=token expired`, Dhan code 807 threaded to reconnect log |
| 2 | TLS error embedding wss feed URL w/ JWT | log shows `?[REDACTED]`; no JWT/clientId substring |
| 3 | Generic transport reset | warn log `source` = network/reset; redacted err |
| 4 | Successful reconnect after outage | info log `WebSocket reconnected` + down_secs + attempts + source |
| 5 | Future edit removes a log | ws_disconnect_logging_guard fails the build |
