# Groww Auto-Scale Test — Mac Runbook (§34, PR-3)

> **Authorization:** `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §34
> (operator, 2026-07-03; Mac-first execution confirmed).
> **Error codes:** `.claude/rules/project/groww-scale-error-codes.md`
> (GROWW-SCALE-01..04).
> **Plan:** `.claude/plans/active-plan-groww-autoscale.md`.

## What this test does

Grows the Groww sidecar fleet up the ladder (default rungs `1 → 2 → 5 → 10`
connections, each holding ≤ 1,000 instruments) with every advance gated on
15 minutes of green health, automatic rollback to the last healthy rung on
any failure, and a 5-minute cooldown + halve on fleet-wide failure.
Everything runs on THIS Mac — local QuestDB, local app, local capture files.

**PROD IS UNTOUCHED.** No SSH, no terraform, no deploy, no AWS write. The
ONLY AWS interaction is the read-only SSM read of the Groww token
(`groww-shared-token-minter-2026-07-02.md` — TickVault never mints).

## Prerequisites (one-time)

1. Docker Desktop running.
2. AWS credentials on the Mac with the `groww-token-minter-reader-tickvault`
   read permission (the same setup the single-conn Groww feed already uses).
3. Python 3 available (`python3 --version`) — the sidecar venv is
   auto-provisioned by the supervisor.

## The exact commands

| Goal | Command |
|---|---|
| Full ladder run (trading day) | `make scale-test` |
| 2×600 cap-probe (per-conn vs per-account verdict) | `make scale-probe` |
| Weekend / holiday machinery validation | `make scale-smoke` |
| Remove the harness config overlay afterwards | `make scale-test-clean` |

Each command: writes a marker-delimited overlay block into
`config/local.toml` (Dhan OFF, Groww ON, scale ON + the mode flags), starts
the local QuestDB container, then runs the app in release mode. `Ctrl-C`
stops it; `make scale-test-clean` removes the overlay (content outside the
markers is never touched).

IntelliJ: the run configuration **"Groww Scale Test"** (`.run/Groww Scale
Test.run.xml`) runs `make scale-test` in a terminal tab.

## What happens at boot (in order)

1. **PREFLIGHT** (`scale_test_preflight.rs`): shards dir writable, disk
   headroom ≥ 20%, QuestDB ILP + HTTP reachable — plus the
   PROD-IS-UNTOUCHED banner. Any FAIL → the fleet does NOT spawn; the
   single-connection Groww path runs instead (capture continues). Fix the
   named check and restart.
2. **Watch set + shard cut**: the daily watch set is cut into contiguous
   per-connection shards; each conn gets its own directory
   `data/groww/shards/cNN/` (watch file, tick NDJSON, status file).
3. **Fleet + ladder**: sidecar children spawn to the ladder's desired
   count; the ladder evaluates every 30s and climbs only when every gate
   holds for `gate_hold_minutes` (default 15) inside the advance window
   (default 09:20–14:30 IST).

## Watching it live

| What | Where |
|---|---|
| Per-connection health rows | `curl -s http://127.0.0.1:3001/api/feeds/health \| jq .groww_scale` |
| Ladder state + counts | gauges `tv_groww_ladder_state`, `tv_groww_conns_desired`, `tv_groww_conns_target` on `/metrics` |
| Rollbacks | `tv_groww_scale_rollbacks_total{reason}` + Telegram-routed `GROWW-SCALE-01/02` log lines |
| Forensic rung history | `select * from groww_scale_audit order by ts desc` (QuestDB console, `make questdb`) |
| Per-conn capture files | `ls -la data/groww/shards/c*/` |

## Reading the probe verdict (`make scale-probe`)

The probe forces exactly **2 connections × 600 instruments** (1,200 total —
deliberately above the documented 1,000 per-session cap) and prints ONE
verdict line when the run reaches a terminal state:

| Verdict label | Meaning | Next step |
|---|---|---|
| `probe_multi_conn_ok` | Both conns captured → the cap is PER-CONNECTION; multi-conn works | proceed with the ladder (`make scale-test`) |
| `probe_per_account_limited` | Conn 0 captured, conn 1 got nothing → per-ACCOUNT limit | STOP laddering; record with a dated note per §34.2 before any tier work |
| `probe_inconclusive` | Conn 0 itself captured nothing → infra/auth problem, not a limit signal | fix the base feed first (token, entitlement, network) |
| `probe_smoke_machinery_validated` | Market was closed → machinery validated only | re-run during market hours for the real answer |

The verdict is also written to `groww_scale_audit` with
`outcome = 'probe_verdict'` and the label in the `reason` column.

## Weekend SMOKE mode (`make scale-smoke`)

When the market is CLOSED, the normal ladder freezes (no probing credit
accrues off-hours). SMOKE mode instead exercises the FULL machinery —
shard cut, per-conn watch files, fleet spawn, rung climbing — with the
tick-dependent gates honestly SKIPPED (no live market ⇒ no ticks by
design, never a failure). Auth rejection and host CPU/disk gates stay
REAL. Every outcome is labelled SMOKE so a machinery-validated run is
never mistaken for a live validation. While the market is open,
`weekend_smoke` has no effect.

## Auto-correction cheat sheet (what fixes itself)

| Failure | Auto-correction | Code |
|---|---|---|
| A rung's new conns fail | rollback to last healthy rung + expo hold (10m→4h) | GROWW-SCALE-01 |
| ALL conns fail within 60s | 5-min cooldown + halve the count | GROWW-SCALE-02 |
| One conn goes silent mid-market | kill + relaunch just that conn | FEED-STALL-01 |
| App restarts mid-ladder | resumes at the last VERIFIED-healthy rung from the audit table | (rehydration) |
| Shard overlap detected | ladder step HALTS fail-closed (cutter bug — should be unreachable) | GROWW-SCALE-03 |
| Audit row write fails | ladder continues; row re-appends next transition | GROWW-SCALE-04 |

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the
ladder never advances through a failing gate, auto-rolls back to the last
VERIFIED-healthy rung, halves + cools down on global failure, and resumes
the last verified rung after a restart. NOT claimed: Groww's server-side
per-account/per-connection limits — discovering them IS this experiment;
a persistent provider reject surfaces as a repeating GROWW-SCALE-02, which
is the honest signal, not a silent workaround.
