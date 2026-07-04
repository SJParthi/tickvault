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
| **100K MAX-SCALE LAB** (local-runtime branch; full master, ladder to 100 conns) | `make scale-max` |
| Max-scale config, weekend SMOKE (machinery-only dry run) | `make scale-max-smoke` |
| Remove the harness config overlay afterwards | `make scale-test-clean` |

`DRY_RUN=1` before any command writes/show the overlay without starting
Docker or the app (harness validation on any machine).

**Composition with the local-runtime branch overlay:** that branch pins its
own `[feeds]` table in `config/local.toml` (Dhan + Groww ON for the normal
local window — see `README-LOCAL.md`). TOML forbids a duplicate `[feeds]`
table, so during scale runs the harness overrides the keys IN PLACE
(`dhan_enabled = false # scale-override(was: true)`) — the run is PURELY
GROWW per the operator's 2026-07-04 directive — and `make scale-test-clean`
restores the original values exactly.

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

## 100K MAX-SCALE LAB (local-runtime branch — operator 2026-07-04)

> **Authorization (verbatim, 2026-07-04):** "check the dynamic scaling
> connections and subscribing entirely the 100k instruments... the max to
> check whether we can fetch the entire 100k or not" — LOCAL-ONLY, on the
> `local-runtime` branch (never merges to main; main's scope locks are
> untouched). Operator update the same day: the experiment is **PURELY
> GROWW** — Dhan stays OFF for the whole run (the overlay writes
> `dhan_enabled = false`).

What `make scale-max` changes vs the normal ladder:

| Knob | Normal ladder | Max-scale lab |
|---|---|---|
| Watch set | daily indices+NTM (~767, [100,1200] envelope) | **FULL Groww master** (~100K rows → shard-cuttable entries) |
| Ladder rungs (connections) | `[1, 2, 5, 10]` | `[1, 2, 5, 10, 20, 40, 80, 100]` |
| Target | 10 conns | 100 conns × 1000/conn = 100K |
| Memory gate | 85% used | **75% used** (~36 GB of the M4 Pro's 48 GB — macOS keeps headroom) |
| QuestDB container memory | 4g | 8g |

The FSM itself is unchanged — the shipped probe-first pace applies: each
rung must hold every gate green for 15 minutes inside the advance window
before the next rung fires, so the earliest 100-conn arrival is ~7 stages
after the first verified rung.

**Fail-closed "subscribe what exists":** if the master yields fewer
entries than 100K, the ladder's ceiling clamps to the shard count, halts
at ceiling there, and the ACTUAL count is recorded (audit rows + the
stage-summary TSV). If the full-master download fails 3 times, the run
degrades LOUDLY to the daily ~767 set (summary rows carry
`full_master_fetch_failed` in the `degraded` column) — capture continues,
the 100K question just stays unanswered that day.

### Mac hardware prerequisites (operator's MacBook Pro, M4 Pro, 14c/48 GB)

- **≥ 50 GB free SSD** before starting — ~100K SIDs of NDJSON capture +
  QuestDB grows fast; the disk gate AUTO-ABORTS (rolls the fleet back) if
  free space drops below 20% of the volume.
- Memory watermark 75%: the ladder rolls back BEFORE the Mac becomes
  unusable (~36 GB used). CPU gate 70% of the 14 cores. Both probes are
  live on macOS (`sysctl vm.loadavg` / `vm_stat`) — no longer skipped.

### Monday 2026-07-06 sequence (IST)

| Time | Action | Why |
|---|---|---|
| 09:15 | Normal boot NOT needed for the lab — but if the Mac is already running the normal Groww profile, `Ctrl-C` it first | one app instance at a time |
| 09:45 | `make scale-probe` — the **make-or-break** 2×600 probe | answers per-CONNECTION vs per-ACCOUNT before any big ladder run; 09:45 avoids the open burst |
| ~10:05 | Read the ProbeVerdict line (also in `groww_scale_audit`, `outcome='probe_verdict'`) | see the verdict table above |
| verdict `probe_multi_conn_ok` | `Ctrl-C`, then `make scale-max` | full ladder toward 100K; advance window 09:20–14:30 |
| verdict `probe_per_account_limited` | STOP — do NOT run `scale-max`; record with a dated note | the account streams one session; laddering would just storm GROWW-SCALE-02 |
| verdict `probe_inconclusive` | fix the base feed (token/entitlement/network) and re-probe | not a limit signal |
| through the day | watch `GET /api/feeds/health .groww_scale` + `data/groww-scale/summary-<date>.tsv` | per-stage evidence accrues automatically |
| any time | `Ctrl-C` then `make scale-test-clean` | full stop + overlay removal |

Tuesday: read the evidence (below) and decide whether Wednesday re-runs a
tuned ladder.

### Where the evidence lands (the Tuesday table)

| Evidence | Where |
|---|---|
| Per-stage summary rows: `ts | outcome | conns | target | ceiling | subscribe_proof | capturing | tick_bytes | cpu% | mem% | disk_free% | degraded` | `data/groww-scale/summary-<date>.tsv` (+ mirrored as `info!` log lines) |
| Every rung transition (forensic) | `groww_scale_audit` table — `select * from groww_scale_audit order by ts` |
| Live per-conn panel | `curl -s http://127.0.0.1:3001/api/feeds/health \| jq .groww_scale` |
| Ticks/sec per stage (derive, never synthesized) | QuestDB: `select count() from ticks where feed = 'groww' and ts > dateadd('m', -1, now())` |
| Per-conn capture files | `ls -la data/groww/shards/c*/` |

### Auto-abort semantics (what stops the ladder by itself)

| Trigger | Action | Signal |
|---|---|---|
| Memory used ≥ 75% of 48 GB | rollback to last healthy rung + expo hold | `GROWW-SCALE-01`, reason `mem_high` |
| CPU load ≥ 70% of 14 cores | same | reason `cpu_high` |
| Disk free ≤ 20% of the volume | same | reason `disk_low` |
| Capture lag > 30s / bridge behind > 4 MiB | same | reason `capture_lag` / `bridge_behind` |
| ALL conns fail within 60s (provider throttle) | 5-min cooldown + HALVE the fleet | `GROWW-SCALE-02` |
| Repeated failure at the same rung | exponential hold `10m → 4h`, retry below the discovered ceiling | audit rows show the discovered cap |

Manual stop at any moment: `Ctrl-C` (the app), then `make scale-test-clean`
(removes the overlay so the next boot is the normal single-conn profile).

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: the
ladder never advances through a failing gate, auto-rolls back to the last
VERIFIED-healthy rung, halves + cools down on global failure, and resumes
the last verified rung after a restart. NOT claimed: Groww's server-side
per-account/per-connection limits — discovering them IS this experiment;
a persistent provider reject surfaces as a repeating GROWW-SCALE-02, which
is the honest signal, not a silent workaround.

Max-scale lab additions to the envelope: whether Groww streams anything
close to 100K instruments to one account is UNKNOWN until the live Monday
run — the ladder discovers and holds below whatever the provider allows;
the sidecar's Python hop is not O(1) (existing §32 envelope); the summary
TSV is a best-effort forensic side-record (the audit table + gauges are
primary). The 75%/70%/20% Mac gates bound the blast radius on the host —
they do not make the provider accept more connections.
