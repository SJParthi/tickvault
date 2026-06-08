# Implementation Plan: QuestDB self-heal + fix CloudWatch log path (the "IntelliJ view" in AWS)

**Status:** VERIFIED
**Date:** 2026-06-08
**Approved by:** Parthiban — "Go ahead" (build the QuestDB self-heal + the AWS end-to-end log view).
**Crate(s) touched:** NONE — deploy + docs only (systemd unit, user-data CloudWatch-agent config,
operator-control Lambda, new runbook). No production Rust, no new ErrorCode → plan-gate exempt.

## Context

Root-caused from the live journal: the app crash-loops forever (`BOOT-02: QuestDB not ready`,
restart counter 26+) because **the QuestDB container is down and nothing on the box recreates it** —
`user-data` runs once (cloud-init), the app unit only waits for the Docker *daemon* (not the QuestDB
*container*), and the portal "Restart QuestDB" button uses `compose restart` (which fails if the
container is gone). Separately, the operator wants the AWS equivalent of IntelliJ's live log view —
but the CloudWatch agent ships `/opt/tickvault/logs/` while the app actually writes
`/opt/tickvault/data/logs/` (WorkingDirectory=/opt/tickvault + `ReadWritePaths=/opt/tickvault/data`),
so **app logs never reach CloudWatch** — a real path-mismatch bug.

## Design

1. **Self-heal QuestDB at app boot** — `deploy/systemd/tickvault.service`: add
   `ExecStartPre=-/usr/bin/docker compose -f /opt/tickvault/repo/deploy/docker/docker-compose.yml up -d questdb`.
   Runs as ec2-user (in the docker group), recreates a missing/stopped QuestDB before the app starts.
   `-` prefix = a transient compose hiccup doesn't hard-fail the unit; the app's own 60s
   `wait_for_questdb_ready` remains the real gate. Fixes the "container gone → loop forever" class.
2. **Restart-QuestDB button → create-or-restart** — `operator-control/handler.py`: change
   `docker compose restart questdb` → `docker compose up -d questdb` so the portal button works even
   when the container vanished (not just when it's merely stopped).
3. **Fix CloudWatch log path (the IntelliJ view)** — `user-data.sh.tftpl` CloudWatch-agent
   `collect_list`: `/opt/tickvault/logs/{errors.jsonl*,app.*.log}` →
   `/opt/tickvault/data/logs/...` (where the app actually writes). This makes the full app log +
   `errors.jsonl` (incl. the `BOOT-02` `error!`) stream to CloudWatch `/tickvault/<env>/app` →
   CloudWatch **Live Tail** becomes the real-time end-to-end console. Also fix the `mkdir` to create
   `data/logs`.
4. **New runbook** `docs/runbooks/aws-docker-daemon-dead.md` — disk-full / QuestDB-down / Docker
   recovery (referenced by `aws-daily-lifecycle.md`); documents the self-heal + the journald-vs-
   CloudWatch split (systemd/boot lines = journald/portal Logs tab; app+error logs = CloudWatch).

**Honest scope note:** journald (the raw systemd `Failed to start` + the final anyhow `Error:` line)
is NOT shipped to CloudWatch by the agent (it tails files, not the journal). But the *root-cause*
`BOOT-02` line IS emitted via `error!` → `errors.jsonl` → reaches CloudWatch after fix #3, so the
operator sees WHY boot failed in CloudWatch. Raw systemd lines stay in journald (portal Logs tab).
A journald→CloudWatch source is deliberately out of scope (fragile; the path fix covers the need).

**Deploy caveat (documented in the runbook):** the systemd-unit + user-data changes take effect on
the next deploy / instance refresh — the existing down box must first be recovered manually
(free disk / `docker compose up -d`).

## Edge Cases

- ExecStartPre under `ProtectSystem=strict` — only reads the compose file (/opt readable) + talks to
  `/run/docker.sock` (/run not locked by strict); ec2-user is in the docker group. Works.
- Disk genuinely full → ExecStartPre `up -d` still can't start QuestDB → app still waits/fails
  (correct — can't conjure space); the runbook covers freeing disk / growing EBS.
- `up -d questdb` is idempotent — a healthy running QuestDB is untouched.

## Failure Modes

- All changes are deploy/docs. ExecStartPre uses `-` so it never blocks boot worse than today.
  Lambda change is a one-word command swap. user-data change is a path string.

## Test Plan

- `python3 -c "import json; ..."` validate the embedded CloudWatch-agent JSON still parses.
- `python3 -m py_compile deploy/aws/lambda/operator-control/handler.py` — Lambda still compiles.
- `cargo test -p tickvault-common --test runbook_cross_link_guard` — new runbook has no dangling links.
- Manual review of the systemd unit (no syntax tool; matches existing directive style).

## Rollback

- `git revert` restores the old systemd unit, the `compose restart`, and the old CW path. Pure
  deploy/docs revert; no runtime code affected.

## Observability

- After deploy: CloudWatch `/tickvault/<env>/app` Live Tail shows the full app log + errors (incl.
  BOOT-02). `tv_disk_*` gauges + the existing 75%-full alarm already cover disk.

## Plan Items

- [x] Item 1 — systemd `ExecStartPre` self-heals QuestDB
  - Files: `deploy/systemd/tickvault.service`
  - Tests: N/A (TEST-EXEMPT systemd unit — no Rust)
- [x] Item 2 — Restart-QuestDB button → `compose up -d`
  - Files: `deploy/aws/lambda/operator-control/handler.py`
  - Tests: N/A (TEST-EXEMPT Lambda — py_compile verified)
- [x] Item 3 — fix CloudWatch agent log path → `/opt/tickvault/data/logs/`
  - Files: `deploy/aws/terraform/user-data.sh.tftpl`
  - Tests: N/A (TEST-EXEMPT Terraform template — JSON validated)
- [x] Item 4 — new `aws-docker-daemon-dead.md` runbook
  - Files: `docs/runbooks/aws-docker-daemon-dead.md`
  - Tests: every_repo_path_referenced_in_runbooks_resolves

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB container removed, app starts | ExecStartPre `up -d` recreates it → app boots (no more loop) |
| 2 | Operator presses Restart QuestDB with container gone | `up -d` recreates it (old `restart` would fail) |
| 3 | App running, operator opens CloudWatch Live Tail | full app log + BOOT/errors stream live (IntelliJ-style) |
| 4 | Disk full | self-heal can't help; runbook → free disk / grow EBS |
