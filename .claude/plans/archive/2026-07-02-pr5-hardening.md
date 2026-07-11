# Implementation Plan: PR-5 — destructive-action server tokens, wipe/autostart race, rotation-spanning tail drain

**Status:** VERIFIED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — standing fix-sequence directive ("once it gets merged go ahead with the plan always"); PR-5 of the adversarial-sweep queue (the 3 MEDIUMs from the 2026-07-02 4-agent verification), started on #1328's merge.

> Guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row 100% matrix + 7-row resilience matrix apply to every item below; the
> honest-100% envelope wording per operator-charter §F is adopted verbatim).

## Design

**H-1 — server-side confirm tokens for the 3 legacy destructive Lambda
actions (security MEDIUM).** `wipe-groww` already 409s unless the caller
sends `{"confirm": "GROWW"}` server-side; the older `wipe-questdb` /
`docker-reset` / `docker-nuke-bare` rely ONLY on a browser `prompt()` — a
scripted call with a stolen bearer fires them with zero friction. Fix:
mirror the wipe-groww pattern — each action 409s unless the payload carries
its own token (`WIPE` / `NUKE` / `ERASE`); the portal JS (which already
prompts for exactly these words) forwards the typed word in the payload.
Defense-in-depth on top of bearer + market-hours + box-running gates.

**H-2 — wipe-vs-autostart race + prev_day_ohlcv (hostile F5).** In
`wipe-questdb`, an external `systemctl start tickvault` (deploy watchdog /
autopilot / concurrent deploy) landing between the TRUNCATE step and the
replay-source `rm -rf` lets the booting bridge re-tail the Groww capture
file from byte 0 and resurrect rows — the exact 2026-07-02 incident class.
Fix (two independent layers): (a) `systemctl disable` immediately after
stop and re-enable just before the final start, so no boot/agent path
auto-starts mid-wipe; (b) REORDER — delete the replay sources (WAL, groww
capture, spill, dlq, caches) BEFORE the TRUNCATE, so even a forced start
mid-window finds nothing to replay (a few seconds of fresh live ticks then
truncated is acceptable inside an operator-initiated FULL wipe). Also add
`prev_day_ohlcv` to the dynamic truncate targets ("fresh start" previously
left it populated).

**H-3 — rotation-spanning downtime tail drain (hostile F4, Rust bridge).**
If the bridge is down across IST midnight, the sidecar archives
`live-ticks.ndjson` → `live-ticks-YYYYMMDD.ndjson`; the bridge only tails
the base path, so bytes appended after the bridge's last flush but before
rotation were archived UNREAD and deleted after 2 days — contradicting the
"rows are in QuestDB" retention assumption. Fix: on resume, when the
persisted snapshot's `ist_day` is a PREVIOUS day, look for that day's
archive; if it exists and is longer than the flushed offset, drain the
archive's tail ONCE through the SAME parse/validate/persist path (offset
temporarily pointed at the archive; capture_seq restored; DEDUP-idempotent),
then byte-0 the live file as today. Bounded: the drain loop is capped at
`GROWW_ARCHIVE_DRAIN_MAX_WAKES` iterations × 4 MiB chunks; archive absent /
already-drained → exact current behavior.

## Edge Cases
- H-1: missing token → 409 with the exact re-send hint; wrong token → 409;
  token with surrounding whitespace → trimmed (same as GROWW).
- H-2: `systemctl disable` fails → `|| true` keeps the wipe going (layer b
  still protects); trap still restarts+re-enables on SSM kill.
- H-3: archive deleted by retention (≥2 days down) → byte-0 live (old
  behavior); archive shorter than offset (rotated twice / wiped) → skip
  drain; snapshot from TODAY → normal live resume, no archive involved;
  drain cap hit → warn + proceed (bounded boot).

## Failure Modes
- H-1 regression (token check removed) → unit test 409-pins each action.
- H-2 regression (order flipped back / disable dropped) → source-order
  ratchet test pins rm-before-truncate + disable-before-truncate.
- H-3: archive drain errors are non-fatal (warn + byte-0 live) — boot never
  blocks on a forensic replay.

## Test Plan
- Lambda: 409-without-token + 409-wrong-token + 200-with-token for each of
  the 3 actions (mock SSM); source-order ratchet for H-2; prev_day_ohlcv in
  the truncate filter; client JS sends each token.
- Bridge: pure `archive_path_for_day` + `should_drain_archive` decision
  tests; drain-cap constant pinned.
- `cargo test -p tickvault-app --lib groww_bridge` + Lambda unittest green.

## Rollback
`git revert` — no schema change; the archive drain only reads files.

## Observability
- H-1: 409s appear in Lambda CloudWatch logs (existing print diagnostics).
- H-3: `tv_groww_archive_tail_drained_total` counter + info! with the
  archive path and bytes drained; warn! on cap hit.

## Plan Items
- [x] H-1 server-side confirm tokens (WIPE/NUKE/ERASE) + client payloads
  - Files: deploy/aws/lambda/operator-control/handler.py, deploy/aws/lambda/operator-control/test_handler.py
  - Tests: test_legacy_destructive_actions_require_server_side_tokens
- [x] H-2 wipe reorder (rm-before-truncate) + disable window + prev_day_ohlcv
  - Files: deploy/aws/lambda/operator-control/handler.py, deploy/aws/lambda/operator-control/test_handler.py
  - Tests: test_wipe_questdb_removes_replay_sources_before_truncate_and_disables_unit
- [x] H-3 rotation-spanning archive tail drain on resume
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_archive_path_for_ist_day_matches_sidecar_rotation, test_should_drain_archive_decision
- [x] Archive the merged PR-4 plan
  - Files: .claude/plans/archive/2026-07-02-pr4-mediums.md
  - Tests: (chore)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | curl with stolen bearer fires wipe-questdb without token | 409, nothing dispatched |
| 2 | Deploy watchdog starts the app mid-wipe | Nothing to replay (sources already deleted) + unit disabled |
| 3 | Bridge down 22:00→09:00 across midnight | Yesterday's archived tail drained into QuestDB, then live file from byte 0 |
| 4 | Bridge down 3 days (archive retention passed) | byte-0 live (old behavior), warn logged |
