# Implementation Plan: Durable fix for the false "auth rejected — refresh the Groww SSM api-key" RED

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session (audit-driven, code-cited)

Crates touched: **tickvault-common** (`crates/common/src/feed_health.rs`,
`crates/common/src/market_hours.rs`), **tickvault-api**
(`crates/api/src/handlers/feeds.rs`), **tickvault-app**
(`crates/app/src/groww_sidecar_supervisor.rs`, `crates/app/src/main.rs`).

PR #1260 only partially fixed the recurring false "auth rejected — refresh the
Groww SSM api-key" RED. A deep audit found it can still recur via THREE paths.
This plan fixes all three durably, contained to the feed-health / status-display
layer — NOTHING touches ticks / dedup / persist / orders.

## Plan Items

- [ ] **HIGH-1 — trading-day-aware market-closed gate.** The `/feeds` health
  market-closed bypass uses **time-of-day-only** `is_within_market_hours_ist()`,
  which returns `true` inside [09:00,15:30) on a Saturday/Sunday/NSE holiday →
  the market-closed-idle bypass never fires → a latched `auth_rejected`
  re-surfaces the false RED. Fix: thread a **trading-day-aware** market-open
  decision into the verdict. Install a global `TradingCalendar` handle in
  `common::market_hours` (mirrors the `core::websocket::connection`
  `set_market_calendar` `OnceLock` precedent, but in `common` so the api crate
  can read it) + a `is_trading_session_now()` free fn = weekday-gated
  `is_within_trading_session_ist()` AND `calendar.is_trading_day_today()`
  (falls back to the weekday-only gate when no calendar is installed). The
  `/feeds` handler computes `market_open` from it. `main.rs` installs the
  calendar at boot.
  - Files: `crates/common/src/market_hours.rs`, `crates/api/src/handlers/feeds.rs`, `crates/app/src/main.rs`
  - Tests: `weekend_or_holiday_is_not_trading_session_when_calendar_installed`, `trading_session_now_falls_back_to_weekday_gate_when_no_calendar`, `set_market_calendar_for_session_is_idempotent`

- [ ] **HIGH-2 — non-tick recovery edge for `auth_rejected`.** Today the ONLY
  mid-session clear is a real persisted tick (`record_tick`/`record_ticks(n>0)`);
  in a legitimately tick-silent window a stale `auth_rejected` latches for the
  whole session. Fix: add a public `FeedHealthRegistry::clear_auth_rejected(feed)`
  edge-triggered clear (reusing the existing private
  `clear_auth_rejected_on_recovery`), and call it from the sidecar supervisor on
  a confirmed `Streaming` line (`groww auth OK` / NDJSON append) — a genuine
  non-tick "the alert is over" recovery edge. The no-false-OK rule is preserved:
  a 0-row flush still does NOT clear; only a real streaming/auth-OK signal does.
  - Files: `crates/common/src/feed_health.rs`, `crates/app/src/groww_sidecar_supervisor.rs`
  - Tests: `test_registry_clear_auth_rejected_clears_when_set`, `test_registry_clear_auth_rejected_is_noop_when_clear`, `streaming_class_clears_auth_rejected` (supervisor)

- [ ] **MEDIUM (root) — generic `Error` class must NOT set `auth_rejected`.** The
  sidecar stderr classifier routes a non-auth NATS/SDK reconnect line or a
  recovered `traceback`/`error:` into the generic `Error` class, which shared the
  SAME `auth_rejected` bit as a real credential reject → "refresh the api-key" RED
  for a non-auth condition. Fix: add `SidecarLineClass::sets_auth_rejected()`
  (true ONLY for `AuthRejected` + `EntitlementRejected`); the supervisor sets
  `auth_rejected` ONLY when that predicate is true. `Error` keeps its `error!`
  log + (via the existing `triggers_alert`) its visibility, but no longer latches
  the actionable auth RED. This is the root that makes the whole false-auth
  family possible.
  - Files: `crates/app/src/groww_sidecar_supervisor.rs`
  - Tests: `error_class_does_not_set_auth_rejected`, `auth_and_entitlement_classes_set_auth_rejected`, `sets_auth_rejected_predicate_matches_only_hard_reject_classes`

## Design

The defect family lives entirely in the **status-display verdict layer**. The
real-time tick/dedup/persist/order paths are untouched. Three precise edits:

1. **Trading-day-aware `market_open`** (HIGH-1). The `feed_health` verdict is a
   pure function that already takes a `market_open: bool`. The bug is that the
   caller computes that bool from a time-of-day-only helper. We make the caller
   compute it from a **trading-day-aware** helper. Holiday awareness needs a
   `TradingCalendar`, which lives in `common`; the api handler has no calendar
   handle. We add a global `OnceLock<Arc<TradingCalendar>>` in
   `common::market_hours` + `is_trading_session_now()` (weekday gate AND
   `is_trading_day_today()`), installed once by `main.rs` at boot. The verdict
   logic in `feed_health.rs` is unchanged — only its `market_open` input becomes
   honest. Reuse over duplication: `is_within_trading_session_ist()` (the
   existing weekday gate) is the fallback and the base of the new fn.

2. **Non-tick clear edge** (HIGH-2). `auth_rejected` is an `AtomicBool` per feed.
   The existing private `clear_auth_rejected_on_recovery` is the exact
   edge-triggered (`load` → single `compare_exchange true→false`) primitive we
   need; we expose a thin public `clear_auth_rejected(feed)` over it and call it
   on a `Streaming` sidecar line — the genuine non-tick recovery signal.

3. **Split visibility-alert from the auth latch** (MEDIUM). `triggers_alert`
   currently means BOTH "fire Telegram" AND "latch auth_rejected". We split the
   second meaning into `sets_auth_rejected()` (only hard auth/entitlement reject
   classes). `Error` stays an alert (Telegram + `error!`) but stops latching the
   actionable auth RED.

## Edge Cases

- Boot before `set_market_calendar_for_session` is installed (e.g. unit tests,
  or the brief boot window): `is_trading_session_now()` falls back to the
  weekday-only gate (still kills the dominant ~104 weekend days/year; holidays
  covered once installed). No panic, no false RED worse than today.
- `TEST_FORCE_IN_MARKET_HOURS` set: `is_trading_session_now()` honours it
  (returns `true`) so existing market-hours tests keep working.
- A 0-row Groww flush during a silent window: `record_ticks(0)` is still a no-op;
  the `Streaming` clear edge fires only on a real `groww auth OK`/NDJSON-append
  line, never on an empty flush (no false recovery — audit Rule 11).
- A real hard auth reject (`error [auth]`) followed by a benign `Streaming` line
  while the credential is genuinely dead: the sidecar only prints `groww auth OK`
  AFTER a successful re-auth, so clearing on it is correct; if the credential is
  truly dead the sidecar prints the auth-reject line again (re-latch). Edge-safe.
- Idempotent calendar install: second `set_*` call returns `false` (OnceLock),
  logged, no effect.
- Holiday that is ALSO a weekend: `is_trading_day_today()` returns false for both
  — no double-counting, single false-path closed.

## Failure Modes

- Calendar `OnceLock` not installed → fallback weekday gate (degraded, not
  broken; never a worse false-RED than the pre-PR state).
- `clear_auth_rejected` racing a re-set from a concurrent hard reject → the
  `compare_exchange` loses via `Err` and the re-set wins (the credential really
  is bad) — the correct outcome.
- `sets_auth_rejected()` mis-classification → covered by unit tests pinning every
  class; a regression that makes `Error` latch again fails the build.
- No new allocation, no new lock on any hot path (all changes are cold-path
  status/display + supervisor stderr drain).

## Test Plan

Pure functions make every fix testable without a live feed:
- `common/feed_health.rs` unit tests: `clear_auth_rejected` clears-when-set and
  is-noop-when-clear (no false recovery).
- `common/market_hours.rs` unit tests: weekend/holiday → not a trading session
  when a calendar is installed; weekday fallback when none; idempotent install.
- `app/groww_sidecar_supervisor.rs` unit tests: `Error` does NOT set
  auth_rejected; `AuthRejected`/`EntitlementRejected` DO; `sets_auth_rejected`
  predicate matches only the hard-reject classes; `Streaming` clears.
- Run `cargo test -p tickvault-common`, `-p tickvault-api`, `-p tickvault-app`
  (the touched crates) all clean. Run `bash .claude/hooks/banned-pattern-scanner.sh`
  + `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all`. Do NOT trip
  `crates/storage/tests/error_level_meta_guard.rs` (no flush/persist phrase
  downgraded — `Error` stays `error!`).

## Rollback

Each fix is independent and contained to the display layer. To roll back:
revert the single PR. No schema change, no migration, no data path touched, so a
revert is byte-safe — the system returns to the pre-PR (#1261) behaviour with the
known false-RED, never to a worse state. The global calendar `OnceLock` is
install-once and read-only; reverting simply stops installing it and the verdict
falls back to the time-of-day gate.

## Observability

- No new `error!` codes needed — this is a status-display correctness fix.
- The `/feeds` health row already echoes `auth_rejected`, `verdict`, `reason`,
  `market_open` — those become honest (the operator literally sees
  `market_open=false` + `verdict=ok` on weekends/holidays instead of the false
  Down). The existing `GrowwSidecarRejected` Telegram still fires for hard
  rejects; the generic `Error` keeps its `error!` log (5-sink → CloudWatch) but
  no longer pages "refresh the api-key".
- Honest envelope: after this PR the actionable "auth rejected — refresh the
  Groww SSM api-key" Down appears ONLY on a confirmed auth/entitlement reject
  DURING an actual trading session, and self-clears on a confirmed re-auth /
  streaming edge without needing a tick — 100% inside the tested verdict
  envelope, ratcheted by the unit tests above.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Saturday 11:00 IST, latched auth_rejected, calendar installed | verdict=Ok, reason="market closed — idle is normal" |
| 2 | NSE holiday 11:00 IST, latched auth_rejected, calendar installed | verdict=Ok (not trading day) |
| 3 | Weekday 11:00 IST, genuine auth reject | verdict=Down, "auth rejected — refresh the Groww SSM api-key" |
| 4 | Benign NATS reconnect `error:` line during market hours | Error class: error! log + Telegram, but auth_rejected NOT set → no false RED |
| 5 | Stale auth_rejected, then `groww auth OK` streaming line, no tick yet | auth_rejected cleared (non-tick recovery edge) |
| 6 | No calendar installed (boot window / tests), Saturday | falls back to weekday gate → not a trading session |
