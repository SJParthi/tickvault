# Implementation Plan: robust QuestDB self-heal (fix #1052 docker-compose crash-loop)

**Status:** APPROVED
**Date:** 2026-06-08
**Approved by:** Parthiban — live incident; "everything needs to be easy/accessible" + deploy-now directive.
**Crate(s) touched:** NONE — deploy + scripts + Lambda only. No production Rust, no new ErrorCode → plan-gate exempt.

## Context

Live AWS box crash-looped on BOOT-02 every ~63s (60s `wait_for_questdb_ready` + 3s `RestartSec`).
Root cause = PR #1052's QuestDB self-heal `ExecStartPre=-/usr/bin/docker compose -f ... up -d questdb`
had THREE bugs, all proven from the journal (`tickvault[PID]` printing Docker's top-level help):
1. This instance has no working `docker compose` v2 plugin → the command is a no-op (prints help).
2. The compose SERVICE name is `tv-questdb`, NOT `questdb` → even with a plugin it errors "no such service".
3. A recreate needs `QDB_PG_USER`/`QDB_PG_PASSWORD` from SSM, absent from the systemd unit env.
The operator's portal "Wipe data" (docker-reset) used plain `docker rm`/`volume rm` (worked → QuestDB
DELETED) then `docker compose up -d` (no-op → never rebuilt), so QuestDB is gone and the app loops.
The same broken `docker compose` is in the portal Restart-QuestDB + docker-reset Lambda paths.

## Design

1. **NEW `scripts/ensure-questdb.sh`** — idempotent, robust bring-up of `tv-questdb`:
   running → no-op; stopped → `docker start` (reuses env, no creds); removed → fetch SSM
   `/tickvault/<env>/questdb/pg-{user,password}`, then recreate via `docker compose` (v2) →
   `docker-compose` (v1) → compose-plugin-by-absolute-path → LAST-RESORT `docker run` a faithful
   tv-questdb (host-published ports, volume, shm, mem, PG creds). Exit 0 up / 1 failed.
2. **`deploy/systemd/tickvault.service`** — ExecStartPre now calls the helper
   (`-/bin/bash /opt/tickvault/repo/scripts/ensure-questdb.sh`) instead of the broken compose line.
   Leading `-` preserved (non-fatal); app's 60s wait stays the real gate.
3. **`deploy/aws/lambda/operator-control/handler.py`** — `restart-questdb` + docker-reset step 7 call
   the helper instead of `docker compose up -d questdb` / `docker compose up -d`, so the portal
   buttons actually work on this box.

deploy-aws.yml already installs the compose plugin (line ~495) on deploy, so the helper's compose
path will normally succeed post-deploy; the docker-run fallback is belt-and-suspenders. The repo is
git-reset to origin/main on the box during deploy, so the new script lands before the unit runs.

## Edge Cases

- Container running → helper no-ops (fast `docker inspect` check).
- Container stopped (not removed) → `docker start` reuses original env; no SSM call needed.
- Container removed + SSM creds missing → helper logs WARN and refuses to `docker run` without auth
  (exit 1) rather than start QuestDB with default creds the app can't use.
- No compose at all → docker-run fallback (host-port access; no compose network needed since the app
  runs on the host and reaches QuestDB at 127.0.0.1:9009/8812/9000).
- Re-running after a docker-run recreate: `docker inspect` sees it running → no-op (idempotent).

## Failure Modes

- SSM unreachable during recreate → WARN + exit 1; app keeps looping (visible, not silent) until SSM
  recovers — same fail-visible posture as before, but now with a clear log line.
- A future `docker compose up` could conflict with a docker-run-created container name; acceptable
  emergency-recovery tradeoff (documented in the helper header).

## Test Plan

- `bash -n scripts/ensure-questdb.sh` (syntax) — PASS.
- `python3 -m py_compile deploy/aws/lambda/operator-control/handler.py` — PASS.
- Live verification post-deploy: app boots (no BOOT-02 loop), `tv-questdb` Up healthy, NTM ~743.
- No unit tests (deploy/scripts/Lambda only; no Rust touched).

## Rollback

`git revert` the commit — restores the prior unit + Lambda. The new script is additive (safe to keep).
No schema/data/wire change.

## Observability

- Helper echoes the method used (`already running` / `started existing` / `recreated via …` /
  `FAILED …`) to stdout → journald (and the deploy SSM command output), so the operator sees exactly
  how QuestDB came up. App BOOT-01/02 codes unchanged.

## Per-item guarantee matrix

All 15 "100% everything" rows + 7 resilience rows from
`.claude/rules/project/per-wave-guarantee-matrix.md` apply to every item in this
plan. This is a deploy/scripts/Lambda-only hotfix (no Rust/wire/schema change):
the Rust-specific rows (DHAT, Criterion, coverage) are satisfied by "no
production Rust touched"; the resilience rows are satisfied by "no new
tick-drop path; QuestDB self-heal strengthened (recovers from removed/stopped/
no-compose states); idempotent; git-revert-clean rollback".

## Plan Items

- [x] Add robust `scripts/ensure-questdb.sh`
  - Files: scripts/ensure-questdb.sh
  - Tests: bash -n (syntax)
- [x] Point systemd ExecStartPre at the helper
  - Files: deploy/systemd/tickvault.service
  - Tests: grep ExecStartPre
- [x] Point Lambda restart-questdb + docker-reset at the helper
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: python3 -m py_compile
