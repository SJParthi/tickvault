# Implementation Plan: Autonomous-Ops M4 — wire fix→verify→rollback loop + M4/M5 status truth-up

**Status:** APPROVED
**Date:** 2026-06-14
**Approved by:** Parthiban (2026-06-14, AskUserQuestion → "Autonomous-ops M4/M5"; this plan scopes the buildable, non-AWS, non-hallucinated slice and is the corrective work for the M4 loop bug found this session)
**Branch:** `claude/upbeat-hypatia-6trzk3`
**Authority:** CLAUDE.md > operator-charter-forever.md > design-first-wall.md > this file
**Scope guard:** NO `crates/*/src/**.rs` changes (only `scripts/`, `.claude/`, `docs/`, and `crates/common/tests/`). Plan-gate exempt; plan-enforcement satisfied by this file.

---

## Design

The M4 ("Verify + Rollback") tooling already exists on disk and is enforced by
`crates/common/tests/autonomous_ops_m3_m4_guard.rs`:
`scripts/triage/verify.sh`, `scripts/triage/rollback.sh`, and 6 `auto-fix-*.sh` +
`*-rollback.sh` pairs. **But the driver — `.claude/triage/claude-loop-prompt.md` —
never invokes verify.sh or rollback.sh**, so the fix→verify→rollback contract is
documented nowhere it actually runs. Separately, the runbook's Step 4 still
describes *live auto-execution of fix scripts* and references the **retired**
`POST /api/instruments/rebuild` endpoint, both of which contradict the operator's
**M3 escalate-only lock** (Parthiban, 2026-06-10, PR #1087): "API stays read-only;
auto-fix scripts remain operator-runbook tools; triage auto-execution stays
escalate-only (detect + page, never act)."

This plan makes the runbook truthful and self-consistent, documents the
operator-run fix→verify→rollback sequence (with correlation IDs), adds ratchets so
the wiring + escalate-only invariant can't silently regress, fixes a stale guard
self-reference in `rollback.sh`, and truth-ups the M4/M5 status in the plan doc
(M4 = built-as-tooling + loop documented; M5 = local chaos shipped, AWS parts
honestly deferred). NO runtime auto-execution is introduced — that respects the
M3 lock.

## Plan Items

- [x] Item 1 — Rewrite `.claude/triage/claude-loop-prompt.md` Step 4 + invariants to the escalate-only model: detect→classify→`--dry-run` preview→escalate; document the OPERATOR-RUN fix→verify→rollback sequence citing `scripts/triage/verify.sh` and `scripts/triage/rollback.sh` with correlation IDs; remove the retired `POST /api/instruments/rebuild` reference + the "loop may run scripts live" claim.
  - Files: `.claude/triage/claude-loop-prompt.md`
- [x] Item 2 — Fix the stale guard self-reference in `scripts/triage/rollback.sh` (`autonomous_ops_m4_guard.rs` → `autonomous_ops_m3_m4_guard.rs`).
  - Files: `scripts/triage/rollback.sh`
- [x] Item 3 — Extend `crates/common/tests/autonomous_ops_m3_m4_guard.rs` with ratchets: (a) runbook references both `verify.sh` and `rollback.sh`; (b) runbook states the escalate-only invariant; (c) runbook does NOT reference the retired `/api/instruments/rebuild`; (d) `rollback.sh` self-reference names the real guard file.
  - Files: `crates/common/tests/autonomous_ops_m3_m4_guard.rs`
  - Tests: `loop_runbook_wires_verify_and_rollback`, `loop_runbook_states_escalate_only_invariant`, `loop_runbook_has_no_retired_rebuild_endpoint`, `rollback_dispatcher_self_reference_names_real_guard`
- [x] Item 4 — Truth-up `.claude/plans/autonomous-operations-100pct.md` layer table + M4/M5 sections with the measured 2026-06-14 state (M4 built-as-tooling + loop wired/corrected; M5 local chaos shipped via 18 chaos_*.rs, AWS Lambda/EBS/ASG/nightly-CI deferred to post-provision).
  - Files: `.claude/plans/autonomous-operations-100pct.md`

## Edge Cases

- Runbook scanned by the guard via substring match — keep canonical strings exact (`scripts/triage/verify.sh`, `scripts/triage/rollback.sh`) so the ratchet is stable.
- `error-rules.yaml` still contains `auto_fix_script:` keys (used by the existing coverage guard) — those stay; only the *runbook's* execution framing changes. The escalate-only behaviour is already enforced at runtime by the confidence<0.95 gate + `is_auto_triage_safe()`; this plan aligns the *documentation* with that reality.
- The retired-endpoint ratchet must match the runbook only (not the whole repo) to avoid false hits on historical docs.

## Failure Modes

- If a future edit re-introduces live auto-execution or the retired endpoint into the runbook → Item 3 ratchets fail the build.
- If verify.sh/rollback.sh are renamed/removed → existing guard tests (`verify_harness_script_exists_*`, `rollback_harness_dispatcher_exists_*`) + new wiring ratchet fail the build.
- No runtime/hot-path impact: zero `crates/*/src` changes, so no tick-loss/latency/O(1) surface is touched.

## Test Plan

- `cargo test -p tickvault-common autonomous_ops_m3_m4_guard` — all existing + 4 new tests green (real output pasted in PR, per zero-loss-charter §4 evidence rule).
- `bash scripts/triage/rollback.sh` usage path (no-arg) exits 1; `bash -n scripts/triage/rollback.sh` parses clean.
- Pre-push gates: banned-pattern scanner, pub-fn guards (no new pub fns), plan-verify (this plan's items ticked), fmt.
- Scoped per `testing-scope.md`: only `tickvault-common` is touched (tests/ only) → scoped run, not full workspace.

## Rollback

- Every change is docs/scripts/tests only. `git revert <sha>` restores all four files verbatim with zero runtime impact. No data migration, no schema, no deployed artifact.

## Observability

- The deliverable IS an observability/triage-loop correctness fix. No new metric/counter (the loop already logs to `data/logs/auto-fix.log` with `corr_id=`). The new ratchet tests are the regression-detection layer. M4/M5 status truth-up improves operator-facing honesty (charter §F).

## Guarantee matrix (per per-wave-guarantee-matrix.md — applicable rows)

- 100% testing: 4 new ratchet tests + existing guard suite green (categories 1/3/4/22 for `common`).
- 100% code checks: banned-pattern + plan-verify + fmt green.
- 100% logging/alerting: unchanged (loop's existing audit log + escalate path).
- Honest envelope: M5 AWS parts explicitly DEFERRED (not claimed done). No "100% autonomous" claim — runtime auto-execution stays escalate-only by operator lock.
- N/A rows: performance/DHAT/bench (no hot path), audit-table (no new event), chaos (no new failure mode) — all N/A, reason: docs/scripts/tests-only change.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Future Claude runs `/loop` | Runbook says detect→classify→dry-run→escalate; never auto-executes a fix against live system (M3 lock honored) |
| 2 | Operator manually remediates | Runbook documents fix → `verify.sh <corr_id> <predicate>` → on fail `rollback.sh <fix> <corr_id>`, all audit-logged |
| 3 | Someone re-adds live auto-exec or retired endpoint to runbook | Build fails (Item 3 ratchet) |
| 4 | Reader checks M4/M5 status | Plan doc shows truthful measured state, AWS parts flagged deferred |
