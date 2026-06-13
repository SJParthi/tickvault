# Plan: wire the missing `InstrumentBuildFailed` boot-failure alert

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "go ahead with the plan")
**Branch:** claude/nice-cerf-hemuvn
**Campaign:** observability dead-variant wiring (slice #2; slice #1 = `QuestDbReconnected` PR #1103, merged)
**Changed crates:** `app` (wiring), `core` (no code change — variant already complete)

---

## Design

`load_instruments(...).await?` is called at TWO sites inside `crates/app/src/main.rs::main()`
— the fast-boot path (~L1120) and the standard-boot path (~L2665). Both emit
`NotificationEvent::InstrumentBuildSuccess` on the `Ok` arm, but the `?` on the
call means **any `Err` propagates straight out of `main()` and the process exits
with only a stderr line — no operator Telegram**. The `InstrumentBuildFailed`
variant exists, is fully formed (`reason`, `manual_trigger_url`), is
`html_escape`d, hides the internal URL, is `Severity::High`, and has formatting +
severity tests — but has **zero production emitters**. This is the exact
asymmetry #1103 fixed (`QuestDbReconnected` ↔ `QuestDbDisconnected`): success is
announced, failure is silent.

**Change:** at each of the two sites, replace the bare `?` with an explicit match;
on `Err(e)` emit `InstrumentBuildFailed { reason: e.to_string(), manual_trigger_url }`
(URL built from `config.api.host`/`config.api.port`, consistent with the existing
formatter that discards it for security), then `return Err(e)` to **preserve the
fail-closed boot-halt** (no behavioural change to control flow — only an added
operator signal on the way out). No `events.rs` change. No new metric/table/dep.

**Why not a duplicate of `INSTR-FETCH-01..04`:** those fire at the per-attempt
level inside the daily-universe fetcher and **infinite-retry** (boot blocks, the
call never returns `Err` for fetch failures). The `Err` that reaches these two
sites is a *different* class: QuestDB reconcile/audit failure during cold-build
(`?`-propagated per `load_daily_universe_plan`'s docstring), snapshot/build
errors, or the `daily_universe_fetcher`-disabled `bail!`. None of those emit a
Telegram today.

## Edge Cases

- **Infinite-retry fetch failures** (`INSTR-FETCH-*`): never reach this arm (they
  loop, never return `Err`) → no double-alert. Verified against
  `load_daily_universe_plan` (`max_attempts = None`).
- **`Indices4Only` scope:** `load_instruments` returns `Ok` unconditionally for the
  synthetic locked universe → the new arm is never hit on that path. Harmless.
- **Both boot paths:** fast-boot uses `fast_notifier`; standard-boot uses
  `notifier`. Each handle is already in scope (used for the sibling
  `InstrumentBuildSuccess` emit immediately below the call).
- **NoOp notifier** (SSM unavailable at boot): `notify()` no-ops + increments
  `tv_telegram_dropped_total{reason="noop_mode"}` — no panic, boot still halts.

## Failure Modes

- **Delivery race on exit:** `notify()` sends via `tokio::spawn`; `main()` returning
  `Err` drops the runtime and may cancel the in-flight send. This is the SAME
  accepted behaviour as the sibling boot-block `IpVerificationFailed` emit (which
  notifies then returns). Mitigation in scope: emit then `return` mirroring the
  established pattern; the honest-envelope claim documents the race explicitly. A
  guaranteed-delivery flush for ALL boot-block notifications is a separate
  cross-cutting fix, out of scope for this one-variant slice.
- **`e.to_string()` content:** `reason` is `html_escape`d by the formatter; the
  REST client's `Display` already redacts query params, so no token/URL leak.
- **No new panic path:** match arm only adds a `notify()` + `return Err(e)`.

## Test Plan

- Existing `events.rs` tests cover formatting + severity (unchanged) — re-run to
  confirm no regression: `cargo test -p tickvault-core --lib notification`.
- New **source-scan wiring guard** (project convention, e.g.
  `secret_manager.rs::tests::test_orphan_position_watchdog_is_wired_into_main`):
  assert `crates/app/src/main.rs` contains an `InstrumentBuildFailed` emit at/near
  both `load_instruments(` call sites (i.e. the `Err` arm is wired, not the bare
  `?`). Fails the build if a future refactor reverts to silent `?`.
- `cargo build -p tickvault-app` clean; `cargo fmt --check`; CI-exact
  `cargo clippy --workspace --no-deps`.
- Pre-commit/pre-push gates green; test-count ratchet bumps by the one new test.

## Rollback

Single-commit revert restores the two bare `?` call sites; no schema, no dep, no
data migration, no config flag. The variant + its `events.rs` tests already exist
in `main` independently, so a revert leaves no dangling reference. Zero blast
radius beyond the two `main.rs` lines + one test.

## Observability

- **Telegram:** `InstrumentBuildFailed` (`Severity::High`) now fires on the boot
  hard-failure path — the operator learns "instruments failed, retry via the
  rebuild endpoint" instead of staring at a dead process.
- **Logging:** the existing `error!`/`?` propagation is unchanged; the boot-halt
  still surfaces in stderr/journald + `errors.jsonl`.
- **Metric:** no new metric; `notify()` already increments
  `tv_telegram_dispatched_total` / `tv_telegram_dropped_total` per the dispatch
  path.
- **Audit:** N/A — this is a boot-halt operator alert, not a SEBI lifecycle event;
  `INSTR-FETCH` audit rows already cover the fetch-attempt forensic chain.

---

## Z+ 15-row "100% everything" matrix

| Demand | Proof for this slice |
|---|---|
| Code coverage | new wiring-guard test; existing variant tests retained |
| Audit coverage | N/A — boot-halt alert, not a lifecycle event (`INSTR-FETCH` audits fetch attempts) |
| Testing coverage | unit (variant fmt/severity) + source-scan wiring guard |
| Code checks | banned-pattern + pub-fn-wiring + plan-gate + secret-scan green |
| Code performance | N/A — cold boot path, not hot path |
| Monitoring | `notify()` dispatch counters (existing) |
| Logging | existing `error!` + stderr boot-halt unchanged |
| Alerting | the Telegram alert IS the deliverable |
| Security | `reason` `html_escape`d; internal URL hidden (existing formatter) |
| Security hardening | no new attack surface; no secret in `reason` (REST `Display` redacts) |
| Bug fixing | adversarial 3-agent on the diff before + after |
| Scenarios | both boot paths + Indices4Only + NoOp notifier covered in Edge Cases |
| Functionalities | both `load_instruments` call sites wired; guard test pins it |
| Code review | 3-agent pass |
| Extreme check | source-scan guard fails build if reverted to silent `?` |

## Z+ 7-row resilience matrix

| Demand | This slice |
|---|---|
| Zero ticks lost | unaffected — boot-time path, no tick code |
| WS never disconnects | unaffected |
| Never slow/locked | unaffected — cold path, no hot-path alloc |
| QuestDB never fails | unaffected — a QuestDB reconcile failure is now *announced*, not hidden |
| O(1) latency | N/A — boot path |
| Uniqueness + dedup | unaffected |
| Real-time proof | operator gains a real-time boot-failure Telegram that did not exist |

## Honest 100% claim

100% inside the tested envelope: the boot hard-failure path now emits a
`Severity::High` operator Telegram before `main()` returns `Err`, proven by the
source-scan wiring guard (both call sites) + the existing variant formatting/
severity tests. **Caveat (no hallucination):** delivery is best-effort — `notify()`
sends via a spawned task and `main()` exit can cancel an in-flight send, identical
to the existing accepted `IpVerificationFailed` boot-block behaviour; guaranteed
flush-before-exit for all boot-block alerts is tracked separately, out of this
slice. No new metric, table, dependency, or hot-path code.

## Plan Items

- [ ] Wire `InstrumentBuildFailed` at the fast-boot `load_instruments(...).await?` site
  - Files: crates/app/src/main.rs
  - Tests: test_instrument_build_failed_wired_into_main (source-scan)
- [ ] Wire `InstrumentBuildFailed` at the standard-boot `load_instruments(...).await?` site
  - Files: crates/app/src/main.rs
  - Tests: test_instrument_build_failed_wired_into_main (covers both sites)
- [ ] Adversarial 3-agent review on the diff (hot-path N/A, security, hostile)
  - Files: (review only)
  - Tests: (none — verification step)
