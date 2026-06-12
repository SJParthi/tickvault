# Plan: wire the BootHealthCheck safety ping (defined-but-never-fired alert)

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban (2026-06-12 — "go ahead with the PRs", "don't ask me, do everything
automated"; "working?" = go. Trigger semantics operator-delegated.)
**Branch:** claude/nice-cerf-hemuvn
**Changed crates:** `app` (extract a pure container-health parser; add `container_health_counts()`;
emit `BootHealthCheck` once after boot infra is ensured)

## Why
`NotificationEvent::BootHealthCheck { services_healthy, services_total }` (Severity::Low) is defined
+ tested in `events.rs` but has **0 production emit sites** (verified by grep) — a "defined but never
called = bug" gap (audit-findings Rule 13). The operator wants a positive boot-complete health ping so
"did the box come up clean at 08:30 IST?" has an affirmative signal (audit-findings Rule 11: positive
progress signal, not just absence-of-error). This wires it.

## Design
1. Extract a **pure** parser `parse_container_health(ps_stdout: &str) -> ContainerHealth { total,
   healthy }` from `check_and_restart_containers` (which today inlines the `docker compose ps` line
   parse). Reuse it in `check_and_restart_containers` (no behaviour change) AND a new
   `pub async fn container_health_counts() -> (usize, usize)` that runs `docker compose ps` once and
   returns `(healthy, total)`.
2. In `main.rs`, immediately after `infra::ensure_infra_running(&config.questdb).await` at the boot
   call site (the `notifier = fast_notifier.clone()` is already in scope), call
   `container_health_counts()` and `notifier.notify(NotificationEvent::BootHealthCheck { services_healthy,
   services_total })` — ONCE per boot. Low severity → informational Telegram, no page.

## Edge Cases
- Docker daemon down → `docker compose ps` fails → parser sees empty → `(0, 0)`; BootHealthCheck still
  fires with 0/0 (honest "nothing healthy"), not suppressed (the operator wants to KNOW).
- All healthy → `(N, N)`; fires N/N.
- This is boot-path, runs ONCE — not a loop, no edge-trigger/market-hours concern (boot is the event).

## Failure Modes
- `docker compose ps` errors → counts `(0,0)`, logged `warn!` (pre-existing pattern), ping still fires.
- notifier is None (tests) → no emit; production always has the strict notifier by this point.

## Test Plan
- `app`: `parse_container_health` unit tests — all-running, mixed, empty, header-only, malformed lines.
- `app`: source-scan guard `boot_health_check_wired_guard.rs` — `main.rs` emits
  `NotificationEvent::BootHealthCheck` after `ensure_infra_running`, and `container_health_counts` exists.
- `cargo test -p tickvault-app`; fmt + clippy(-D warnings) + banned-pattern + pub-fn guards; 3-agent review.

## Rollback
Additive: one new pure fn + one new async fn + one emit call. No signature change to
`ensure_infra_running`. Revert = `git revert`. No data path touched.

## Observability
- Layer 5 (Telegram): `BootHealthCheck` (Low) — the positive boot signal, now actually fires.
- Layer 1: reuses existing `tv_docker_containers_total` / `tv_docker_containers_healthy` gauges.
- No new counter/table.

## Per-Item Guarantee Matrix
Carries the 15+7 matrix by reference — `.claude/rules/project/per-wave-guarantee-matrix.md`. Specifics:
100% functionality (a defined-but-dead alert now wired + tested — closes the Rule-13 gap); pure parser
unit-tested; honest envelope: this is a boot-time positive ping, not a continuous monitor.

## Plan Items
- [x] Extract pure `parse_container_health` + add `container_health_counts()`; reuse in `check_and_restart_containers`
  - Files: crates/app/src/infra.rs
  - Tests: parse_container_health unit tests
- [x] Emit `BootHealthCheck` once after `ensure_infra_running` at the boot call site
  - Files: crates/app/src/main.rs
  - Tests: boot_health_check_wired_guard (source-scan)

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | boot, all containers running | BootHealthCheck N/N (Low) Telegram |
| 2 | boot, one container down | BootHealthCheck (N-1)/N |
| 3 | docker daemon down at boot | BootHealthCheck 0/0 (honest, still fires) |
