# Implementation Plan: M3 reality-sync — retire dead depth auto-fix pair + honest milestone status

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (standing directive 2026-06-10: "once it gets merged go ahead with the plan always" — continuing `.claude/plans/autonomous-operations-100pct.md` after M2 merged via #1083; this is the autonomous-SAFE slice of M3. The architecturally significant remainder — re-introducing mutating HTTP endpoints — is explicitly DEFERRED to an operator decision, documented in the milestone doc update below.)
**Authority:** `.claude/plans/autonomous-operations-100pct.md` Milestone 3.

**Per-item guarantee matrix:** cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` per its "or cross-reference it" clause. Deltas: shell scripts + one meta-guard test file + plan docs only; no production crate `src/` change, no hot path, no new pub fn, no dep, no schema.

## Design

**Measured baseline (2026-06-10 audit):** M3 is further along AND further behind
than the plan doc says. Already shipped by prior sessions: 7 fix + 7 rollback
script pairs (all `--dry-run`-capable, all audit-logging to
`data/logs/auto-fix.log`), `scripts/triage/verify.sh` + `rollback.sh` (M4
dispatchers), and meta-guard `crates/common/tests/autonomous_ops_m3_m4_guard.rs`.
BUT: **zero of the 7 scripts has a working endpoint** — the API was narrowed to
12 read-only observability routes post-AWS-lifecycle; `/api/instruments/rebuild`
(the one endpoint a script header still claims "is already exposed") was retired
in PR #6b. All scripts fail safely (exit 1/2 + audit log) and every referencing
YAML rule is at 0.85 confidence → escalates, so the runtime triage chain is
fail-safe today.

**This PR ships only the autonomous-safe slice:**
1. Retire `auto-fix-restart-depth.sh` + `-rollback.sh` — depth-20/200 is deleted
   FOREVER per `websocket-connection-scope-lock.md`; its endpoint can never
   exist; no YAML rule references it. Keeping it invites a future session to
   "implement" a forbidden subsystem.
2. DEFERRED (documented): correcting the false "already exposed and
   idempotent" header claim in `auto-fix-refresh-instruments.sh` — this session
   can only land commits via the GitHub contents/trees API (local pre-commit
   hooks cannot run: the container's rustup 1.95.0 toolchain is missing its
   cargo binary), and an API write to a `.sh` strips the executable bit, which
   would fail `triage_yaml_auto_fix_targets_all_exist_and_executable` with no
   API path to restore `+x`. The false claim is instead corrected in the
   milestone doc (item 4); the one-line comment fix goes to the next
   local-git-capable session.
3. Update `autonomous_ops_m3_m4_guard.rs` PRE_M3_EXEMPT list (drop the deleted
   script's entry).
4. Update `.claude/plans/autonomous-operations-100pct.md` M2+M3 status to the
   measured reality, and record the OPEN OPERATOR DECISION for the M3 remainder:
   whether to (a) re-introduce authenticated mutating endpoints
   (`/api/auth/rotate`, `/api/spill/drain`, `/api/ws/reset-pool`,
   `/api/kill-switch/activate`, possibly `/api/instruments/rebuild`) against the
   deliberately read-only API — new attack surface incl. token-rotation and
   order-kill-switch over HTTP, contradicting the post-AWS-lifecycle narrowing —
   or (b) re-scope M3: scripts remain operator-runbook tools and auto-execution
   stays escalate-only. NO endpoint code is written in this PR.

## Edge Cases

- Guard test scans `scripts/auto-fix-*.sh` dynamically: deleting BOTH halves of
  the restart-depth pair keeps the pair-completeness test green; the stale
  PRE_M3_EXEMPT entry is removed in the same PR so the list never names a
  nonexistent file.
- `make triage-dry-run` / `error-triage.sh` never reference restart-depth (no
  YAML rule does) — no runtime path changes.
- refresh-instruments stays referenced by the DATA-813 rule; behaviour is
  unchanged (404 → exit 2 + audit log) — only the lying comment is fixed.

## Failure Modes

- A future variant of the depth subsystem returning is governed by the WS-scope
  lock's re-approval protocol, not by keeping a dead script around.
- If the guard test had hardcoded restart-depth elsewhere, the build would fail
  loudly in CI — grep shows only the PRE_M3_EXEMPT entry.

## Test Plan

- `cargo test -p tickvault-common --test autonomous_ops_m3_m4_guard` green after
  the deletions + exemption-list edit.
- `cargo test -p tickvault-common --test triage_rules_guard --test
  triage_rules_full_coverage_guard` green (script-reference tests).
- `bash scripts/auto-fix-refresh-instruments.sh --dry-run` still exits 0/1
  exactly as before (comment-only change).
- banned-pattern scanner clean.

## Rollback

Single revert restores the two scripts, the exemption entry, and the doc text.
No runtime, data, or schema surface.

## Observability

No runtime change. The milestone doc becomes the accurate source for the next
session (this session was itself misled by the stale "46 missing rules" claim —
this PR prevents the same wasted re-discovery).

## Plan Items

- [x] Retire the dead depth auto-fix pair (deletes the two restart-depth
      scripts named in Design item 1; verifier note: deleted paths are
      intentionally not listed as Files since they no longer exist)
  - Files: crates/common/tests/autonomous_ops_m3_m4_guard.rs
  - Tests: every_auto_fix_script_has_matching_rollback

- [x] Drop the deleted script from PRE_M3_EXEMPT
  - Files: crates/common/tests/autonomous_ops_m3_m4_guard.rs
  - Tests: auto_fix_scripts_emit_correlation_id_in_logs

- [x] Honest-state corrections (milestone doc; script-comment fix deferred
      per Design item 2's executable-bit constraint)
  - Files: .claude/plans/autonomous-operations-100pct.md
  - Tests: auto_fix_actions_must_reference_an_existing_script (existing; path unchanged)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Guard suite after deletions | all green (dynamic pair scan) |
| 2 | DATA-813 rule still resolves its script | triage_rules_guard green |
| 3 | Operator reads milestone doc next session | accurate state + explicit pending decision |
