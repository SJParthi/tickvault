# Implementation Plan: Operator portal 3-tab REDESIGN — Overview / Data / Admin

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — GO given this day; verbatim complaint: "webpage looks completely messy, too many buttons".
**Branch:** `claude/charming-newton-qy6j0i`
**Changed area:** deploy-only (`deploy/aws/lambda/operator-control/handler.py` + `test_handler.py` + `README.md`). No `crates/` code.
**Guarantee matrices:** cross-referenced per `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row matrices apply; deploy-only rows are N/A'd honestly in the PR body).

---

## Design

UI reorganization ONLY — zero changes to action semantics, guards, confirm tokens, or SSM
command logic. The single-file Lambda portal collapses from 7 tabs
(Overview / Data / DB / GitHub / Logs / AWS / Latency) to THREE:

1. **Overview** — keeps: ticks-today hero, instance/app/market/peak badges, refresh +
   auto-refresh, live ticks/sec sparkline, guarantees shields, FEEDS toggles. Adds:
   (a) a thin AWS strip (spend $ · alarms firing · disk used %) sourced from the existing
   `aws_status` action, wrapped in a click-to-expand `<details>` revealing the alarm list +
   storage shields; (b) a compact Latency card reusing the existing `latency` action +
   `latnet`/`latproc` renderers behind the "⚡ Measure now" button; (c) a single stopped-box
   banner ("Box stopped (auto-stops 16:30 IST, auto-starts 08:30 Mon–Fri) — guarantees
   resume on start") replacing the scary per-shield "unreachable" scatter, with the shields
   rendered greyed as "—" ONLY when `instance_state != running`; (d) ONE context-aware
   instance button ("▶ Start instance" when stopped / "■ Stop instance" when running) with
   its own force checkbox — the server-side market-hours guard is untouched.
2. **Data** — candles-sealed bars + cross-verify card + the read-only DB console from
   PR #1326 folded in (tables list, query box, grid, CSV download). The old duplicate
   simple SQL box is removed — the DB console's query box IS the SQL query box (same `sql`
   action, same server-side gate).
3. **Admin** — Restart app, Restart QuestDB, Stop app, force checkbox, a **collapsed**
   `<details id="danger">` danger zone with ONE severity radio picker (Wipe GROWW only →
   Wipe ALL data → Full Docker reset → Bare Nuke), each keeping its OWN typed confirm token
   (GROWW / WIPE / NUKE / ERASE) and honest description lines, one "Execute" button
   dispatching to the UNCHANGED existing JS functions/endpoints, plus "Lock / forget this
   device".

Backend deltas: the Logs + GitHub TABS are deleted (UI + now-unused JS: `loadLogs`,
`loadGithub`, `ghMerge`, `ghDeploy`, `ciBadge`, `runSql`). The `logs` API route is KEPT
(the tickvault-logs MCP server's `cloudwatch_logs` tool POSTs `{"action":"logs"}` to the
portal — verified caller at `scripts/mcp-servers/tickvault-logs/server.py:1127`). The
`gh_prs`/`gh_merge`/`gh_deploy` routes have NO caller outside the deleted tab → removed,
along with the dead `_gh`/`_gh_open_prs`/`_github_token` helpers and `GH_*` env reads
(Terraform may still SET those env vars — the Lambda now ignores them; IAM/terraform
cleanup is a follow-up, not this PR).

## Plan Items

- [x] 3-tab HTML restructure (Overview/Data/Admin), Logs+GitHub tabs deleted
  - Files: deploy/aws/lambda/operator-control/handler.py (`_console_html`)
  - Tests: test_html_has_exactly_three_tabs, test_logs_and_github_tabs_absent
- [x] Overview: AWS strip + expandable details, compact latency card, stopped-box banner +
      greyed shields, context-aware instance button
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_overview_has_aws_strip_and_latency_card, test_stopped_box_banner_and_grey_shields,
    test_context_aware_instance_button
- [x] Data: DB console folded in; Admin: collapsed danger zone + severity picker + lock
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_db_console_folded_into_data_tab, test_danger_zone_collapsed_by_default,
    test_severity_picker_maps_each_choice_to_action_and_token
- [x] Dead gh_* routes + helpers removed; `logs` route kept for the MCP server
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_gh_actions_now_unknown, test_logs_route_kept_for_mcp_server
- [x] Tests updated + suite green
  - Files: deploy/aws/lambda/operator-control/test_handler.py
  - Tests: `python3 -m unittest discover deploy/aws/lambda/operator-control`

## Edge Cases

- Box STOPPED: `view` returns `instance_state=stopped` + empty snapshot → banner shows,
  shields grey "—", instance button flips to "▶ Start instance". No fake failures.
- Box RUNNING but QuestDB/app degraded: the EXISTING shield logic (unreachable/amber,
  DEDUP disabled/red, schema drift/amber) renders unchanged — the grey path is gated
  strictly on `instance_state !== 'running'` so real failures are never hidden.
- Danger zone with NO radio selected → Execute toasts "pick an action" and dispatches
  nothing.
- Each danger action retains its own typed-token prompt (GROWW/WIPE/NUKE/ERASE) — the
  picker only chooses WHICH prompt fires; a wrong/cancelled token still aborts.
- `aws_status` partial failure (CloudWatch throttle → `alarms_firing: null`) renders "?"
  in the strip, matching the old AWS tab semantics.
- MCP server log reads: `{"action":"logs"}` route unchanged, verified by test.

## Failure Modes

- A JS handler referencing a removed element would throw at runtime → mitigated by
  removing ALL dead JS (`runSql`, `loadLogs`, `loadGithub`, `ghMerge`, `ghDeploy`,
  `ciBadge`) and by tests asserting every remaining onclick target function exists.
- pollNuke / command-status flow must survive the DOM move → functions untouched; tests
  keep asserting pollNuke + command-status + honest outcome markers.
- Removing gh_* routes could break an unseen caller → grepped the whole repo; only the
  deleted GitHub tab called them. The MCP-used `logs` route is kept + pinned by test.

## Test Plan

`python3 -m unittest discover deploy/aws/lambda/operator-control` — all pre-existing
tests (updated where they asserted the old tab structure) + new tests: three tabs only;
logs/github tabs absent; gh_* actions return 400 unknown; `logs` action still routed;
danger zone `<details>` collapsed by default; severity picker maps each radio value to
the correct function + typed token; context-aware start/stop button; stopped-box banner
+ greyed shields gated on non-running state; AWS strip + latency card in Overview; DB
console folded into Data.

## Rollback

Single squash commit on a deploy-only path — `git revert <sha>` restores the 7-tab
portal byte-identically. No schema, no SSM params, no IAM, no app code touched.

## Observability

The portal is itself an observability surface; this PR only reorganizes it. All action
routes, guards, and honest-outcome markers (WIPE-COMPLETE/PARTIAL, GROWW-WIPE-*,
DOCKER-RESET-FAILED, bare-nuke-*) are unchanged and still test-pinned. Lambda-side
`print()` diagnostics to CloudWatch Logs unchanged.
