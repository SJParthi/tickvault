---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/app/src/infra.rs"
  - "crates/common/src/constants.rs"
  - "crates/core/src/notification/events.rs"
---

# Wave 2-C Error Codes

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Wave 2-C hardening implementation (Item 7).
> Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## BOOT-03 — wall-clock skew vs trusted source exceeded threshold — HALTING

**Trigger:** the boot-time clock-skew probe (`crates/app/src/infra.rs`
function `probe_clock_skew`) sampled the local wall-clock vs a trusted
upstream source (PRIMARY = `chronyc tracking` shell-out; FALLBACK =
QuestDB `SELECT now()` over PG-wire) and observed a skew of magnitude
≥ `CLOCK_SKEW_HALT_THRESHOLD_SECS` (= 2.0s, pinned in
`crates/common/src/constants.rs`). The app HALTS. Severity::Critical.

**⚠ CORRECTED 2026-10-11 (audit CLOCK-3):** there is no FALLBACK any more.
QuestDB runs on this host and shares its kernel clock, so `SELECT now()`
read about 0 skew whatever the real drift was, and a stopped chronyd
passed this gate and fed `tv_clock_skew_seconds{source="questdb_now"}` to
the `clock-skew-high` alarm. Now, when `chronyc` cannot answer, the probe
returns Unavailable, boot proceeds with a BOOT-03 `warn!` (not an
`error!`, so it does not page), no skew number is written, and the local
gauge `tv_clock_skew_probe_available` reads 0. Honest limit: while chronyc
is unavailable the CloudWatch gauge keeps its last chronyc value, and the
alarm treats missing data as not breaching. Step 2 of the triage below is
kept as history; a container shares the host clock, so it never helps.

**Why this is a HALT, not a warn:**
IST timestamp math + DEDUP UPSERT keys depend on accurate wall-clock.
A ±2s skew can cross IST midnight, the 09:00:00 market-open gate, or
the 15:30:00 close gate — silently splitting or merging trading days
in QuestDB. Once a partition is written under the wrong day, SEBI
reconstruction is unreliable, and DEDUP keys keyed on `trading_date_ist`
(per `previous_close`, `phase2_audit`, etc.) become non-idempotent on
replay. Refusing to boot is cheaper than recovering from a bad day.

**Triage:**
1. `chronyc tracking` on the host — is `Last offset` ≥ 2.0s? If so,
   chrony is desynced from its NTP source. Run `chronyc -a 'burst 4/4'`
   and `chronyc -a makestep` to force a step adjustment.
2. `date -u` vs `docker exec tv-questdb date -u` — does the QuestDB
   container's wall-clock disagree? If yes, restart Docker so the
   container picks up the corrected host clock.
3. After remediation, restart the app — the boot probe will pass on
   skew < 2.0s and the app will start normally.

**Do NOT:**
- Silently raise `CLOCK_SKEW_HALT_THRESHOLD_SECS` to "make the warning
  go away". A higher threshold means we admit timestamp corruption.
- Disable the probe. There is no safe way to run the trading pipeline
  with a drifting clock. Fix the host instead.

**Source:**
- Constant pin: `crates/common/src/constants.rs::CLOCK_SKEW_HALT_THRESHOLD_SECS`
- Probe function: `crates/app/src/infra.rs::probe_clock_skew`
- Notification event:
  `crates/core/src/notification/events.rs::NotificationEvent::BootClockSkewExceeded`
- Severity assignment: `crates/common/src/error_code.rs` — `Boot03ClockSkewExceeded => Severity::Critical`
- Cross-ref runbook target: this file
- Ratchet tests:
  - `test_boot_probe_emits_critical_on_clock_skew_gt_2s`
  - `test_boot_probe_passes_at_skew_below_threshold`
  - `test_clock_skew_threshold_constant_is_2s`
  - `test_boot_clock_skew_event_is_critical_severity`
