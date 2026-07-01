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
| 4 | Both IP-echo endpoints unreachable | transient warn!, mismatch counter reset, no halt |
| 5 | paper/sandbox mode | monitor not spawned |
