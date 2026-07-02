# Implementation Plan: AUTH-P12 — wire the runtime static-IP / EIP-change monitor

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — standing approval for the permutation-coverage audit fix queue (`.claude/plans/permutation-coverage-audit-2026-07-01.md` §"Fix queue", PR-1 AUTH-P12).
**Branch:** `claude/harden-ip-monitor-wiring`
**Changed crates:** `core` (crates/core), `app` (crates/app)

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row, mandatory
> per `per-item-guarantee-check.sh`). Dominant guarantee here: **strictly no worse
> than today** — purely additive detection; boot-time IP verify unchanged; no
> hot-path (tick) code touched; the monitor is a 5-min-cadence cold-path task.

## Design

**Gap (AUTH-P12).** `crates/core/src/network/ip_monitor.rs::spawn_ip_monitor` is
defined + fully unit-tested but has ZERO production call site — every caller is
`#[cfg(test)]`. `crates/app/src/main.rs` wires ONLY boot-time IP verification
(`verify_public_ip` at Step 5.5 + `verify_static_ip_at_boot` at Step 6a). A
mid-session public-IP / Elastic-IP change (EIP disassociation, NAT failover) is
therefore never re-detected. `docs/architecture/guarantees.md:118` falsely cites
the never-spawned detector as the proof artefact for "Static IP enforcement".

**Fix.** Spawn `spawn_ip_monitor` from `run_dhan_lane` Step 5.5 (live mode only,
where the boot `verify_public_ip` already runs and returns the verified egress
IP). The monitor polls the public IP every `IP_MONITOR_CHECK_INTERVAL_SECS`
against that boot-verified IP.

**Halt-vs-alert decision (audit-mandated, NOT a blind halt).**
On a *confirmed sustained* mismatch (confirm-twice debounce — a single transient
metadata blip does not act):
- Always emit `error!(code = GAP-NET-01)` CRITICAL (routes to Telegram via the
  5-sink ERROR pipeline).
- HALT the process (`std::process::exit(1)`) ONLY when `dry_run == false` — real
  orders would be rejected by Dhan's static-IP mandate, so blinding-then-halting
  is the safe outcome.
- When `dry_run == true` (current state, ~3 months no real orders) DO NOT halt —
  killing a working live feed for a no-orders IP change would drop ticks for zero
  financial benefit. Alert only; keep streaming.

The decision is a **pure function** `decide_ip_action(consecutive_mismatches,
threshold, halt_on_mismatch) -> IpMonitorAction` so it is exhaustively
unit-testable with no I/O.

**Debounce.** The monitor tracks `consecutive_mismatches`; a `Match` or
`CheckFailed` (network blip — indistinguishable from EIP-detach, handled as
transient per existing `warn!`) resets the counter. Action fires only when
`consecutive_mismatches >= IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD` (= 2). Reuses
the monitor's existing 5-minute polling loop, so a confirmed mismatch is ≥ 2 poll
cycles of the SAME wrong IP — never a one-shot flap.

**Market-hours awareness (audit-findings Rule 3).** An EIP/IP change is a
host-infrastructure fact true 24/7 (not live-data-driven) and it breaks SSM +
Dhan reachability whenever it happens — so detection itself is NOT market-hours
gated (a weekend EIP detach must still be seen). The *halt* is gated by `dry_run`
(a wrong IP is equally fatal for orders whenever real trading resumes), not by
market hours. The alert is edge-triggered (fires once per confirmed episode;
counter resets on recovery) — no spam (audit-findings Rule 4).

**Lifetime.** Mirror the seal-writer pattern (main.rs:4153): create a
`watch::channel(false)` shutdown handle, `std::mem::forget` the sender for
process lifetime, hand the receiver to `spawn_ip_monitor`.

## Edge Cases

- **`dry_run == true` (today):** confirmed mismatch → alert only, NO halt, feed
  keeps running. `decide_ip_action(2, 2, false) == AlertOnly`.
- **`dry_run == false` (future live):** confirmed mismatch → alert + halt.
  `decide_ip_action(2, 2, true) == AlertAndHalt`.
- **Single transient mismatch (1 poll):** below threshold → Continue (no action,
  no alert). `decide_ip_action(1, 2, _) == Continue`.
- **`CheckFailed` (both IP-echo endpoints unreachable — e.g. EIP fully detached):**
  transient `warn!`, resets the consecutive-mismatch counter. AUTH-P13
  (stranded-box escalation) is OUT OF SCOPE for this PR — tracked separately in
  the audit at medium.
- **Non-live mode (paper/sandbox):** monitor is NOT spawned (no Dhan orders, no
  static-IP mandate) — same gating as the existing `verify_public_ip` Step 5.5.
- **Empty / disabled config:** existing `spawn_ip_monitor` guard exits the task
  immediately (already tested).

## Failure Modes

- **False halt on a flap:** prevented by confirm-twice debounce (≥ 2 poll cycles
  of the same wrong IP).
- **Silent dead-code regression:** prevented by a source-scan wiring guard test
  asserting `spawn_ip_monitor(` appears in `crates/app/src/main.rs`.
- **IP-echo service outage misread as EIP change:** `CheckFailed` is handled as
  transient and resets the counter, so an echo outage never triggers halt.
- **Monitor task panic:** the task has no `unwrap`/`expect` on the poll path; a
  panic kills only the monitor, never the feed. Supervised-respawn is a possible
  follow-on but not required for this gap (boot-time gate remains primary).

## Test Plan

Unit (crates/core, `ip_monitor::tests`):
- `test_decide_ip_action_below_threshold_continues`
- `test_decide_ip_action_at_threshold_dry_run_alerts_only`
- `test_decide_ip_action_at_threshold_live_alerts_and_halts`
- `test_decide_ip_action_above_threshold_still_acts`
- `test_ip_monitor_mismatch_confirm_threshold_is_two`

Wiring guard (crates/app, `ip_monitor_wiring_guard.rs`):
- `test_spawn_ip_monitor_has_production_call_site` — source-scan asserts
  `spawn_ip_monitor(` is present in `crates/app/src/main.rs` (mirrors
  `health_counter_fix7_guard.rs`), so it can never regress to dead code.
- `test_ip_monitor_halt_is_dry_run_gated` — source-scan asserts the wiring reads
  `dry_run` to decide halt-vs-alert.

Verify: `cargo check -p tickvault-core -p tickvault-app` then
`cargo test -p tickvault-core -p tickvault-app --lib` green.

## Rollback

Single-commit PR on a feature branch. Revert = `git revert <sha>`; the monitor is
purely additive (a new spawned task + a new pure fn + two new constants + a guard
test). Removing it returns to the prior boot-only IP-verify behaviour with zero
data-path change. No schema change, no config-format change, no migration.

## Observability

- `error!(code = "GAP-NET-01", severity = "…")` on confirmed mismatch — routes to
  Telegram via the 5-sink ERROR pipeline (existing GAP-NET-01 runbook:
  `.claude/rules/dhan/authentication.md`, `docs/runbooks/README.md:40`).
- The existing `spawn_ip_monitor` `info!("GAP-NET-01: IP monitoring started")`
  positive-signal log confirms the monitor is live at boot.
- Per-poll `IpCheckResult` logging (Match / Mismatch / CheckFailed) already exists;
  no new Prometheus counter is strictly required (the ERROR-with-code is the
  operator-actionable signal).

## Plan Items

- [x] Item 1 — Add `IP_MONITOR_CHECK_INTERVAL_SECS` + `IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD` constants and a pure `decide_ip_action` + `IpMonitorAction` to `ip_monitor.rs`; wire the debounce + dry_run-gated halt into the `spawn_ip_monitor` loop.
  - Files: crates/core/src/network/ip_monitor.rs
  - Tests: test_decide_ip_action_below_threshold_continues, test_decide_ip_action_at_threshold_dry_run_alerts_only, test_decide_ip_action_at_threshold_live_alerts_and_halts, test_decide_ip_action_above_threshold_still_acts, test_ip_monitor_mismatch_confirm_threshold_is_two

- [x] Item 2 — Spawn the monitor from `run_dhan_lane` Step 5.5 (live mode) using the boot-verified IP + `!config.strategy.dry_run` as the halt gate.
  - Files: crates/app/src/main.rs
  - Tests: (covered by wiring guard in Item 3)

- [x] Item 3 — Add the source-scan wiring guard.
  - Files: crates/app/tests/ip_monitor_wiring_guard.rs
  - Tests: test_spawn_ip_monitor_has_production_call_site, test_ip_monitor_halt_is_dry_run_gated

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | dry_run=true, IP changes for 2 poll cycles | CRITICAL Telegram + error!(GAP-NET-01); feed keeps running (no halt) |
| 2 | dry_run=false, IP changes for 2 poll cycles | CRITICAL Telegram + error!; process halts (orders would be rejected) |
| 3 | IP flaps for 1 poll cycle then recovers | No action, no alert (below confirm threshold) |
| 4 | Both IP-echo endpoints unreachable | transient warn!, mismatch counter reset, no halt; AUTH-P13 CheckFailed streak escalates to CRITICAL past threshold |
| 5 | paper/sandbox mode | monitor not spawned |

---

# Implementation Plan: CCL-06 — stop silently dropping Muhurat-session ticks

**Status:** VERIFIED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — audit fix-queue PR-3, `.claude/plans/permutation-coverage-audit-2026-07-01.md` §140/§175

> **Guarantee matrices:** carried by cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory per `per-item-guarantee-check.sh`). All 15 rows of the 100% Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to every item in this plan.

## Design

**The bug (CCL-06, high):** `crates/app/src/main.rs` sets
`should_connect_ws = subscription_plan.is_some() && (is_trading || is_mock_trading || is_muhurat)`
(main.rs ~5744) so on a Diwali Muhurat day the live feed **connects** — but the
tick-processor persist gate `is_within_persist_window` (tick_processor.rs:119) is
hardcoded to `[09:00, 15:30)` IST with NO Muhurat branch. A Muhurat evening tick
(~18:15 IST) fails the range → `continue` (tick_processor.rs:1146/1418) → dropped
before persist/seal/broadcast. The whole ~1h Muhurat session stores ZERO data
despite a live connection. This is the audit Rule 11 "connect-and-drop-everything"
false-OK — the worst of both states.

**The fix (config-independent, constant-window, boot-gated — mirrors the §30
`always_on` GIFT-Nifty exemption precedent):**

1. Add a Muhurat persist window as named constants in `crates/common/src/constants.rs`:
   `MUHURAT_PERSIST_START_SECS_OF_DAY_IST = 64_800` (18:00 IST) and
   `MUHURAT_PERSIST_END_SECS_OF_DAY_IST = 70_200` (19:30 IST). This is a deliberate
   **superset** of the historical announced NSE Muhurat windows (2023 was 18:15–19:15,
   2024 was 18:00–19:00), so a slightly-early/late tick is still captured. Compile-time
   asserts pin start<end, both within a day, and start >= the regular window end
   (disjoint from + after `[09:00, 15:30)`).

2. Add a `crates/common/src/muhurat.rs` process-global (mirror of `always_on.rs`):
   `init_muhurat_session(active: bool)` (OnceLock, first-call-wins) + `current() -> bool`.
   Boot calls `init_muhurat_session(is_muhurat)` ONCE after the calendar loads; every
   spawn site reads `current()` to obtain the flag it passes EXPLICITLY into
   `run_tick_processor`. Default (never-init) = `false` → today's behaviour byte-for-byte.

3. Widen the two persist-window free functions in `tick_processor.rs` to take an explicit
   `muhurat_active: bool`. When `true`, the accepted set becomes
   `[09:00,15:30) ∪ [18:00,19:30)` (exchange-ts gate) and
   `[09:00,15:31) ∪ [18:00,19:30)` (wall-clock gate — Muhurat has no post-close grace
   tail since 19:30 is already generous). When `false`, byte-identical to today. The
   hot loop reads `tickvault_common::muhurat::current()` ONCE before the loop (O(1),
   zero-alloc, `bool` Copy, exactly like `always_on`) and passes the bool into every
   gate call.

**Why constants, not config:** the audit offered "config-driven muhurat_open/muhurat_close
OR a MUHURAT_*_SECS const set" — the const set is chosen because (a) it avoids
`TradingConfig` Deserialize-schema churn and its round-trip guards, (b) it keeps every
gate a pure unit-testable function, (c) the superset window makes the exact
year-to-year announced time irrelevant for capture. The window can be widened later via
a config follow-up without touching this contract.

**Why widening can't regress a normal day:** `is_muhurat` is `false` on every
trading/mock day, so `muhurat_active=false` and the accepted set is exactly `[09:00,15:30)`
as today. The Muhurat branch is only ever active on the ~1 day/yr Muhurat date.

## Edge Cases

- Normal trading day (`is_muhurat=false`): both gates identical to today — pinned by
  keeping ALL existing `test_persist_window_*` tests green (updated to pass `false`).
- Muhurat tick at 18:15 IST with `muhurat_active=true`: accepted (the new ratchet
  `test_muhurat_tick_1815_ist_is_persisted`).
- Muhurat tick at 18:15 IST with `muhurat_active=false`: still rejected (proves the flag
  actually gates it — no accidental always-on widening).
- Boundary 18:00:00 inclusive / 19:30:00 exclusive (half-open, matches the regular window's
  half-open contract).
- 17:59:59 (before Muhurat open) and 19:30:00 (at Muhurat close) rejected even when active.
- `muhurat::current()` before init → `false` (never-boot / unit-test / Indices4Only scope).
- always-on (`window_exempt`) instruments are unaffected — they still bypass BOTH gates
  regardless of Muhurat.

## Failure Modes

- Muhurat flag never set at boot → `current()` returns `false` → Muhurat ticks dropped
  (degrades to today's behaviour, never a crash). Acceptable: it is exactly the
  pre-fix state, not a regression.
- Config carries a Muhurat DATE but NSE cancels the session → feed connects, no ticks
  arrive in the window → nothing persisted (correct — no false data).
- Zero hot-path allocation added; one `bool` Copy read before the loop. No new panic
  paths (no unwrap/expect/println).

## Test Plan

Unit (truth-table, in `tick_processor.rs::tests` — no live box):
- `test_muhurat_tick_1815_ist_is_persisted` — 18:15 IST + `muhurat_active=true` → accepted.
- `test_muhurat_tick_1815_ist_dropped_when_flag_off` — 18:15 + `muhurat_active=false` → dropped.
- `test_muhurat_window_boundaries` — 18:00:00 in, 17:59:59 out, 19:29:59 in, 19:30:00 out (active).
- `test_regular_window_unchanged_when_muhurat_active` — 12:00 IST accepted, 08:00/16:00 dropped even with `muhurat_active=true` (regular window still additive).
- `test_muhurat_wall_clock_1815_accepted_when_active` — wall-clock gate symmetric.
- All existing `test_persist_window_*` updated to pass `false` and stay green.
In `crates/common/src/muhurat.rs::tests`:
- `current_before_init_is_false_then_reflects_init`.
Constants (`constants.rs::tests`):
- `test_muhurat_window_constants_pinned` (18:00/19:30, start<end, disjoint from regular).

Meta-guards run pre-push: `per_item_guarantee_matrix_guard` (storage), `aws_infra_wiring`
(common), `error_code_rule_file_crossref` + `error_code_tag_guard` (common — touched crate),
`per-item-guarantee-check.sh`, `plan-verify.sh`, `banned-pattern-scanner.sh`,
`pub-fn-test-guard.sh`, `pub-fn-wiring-guard.sh`.

## Rollback

Pure additive + gated-by-`false`-default change. Revert the single commit → the
`muhurat` module + constants + the `muhurat_active` param disappear; `should_connect_ws`
is untouched, so behaviour returns exactly to `34f0a970`. No schema migration, no data
migration, no config change required. `git revert <sha>` is complete + safe.

## Observability

- No new ErrorCode/metric needed for the happy path. The existing
  `tv_outside_hours_filtered_total` counter still increments for genuinely-out-of-window
  ticks; Muhurat ticks now simply pass the gate and flow through the existing
  persist/seal/broadcast + their existing counters (`tv_ticks_processed_total`, etc.),
  so a Muhurat session becomes visible in the SAME dashboards/tables as any session —
  which is the whole point (kills the false-OK dark hole).
- Boot already logs `is_muhurat_session` (main.rs:1343) — operator sees the flag at boot.

## Plan Items

- [x] Item 1 — Add Muhurat persist-window constants + compile-time asserts + unit test.
  - Files: crates/common/src/constants.rs
  - Tests: test_muhurat_window_constants_pinned

- [x] Item 2 — Add `muhurat` process-global module (mirror of `always_on`) + wire `pub mod muhurat;`.
  - Files: crates/common/src/muhurat.rs, crates/common/src/lib.rs
  - Tests: current_before_init_is_false_then_reflects_init

- [x] Item 3 — Widen the two persist-window gates to accept the Muhurat window when active; read the boot flag once in the hot loop; add ratchet tests.
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_muhurat_tick_1815_ist_is_persisted, test_muhurat_tick_1815_ist_dropped_when_flag_off, test_muhurat_window_boundaries, test_regular_window_unchanged_when_muhurat_active, test_muhurat_wall_clock_1815_accepted_when_active

- [x] Item 4 — Boot: call `init_muhurat_session(is_muhurat)` once after calendar load.
  - Files: crates/app/src/main.rs
  - Tests: (wiring — covered by pub-fn-wiring-guard; boot is TEST-EXEMPT live path)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Normal trading day, 12:00 IST tick | Persisted (unchanged) |
| 2 | Normal trading day, 18:15 IST tick | Dropped (is_muhurat=false; unchanged) |
| 3 | Muhurat day, 18:15 IST tick | Persisted (the fix) |
| 4 | Muhurat day, 12:00 IST tick | Persisted (regular window still additive) |
| 5 | Muhurat day, 17:59:59 IST tick | Dropped (before Muhurat open) |
| 6 | Muhurat day, 19:30:00 IST tick | Dropped (Muhurat close exclusive) |
| 7 | Boot without calendar (Indices4Only/test) | muhurat::current()=false → today's behaviour |
