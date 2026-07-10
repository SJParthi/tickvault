# Implementation Plan: Holiday-Calendar Coverage-Horizon Staleness Guard (W2 PR #5)

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — 2026-07-09 audit follow-up row 15, coordinator-approved

## Design

The NSE holiday calendar in `config/base.toml [[trading.nse_holidays]]` lists 2026
dates only (newest: 2026-12-25). `TradingCalendar::is_holiday`
(`crates/common/src/trading_calendar.rs:147`) is a bare `HashSet::contains`, so
every 2027 weekday — including real 2027 NSE holidays — reads as a trading day the
moment the year rolls. Today the ONLY detection is two build-time ratchets
(`crates/common/tests/nse_holiday_calendar_guard.rs` +
`nse_holiday_calendar_currency_guard.rs`) that go RED on/after Jan 1 — reactive
(post-cliff, CI-only), with ZERO runtime operator signal as the cliff APPROACHES.
Separately, the NSE-endpoint cross-check helpers
(`crates/core/src/instrument/nse_holiday_cross_check.rs` — `parse_dd_mmm_yyyy`,
`compare_holiday_sets`, `HolidayDiscrepancy`) are DEAD: their Sub-PR #10b fetch
orchestrator never shipped (only the `mod` declaration in
`crates/core/src/instrument/mod.rs:166` references the module; zero production
call sites; the `nse_holiday_cross_check_enabled` config flag it documents does
not exist in `config.rs`). Wiring that check requires a NEW nseindia.com REST
fetch + cookie-warmup — gated by `no-rest-except-live-feed-2026-06-27.md`'s
edit-rule-file-first protocol — so it is flagged as a follow-up needing an
operator quote, NOT silently shipped here.

This PR ships the self-contained RUNTIME coverage-horizon guard (config-only
input, no network):

1. `crates/common/src/trading_calendar.rs` — pure staleness primitives on
   `TradingCalendar`: `coverage_end_date()` (Dec 31 of the newest listed holiday
   year — NSE circulars are per calendar year, so the calendar vouches for the
   full year of its newest entry), `coverage_days_remaining(today)` (signed;
   `None` = empty calendar = maximally stale), and free fn
   `is_calendar_coverage_stale(days_remaining, threshold)` (`None` → stale;
   `Some(d)` → `d < threshold`, strict).
2. `crates/common/src/constants.rs` — `CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS = 60`.
   Justification: NSE publishes the next calendar year's holiday circular
   typically in December; 60 days before the Dec-31 cliff means daily paging
   begins ~Nov 1 — a bounded "watch for the circular, then paste it" reminder
   window. 90 would start ~Oct 2 (2+ months of daily pages before the circular
   plausibly exists = pager fatigue); 30 (Dec 1) leaves too little slack for a
   late circular + year-end operator absence.
3. `crates/core/src/notification/events.rs` — new typed
   `NotificationEvent::HolidayCalendarCoverageLow { days_remaining, coverage_end_display }`
   at `Severity::High` (demands operator action: paste the official NSE list),
   body per the 10 Telegram commandments (emoji, plain English, IST date,
   action steps, no file paths/library names).
4. `crates/app/src/calendar_staleness.rs` — NEW module: pure decision fn
   `should_alert_today(days_remaining, threshold, today, last_alerted)` +
   `spawn_calendar_staleness_watchdog(calendar, notifier_slot)` — checks
   immediately at boot then every 6h; per-IST-date in-memory latch (Rule 4:
   max one Telegram per process per IST day); sets gauge
   `tv_calendar_coverage_days_remaining` every check; increments
   `tv_calendar_staleness_alerts_total` per fired alert; `warn!` (not `error!`
   — this is operator maintenance, not a runtime failure; no new ErrorCode, so
   no rule-file crossref burden). If stale but the lazily-filled notifier slot
   is still empty (boot prefix), retries in 30s without setting the latch.
5. Wire into `crates/app/src/main.rs` common boot prefix (after the
   `set_market_calendar_for_session` install, ~line 805) — runs on EVERY boot
   path (fast/slow/Groww-only). Boot-time check gives daily cadence on the AWS
   box (which restarts every weekday 08:30 IST and is OFF at IST midnight — a
   midnight-only task would NEVER run in prod); the 6h loop covers long-lived
   local sessions. Deliberately NOT market-hours-gated: the signal is
   date-based calendar maintenance, valid at any time (Rule 3 targets
   market-data tasks that false-page off-hours; this is neither).
6. Wiring ratchet in `crates/core/src/auth/secret_manager.rs` (house
   `test_*_is_wired_into_main` source-scan pattern).
7. Out-of-coverage gating behavior is DOCUMENTED, not changed: fail-closed
   gating on out-of-coverage dates would stop all trading every 2027 day if
   pages are missed — strictly worse than the current cost/false-open failure
   direction (per the currency guard's own analysis). The alert + the existing
   Jan-1 red-build ratchets remain the mitigations.

## Edge Cases

- Exactly N=60 days remaining → NOT stale (strict `<`); 59 → stale; 61 → not.
- today == coverage_end (Dec 31) → 0 days remaining → stale.
- today PAST coverage_end (e.g. 2027-01-02 with 2026-only calendar) → negative
  days_remaining → stale; gauge honestly reports the negative value.
- Empty holiday list → `coverage_end_date() == None` → stale (guard tests in
  crates/common/tests already prevent an empty prod config; the code path is
  still handled, never panics).
- Operator pastes the 2027 circular → newest year 2027 → coverage end
  2027-12-31 → ~380 days remaining → quiet immediately on next boot/6h tick.
- Year-boundary math: `(coverage_end - today).num_days()` on `NaiveDate` is
  timezone-free; `today` is supplied as IST wall-clock date (`today_ist()`),
  matching the calendar's IST convention — no UTC drift at the boundary.
- Restart mid-day re-arms the in-memory latch → at most one page per
  process-day; bounded by boots/day (documented honest envelope).
- Notifier slot never filled (pathological) → 30s retry loop, warn!-visible,
  no panic, no unbounded memory.
- Muhurat/mock dates deliberately EXCLUDED from the horizon (nse_holidays is
  the safety-critical gating list).

## Failure Modes

- Watchdog task dies → loop body is panic-free (pure date math + gauge +
  `ArcSwapOption::load_full` + notify; no unwrap/expect/I-O); worst case the
  next boot re-checks (daily on prod). No supervisor added — justified: no
  panic surface, and a supervisor would demand a new ErrorCode + runbook for
  a maintenance reminder.
- Telegram send fails → existing NotificationService retry/drop machinery
  (TELEGRAM-01) owns delivery; the latch is set only after `notify()` is
  invoked (fire-and-forget by design, same as every other event).
- Config parse failure → `TradingCalendar::from_config` already fails boot
  before the watchdog spawns (unchanged).
- Gauge absent when calendar empty → honest (no fake 0), alert still fires.

## Test Plan

- `crates/common/src/trading_calendar.rs` unit tests: coverage_end_date for
  2026-only calendar (== 2026-12-31); empty calendar → None; multi-year picks
  max year; days_remaining exact-boundary N/N-1/N+1 with synthetic dates
  (calendar ending 2026-12-31, today 2026-11-01/02 etc.); today==end → 0;
  past-only calendar → negative; `is_calendar_coverage_stale` truth table
  incl. None; the task-specified case: end 2026-12-31 + today 2026-11-15 →
  46 days → stale at N=60.
- `crates/app/src/calendar_staleness.rs` unit tests: `should_alert_today`
  boundaries (stale+never-alerted → true; stale+alerted-today → false;
  stale+alerted-yesterday → true; fresh → false).
- `crates/core/src/notification/events.rs` tests: severity is High; body
  carries the day count + coverage-end date; commandment-compliant (no paths).
- `crates/common/tests/calendar_coverage_horizon_guard.rs`: parses the REAL
  `config/base.toml`, builds the calendar, asserts `coverage_end_date()` is
  Some(Dec 31 of a year >= 2026) — MECHANISM + parseability pinned, NO
  hardcoded "2027 must exist" (would red the build today; NSE has not
  published 2027 — fabricating dates is banned).
- `crates/core/src/auth/secret_manager.rs`:
  `test_calendar_staleness_watchdog_is_wired_into_main` (source-scan of
  main.rs for the spawn call).
- Scoped runs: `cargo test -p tickvault-common -p tickvault-core -p tickvault-app`
  (common changed → workspace escalation per testing-scope; run workspace).

## Rollback

Single revert of the PR restores prior behavior exactly: the watchdog module,
the event variant, the two constants, and the calendar methods are additive;
no existing gating/subscription/persistence path is modified. No config
migration, no schema change, no new table. Removing the spawn line alone
(one-line change) disables the runtime alert while keeping the pure API.

## Observability

- Gauge `tv_calendar_coverage_days_remaining` (no labels) — set at boot +
  every 6h check; CloudWatch-scrapeable trend toward the cliff.
- Counter `tv_calendar_staleness_alerts_total` (no labels) — one increment per
  fired Telegram.
- `warn!` structured log line per stale check (target
  `tickvault_app::calendar_staleness`) with `days_remaining` +
  `coverage_end` fields; `info!` one-shot at spawn with the computed horizon.
- Typed `NotificationEvent::HolidayCalendarCoverageLow` → Severity::High →
  immediate Telegram via the existing severity routing; edge-latched to one
  per IST day per process.
- No new ErrorCode (no error-class condition; avoids crossref/runbook churn
  for a maintenance reminder) — decision recorded here per the error-taxonomy
  conventions.

## Plan Items

- [x] Pure staleness primitives + tests
  - Files: crates/common/src/trading_calendar.rs, crates/common/src/constants.rs
  - Tests: test_coverage_end_date_is_dec31_of_newest_year, test_coverage_days_remaining_boundaries, test_is_calendar_coverage_stale_truth_table
- [x] Typed Telegram event
  - Files: crates/core/src/notification/events.rs
  - Tests: test_holiday_calendar_coverage_low_severity_is_high, test_holiday_calendar_coverage_low_body_commandments
- [x] Watchdog module + main.rs wiring + wiring ratchet
  - Files: crates/app/src/calendar_staleness.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/core/src/auth/secret_manager.rs
  - Tests: test_should_alert_today_boundaries, test_calendar_staleness_watchdog_is_wired_into_main
- [x] Coverage-horizon mechanism guard (real config, no 2027 hardcode)
  - Files: crates/common/tests/calendar_coverage_horizon_guard.rs
  - Tests: test_base_toml_coverage_end_parses_to_dec31

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` — the
15-row + 7-row matrices apply to every item above. Item-specifics: cold-path
only (zero hot-path allocation — no DHAT/Criterion needed, not a hot path);
audit coverage N/A (no SEBI-relevant event — an operator maintenance reminder;
the structured warn! + counter are the forensic record); alert = the typed
High Telegram event itself; ratchets = the wiring guard + the horizon guard +
the boundary unit tests (build-failing on regression).
