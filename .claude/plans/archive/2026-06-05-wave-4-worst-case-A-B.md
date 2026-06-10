# Wave 4 Worst-Case Catalog — Categories A + B

> **Authority:** companion to `.claude/plans/active-plan-wave-4.md`
> **Scope:** Wave-4-E1 sub-PR enumerates every scenario below into a
> chaos test + ErrorCode + ratchet. SCOPE block in active-plan-wave-4.md
> summarizes; this file is the canonical enumeration.

## Category A — Process / Container failures (7 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| A1 | OOM-killer kills tickvault (cgroup memory limit hit) | `memory.events::oom_kill` increment vs baseline | systemd / Docker restart policy `unless-stopped`; rescue ring re-replays via spill on next boot | PROC-01 | `chaos_oom_kill_recovery.rs` |
| A2 | Container restart loop (5+ restarts in 1h) | `docker inspect` RestartCount delta | Boot-time guard: refuse to start if peer container in restart loop; Telegram CRITICAL | PROC-02 | `chaos_container_restart_loop.rs` |
| A3 | Process panic in non-main thread (e.g. WS read loop) | `tokio::spawn` JoinError | Pool supervisor (WS-GAP-05) respawns within 5s | (existing WS-GAP-05) | `chaos_ws_task_panic.rs` |
| A4 | Process panic in main task (boot sequence) | systemd / Docker exit code != 0 | Boot probes (BOOT-01/02) catch and HALT cleanly with Telegram | (existing BOOT-01/02) | covered by existing |
| A5 | Docker daemon dies | `dockerd` SIGKILL | Out of scope — operator action required (page via SNS SMS) | n/a | manual runbook |
| A6 | Container image SHA changed underneath running app | Boot-time `image_audit` row vs current `/proc/1/cgroup` | Telegram WARN; persist drift in `app_image_audit` table | PROC-03 (reserved, not in stub) | `chaos_app_image_drift.rs` |
| A7 | Tickvault deployed twice (rolling deploy collision) | `live_instance_lock` row < 60s old at boot | Refuse to start; second instance HALTs | RESILIENCE-01 (E3) | `chaos_dual_instance.rs` |

## Category B — Network failures (4 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| B1 | Static IP changes mid-session (EIP reassociated) | `ip_monitor` 60s poll diff vs boot baseline | Mid-market: HALT + page (Dhan rejects orders); off-hours: WARN | NET-01 | `chaos_ip_change_mid_session.rs` |
| B2 | DNS resolution fails for Dhan endpoints | 3 consecutive failures within 60s | Exponential backoff; if all 5 endpoints fail, HALT | NET-02 | `chaos_dns_resolution_storm.rs` |
| B3 | TLS handshake fails (cert chain rejection) | `aws-lc-rs` rejects server cert | HALT + page (suggests upstream cert rotation or our trust store stale) | (covered by reqwest error log) | `chaos_tls_handshake_failure.rs` |
| B4 | Network partition (loss of all egress) | Watchdog: 0 bytes received from any Dhan endpoint for 120s | Same as Scenario 6 in disaster-recovery.md (all 5 WS connections drop) — sleep-until-open | (existing WS-GAP-04/05) | `chaos_network_partition.rs` |

## What this file does NOT cover

- AWS-side networking (NAT GW failure, VPC endpoint outage) — out of
  scope until AWS deployment lands
- IPv6 transition — not in operator's roadmap
- Kernel-level network bugs — operator runbook only

## Trigger

This file activates when editing:
- `crates/app/src/oom_monitor.rs` (new)
- `crates/core/src/network/ip_monitor.rs` (Wave-4-E1 wiring)
- Any chaos test under `crates/storage/tests/chaos_*.rs` matching A1–A7 / B1–B4
