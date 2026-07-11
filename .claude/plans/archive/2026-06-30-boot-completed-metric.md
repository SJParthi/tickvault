# Implementation Plan: Real `tv_boot_completed` metric + repoint boot-heartbeat alarm

**Status:** APPROVED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — follow-up explicitly flagged by PR #1278 ("emit a dedicated `tv_boot_completed` metric + add to the CW-agent filter", `daily-universe-scope-expansion-2026-05-27.md` §19).

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.
> Scope is intentionally narrow: ONE boot-success metric emit + the CloudWatch
> scrape filter + the alarm repoint. No `dry_run`, budget, instance-lock, 429,
> WAL-spill, `MAX_DAILY_UNIVERSE_SIZE`, or `SEAL_BUFFER_CAPACITY` change.

## Design

PR #1278 added `deploy/aws/terraform/boot-heartbeat-alarm.tf`, which pages when
the app hung / never booted in the 08:50–09:10 IST window. Because the ideal
signal `tv_boot_completed` did not exist (no Rust emitter, not in the CloudWatch
scrape filter), #1278 alarmed on a PROXY (`tv_realtime_guarantee_score`, the 10s
SLO gauge) and explicitly flagged the dedicated-metric follow-up.

This change makes the ideal signal real:

1. Emit `metrics::gauge!("tv_boot_completed").set(1.0)` ONCE at the genuine
   boot-completion point in `crates/app/src/main.rs` — immediately after the
   `if/else` "boot sequence completed" block and alongside
   `infra::notify_systemd_ready()`. This point is reached ONLY when every boot
   gate has passed: a halt uses `std::process::exit(...)` or `anyhow::bail!`/`?`
   propagation, neither of which reaches this line, so a wedged/halted boot
   NEVER emits it (missing = page — exactly the intent).
   `metrics-exporter-prometheus` persists a gauge's last value on every `/metrics`
   scrape, so the one-shot `set(1.0)` keeps appearing in every scrape while the
   process is alive (same established pattern as `tv_instrument_load_duration_ms`,
   set once at boot) and disappears only when the process exits — which is the
   precise "missing → breaching" semantic the alarm needs.

2. Add `tv_boot_completed` to the CloudWatch-agent `metric_declaration` filter in
   `deploy/aws/terraform/user-data.sh.tftpl` (both `label_matcher` and
   `metric_selectors` regex) so the metric is actually shipped to CloudWatch.

3. Repoint the alarm in `boot-heartbeat-alarm.tf`: `metric_name` from
   `tv_realtime_guarantee_score` → `tv_boot_completed`. Keep
   `treat_missing_data = "breaching"`, the `LessThanThreshold`/`threshold=0`/
   `statistic="Maximum"` math (a `1.0` gauge never satisfies `<0`, so
   present=OK / missing=breaching), the 08:50–09:10 IST enable-window Lambda, and
   all SNS routing intact. Update the comments/description/output to note it is
   now the dedicated boot signal.

## Edge Cases

- Boot over the timeout budget but still completed: the emit is placed AFTER the
  whole `if (over budget) {...} else {...}` block, so BOTH completed-boot cases
  (over and under budget) emit exactly once.
- Halt / early `bail!` / `process::exit` before the emit: the line is never
  reached, so no datapoint → alarm pages (intended).
- Nightly/weekend intentional stop: the metric goes missing (process exits), but
  the enable-window Lambda keeps the alarm's actions OFF outside 08:50–09:10 IST
  Mon–Fri, so the stop never false-pages. Unchanged from #1278.
- SLO feature disabled: the new signal does NOT depend on the SLO feature gate
  (unlike the proxy), so this is a strict improvement — the boot proof now fires
  regardless of the `realtime_guarantee_score` feature flag.

## Failure Modes

- CloudWatch agent does not scrape the metric → caught by step 2 (filter add) and
  by the adversarial self-review check "metric is in the scrape filter".
- Emit accidentally placed on a non-success path → caught by the source-scan
  guard test asserting it sits after `notify_systemd_ready()` /
  `"boot sequence completed"` and that `tv_boot_completed` appears exactly once.
- Metric-name string drift → the guard test pins the exact `tv_boot_completed`
  literal in main.rs AND the terraform filter.

## Test Plan

- New source-scan guard `crates/app/tests/boot_completed_metric_guard.rs`:
  (a) `main.rs` contains `metrics::gauge!("tv_boot_completed")` exactly once and
  it appears after the `notify_systemd_ready()` boot-complete marker;
  (b) the literal `tv_boot_completed` appears in
  `deploy/aws/terraform/user-data.sh.tftpl` (scrape filter) and in
  `deploy/aws/terraform/boot-heartbeat-alarm.tf` `metric_name`;
  (c) the alarm no longer uses `tv_realtime_guarantee_score` as its `metric_name`.
- `cargo test -p tickvault-app` (paste `test result:` lines).
- `bash .claude/hooks/banned-pattern-scanner.sh`, `bash .claude/hooks/plan-gate.sh "$PWD"`,
  `bash .claude/hooks/plan-verify.sh`.
- `bash -n` not applicable (terraform/HCL); `terraform fmt -check` if available.

## Rollback

Single-commit revert restores the proxy alarm: revert the `metric_name` line in
`boot-heartbeat-alarm.tf`, drop `tv_boot_completed` from the filter, and remove
the one-line emit + guard test. No data migration, no schema, no stateful change.

## Observability

The change IS observability: it adds a real CloudWatch metric `tv_boot_completed`
(gauge, value 1.0 on a completed boot, MISSING on a hung/halted boot) and points
the existing boot-heartbeat alarm at it. The metric flows
`metrics::gauge!` → Prometheus exporter `/metrics` → CloudWatch-agent EMF
processor → `Tickvault/Prod` namespace → `tv-<env>-boot-heartbeat-missing` alarm
→ SNS. Touched crate: `tickvault-app` (`crates/app/src/main.rs`).
