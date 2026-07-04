# README-LOCAL — the `local-runtime` branch (Mac local-window runbook)

> **THIS FILE EXISTS ONLY ON THE `local-runtime` BRANCH.** If you can read it
> on `main`, something merged the wrong way — see "The one-way rule" below.
>
> **Operator directive 2026-07-04 (verbatim):** "we need to maintain the
> branch something like this is purely for local and it should run only on
> local... whatever the changes or codes or functionalities changes also
> everything should be merged entirely from all different branches to this
> branch alone".

## What this branch is

The permanent branch you RUN on the Mac during the local window (e.g. the
Mon–Wed window while the AWS box is paused). It is `main` + three
local-only overlay commits:

| Overlay | File | What it does |
|---|---|---|
| Feeds ON | `config/local.toml` `[feeds]` | Pins Dhan + Groww live feeds ON for the local run (dry_run stays true — NO real orders) |
| QuestDB memory | `deploy/docker/docker-compose.local.yml` | Raises the QuestDB memory ceiling to 6g on the Mac (resource limit only — no version/service changes) |
| This runbook | `README-LOCAL.md` | You are here |

## The one-way rule (NEVER merge this branch to main)

```
main ──(sync robot, every push)──▶ local-runtime     ✅ allowed, automatic
local-runtime ──▶ main                                ❌ FORBIDDEN, CI-blocked
```

- **Into this branch:** the GitHub sync robot
  (`.github/workflows/local-branch-sync.yml`) merges `main` into
  `local-runtime` after every push to main, with the message
  `chore(local): merge main into local-runtime`. You never have to rebase.
- **Out of this branch:** NEVER. The `Local-Runtime Branch Guard` job in CI
  fails any PR from `local-runtime` (or `local-runtime/*`) into main, and
  that guard is part of the merge-blocking All Green gate. If you build
  something here that is generally useful, cherry-pick it onto a normal
  feature branch and PR that.
- **If the robot hits a merge conflict:** it does NOT guess. It aborts,
  opens/updates a single GitHub issue titled `local-runtime sync conflict`
  listing the conflicting files, and goes red. You resolve on the Mac:

  ```bash
  git fetch origin
  git checkout local-runtime
  git merge origin/main     # resolve conflicts, KEEP the local overlay
  git push origin local-runtime
  ```

## ZERO-TOUCH autopilot (operator 2026-07-04: "i won't run any command")

**The ONE bootstrap step** (a Mac-side Claude session can run it for you —
after this, Mon–Wed is 100% hands-off):

```bash
make local-autopilot-install
```

That installs a launchd agent that fires `scripts/local-autopilot.sh run`
every weekday at **08:55 (Mac local time — the Mac must be on IST)**. From
then on, each morning the autopilot, entirely by itself:

1. Classifies the day: weekend / NSE holiday → quiet no-op; **Jul 6–8**
   (the scale window; `data/local-autopilot/scale-window.conf` overrides
   the dates) → scale-lab day; otherwise normal local day.
2. Preflight: on `local-runtime` + fast-forwards to origin (a conflict →
   runs the existing code + warns on Telegram), launches Docker.app if the
   daemon is down (waits up to 120s), checks ≥ 50 GB free disk, brings
   QuestDB up (6g local override) and waits until it answers.
3. Arms `caffeinate -dims` until 15:45 IST so the Mac never sleeps
   mid-session (**lid must stay OPEN** — honest caveat: a closed lid still
   sleeps most MacBooks regardless of caffeinate unless on power with an
   external display attached).
4. Scale day: boots the app with the 2×600 cap-probe overlay, then from
   09:45 IST **reads the ProbeVerdict itself** from the audit table —
   multi-conn OK → automatically restarts into the full 100K ladder;
   per-account-limited → Telegrams the answer (that IS the experiment's
   result) and continues a normal session; inconclusive / 20-min timeout →
   Telegram + normal session. Normal day: boots the branch profile
   directly.
5. All-day monitor loop: app died → one auto-relaunch with 30s backoff; a
   second death → Telegram alert with the log tail (no crash-looping).
   Docker died mid-session → one self-heal try + Telegram.
6. 15:35 IST: graceful app stop, overlay clean, end-of-day Telegram digest
   (scale stage table + app-log error count).

Watch or intervene any time: `make local-status`, the logs in
`data/local-autopilot/`, and the Telegram messages (all prefixed
"💻 LOCAL autopilot"). Remove with `make local-autopilot-uninstall`.

## Manual control (manual ALWAYS wins)

The autopilot handles everything; double-click Stop to take over manually
any time; double-click Start to resume. The autopilot never fights a
manual decision.

| Action | Double-click (repo root, no terminal) | IntelliJ (one button) | Terminal equivalent |
|---|---|---|---|
| Take over / resume | **`Start TickVault.command`** | **Run tickvault** | `make local-start` |
| Stop everything now | **`Stop TickVault.command`** | **Stop tickvault** | `make local-stop` |

**IntelliJ: ONE button does everything** (operator 2026-07-04: "just provide
everything in the fucking run tickvault"). The **Run tickvault** run config is
the FULL chain from a cold Mac — dev tools + Docker auto-setup (idempotent
ensure-ready, <1s when already provisioned) → QuestDB up with the local
override (8 GB memory, same as the scale lab) → wait for QuestDB to answer →
build + boot the app → tail the live log in the run window. Idempotent: if the
app is already running it adopts it and just tails. Stopping the run window
stops ONLY the log tail — use **Stop tickvault** for a graceful stop. The old
"Docker Restart" / "Ensure Ready" / "Run tickvault (make)" configs are removed
(folded into the one button); "Groww Scale Test" stays separate because it
applies config overrides a normal run must never apply.

- **Stop** gracefully stops the app, cleans the scale overlay, leaves
  QuestDB up, and writes `data/local-manual-stop.marker` — the autopilot
  stands down for the REST OF THE DAY (no auto-relaunch, no probe actions).
  The marker expires automatically at the next trading day, or the moment
  you Start again.
- **Start** clears the marker and is idempotent: if the autopilot already
  has the app running it detects that (shared pid file) and does NOT
  double-start — it adopts the running session and tails the live log.

During the session (both modes):

- The AWS box must be OFF/paused during the window (that is the premise of
  Dhan-on locally — no dual-instance fight). Scale-lab runs are PURELY
  GROWW — the harness flips Dhan off for the run and restores it on clean.
- Telegram messages from this run are tagged **💻 LOCAL** right after the
  severity tag (AWS runs tag **☁️ AWS**), so you always know which machine
  is talking. (The badge ships to main via the local-window PR; it reaches
  this branch through the sync robot.)
- The 15:31 IST cross-verify + digest still run automatically inside the
  app before the 15:35 stop.

## Groww 100K max-scale lab (Mon Jul 6 – Wed Jul 8)

This branch also hosts the PURELY-GROWW max-scale experiment (operator
2026-07-04): `make scale-probe` (the Monday 09:45 2×600 make-or-break) and
`make scale-max` (full master, ladder to 100 connections). During scale
runs the harness flips `dhan_enabled` OFF in place (restored by
`make scale-test-clean`) — the feeds-ON overlay above applies to the
NORMAL local window only. Full sequence + evidence locations:
`docs/runbooks/groww-scale-test.md` ("100K MAX-SCALE LAB" section).

## Where the data lives

Everything is on the Mac, under the repo:

| Data | Path |
|---|---|
| QuestDB (ticks, candles, audit tables) | Docker volume `tv-questdb-data` (inspect via `make questdb` → localhost:9000) |
| App + error logs | `data/logs/` (`app.YYYY-MM-DD.log`, `errors.jsonl.*`) |
| WAL frame spill / overflow / safety net | `data/spill/`, `data/dlq/` |
| Cross-verify CSVs | `data/cross-verify/` |
| Instrument cache + plan snapshot | `data/instrument-cache/` |

## Honest envelope

- `dry_run = true` throughout — this branch never places real orders.
- The overlay changes resource limits and feed flags only; every image,
  version, schema and code path is identical to main at the last sync.
- The one-way guard keys on the branch NAME `local-runtime` — renaming the
  branch would bypass it; don't.
