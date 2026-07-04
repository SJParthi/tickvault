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

## Daily run (Mon–Wed local window)

Pre-flight (evening before or ~08:30 IST):

```bash
git checkout local-runtime && git pull      # sync robot keeps it fresh
docker compose -f deploy/docker/docker-compose.yml \
               -f deploy/docker/docker-compose.local.yml up -d   # QuestDB with the 6g local ceiling
# (plain `make docker-up` also works; add QDB_MEM_LIMIT=6g to match)
make run                                    # boots the app (pretty logs)
make doctor                                 # 7-section health check — all green before 09:00
```

During the session:

- **Keep the Mac AWAKE 09:00–15:30 IST.** System Settings → prevent sleep
  on power, or `caffeinate -dims` in a spare terminal. A sleeping Mac drops
  the WebSockets and the local window loses ticks.
- The AWS box must be OFF/paused during the window (that is the premise of
  Dhan-on locally — no dual-instance fight).
- Telegram messages from this run are tagged **💻 LOCAL** right after the
  severity tag (AWS runs tag **☁️ AWS**), so you always know which machine
  is talking. (The badge ships to main via the local-window PR; it reaches
  this branch through the sync robot.)

After the close (15:31 IST cross-verify + digest run automatically):

```bash
make stop            # stop the app when you're done for the day
```

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
