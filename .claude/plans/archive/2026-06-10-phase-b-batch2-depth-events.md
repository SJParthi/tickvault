# Implementation Plan: Phase B batch 2 — unconstructed depth-era NotificationEvent variants

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban, 2026-06-10: "yes go ahead with your recommendation" (Phase B of the deletion audit — step 5 "delete unused events.rs NotificationEvent variants (DepthRebalance*, … Depth20Dyn*…)"), reaffirmed same day: "once it gets merged go ahead with the plan always". Batch 1 merged as #1084.

**Guarantee matrices:** See `per-wave-guarantee-matrix.md` (cross-referenced; deletion-only change — no new pub fns, tables, or hot-path code).

---

## Design

The depth-20/200 pools, the depth rebalancer, and the depth-20-dynamic selector were all deleted in AWS-lifecycle PR #4 (2026-05-19). Five `NotificationEvent` variants that those subsystems emitted survived in `crates/core/` with **zero production construction sites** (verified by repo-wide grep 2026-06-10, post-#1084):

| Variant | Remaining references |
|---|---|
| `DepthRebalanced` | events.rs def/arms/tests + the service.rs suppression block (a match arm, not a constructor) |
| `DepthRebalanceFailed` | events.rs def/arms/tests + `event_formatting_coverage.rs` render call (#1081 coverage battery) |
| `Depth20DynamicTopSetEmpty` | events.rs + `event_formatting_coverage.rs` |
| `Depth20DynamicSwapChannelBroken` | events.rs + `event_formatting_coverage.rs` |
| `Depth20DynamicSwapApplied` | events.rs + `event_formatting_coverage.rs` |

Deletion surface (all in `crates/core`):
1. `notification/events.rs` — 5 variant definitions, `DepthRebalanceLevels` enum + impl (`title_fragment`/`action_line`, used only by `DepthRebalanced`), 5 `to_message` arms, 5 `topic`/name arms, 5 `severity` arms, the `DepthRebalanced` tuple in the name-roundtrip test, and the 6 `test_depth_rebalance_*` test fns.
2. `notification/service.rs` — the `DepthRebalanced` Telegram-suppression block + `tv_depth_rebalance_telegram_suppressed_total` counter (the variant it suppresses no longer exists; the counter has no other emitter and no alert rule pins it — verified: metric name appears ONLY in service.rs).
3. `tests/event_formatting_coverage.rs` — the 4 render calls for the deleted variants (added by #1081 to cover then-unexercised arms; the arms are now deleted).
4. `.claude/rules/project/observability-architecture.md` — clauses 7/8/10 of "What future sessions MUST NOT do" annotated RETIRED 2026-06-10 (the variants they protect are deleted; clause numbering preserved).

Blockers verified CLEARED since the audit: `crates/core/tests/depth_rebalance_telegram_suppression_guard.rs` (the 3 source-scan ratchets that pinned the suppression code) **no longer exists** — deleted by an earlier session. The `session_2026_04_22_regression_guard.rs` mention is a comment only.

OUT of scope (named follow-up): `DepthDynamicV2DiffApplied` + the exported `DepthDiffEntry` type — a distinct pipeline-v2 family whose deletion reaches `notification/mod.rs` exports and HTML-escape format tests; needs its own pass. ErrorCode variants `Depth20Dyn01/02` stay (documented contract stubs satisfying the crossref test via wave-4/5 rule files).

## Edge Cases

- The service.rs suppression `return` is the only behavioural code touched: with the variant gone, the match arm is unreachable code — both must go in the same commit or compile fails (caught by `cargo build`).
- `DepthRebalanceLevels` is used only by `DepthRebalanced` (verified by grep); its `title_fragment`/`action_line` tests delete with it.
- `event_formatting_coverage.rs` render calls live inside multi-assert test fns — remove only the deleted-variant blocks; surrounding asserts (e.g. `Depth20DynamicSwapApplied`'s neighbour arms like `DepthDynamicV2DiffApplied`) stay.
- Severity/name/topic matches are exhaustive — removing variants + their arms keeps exhaustiveness; a missed arm fails compile.
- Test-count baseline drops by ~6 (the `test_depth_rebalance_*` fns); updated with justification in the same commit per the documented override.
- Rule-file edit: clauses are annotated RETIRED, not deleted, preserving the historical-audit-trail pattern used throughout `.claude/rules/`.

## Failure Modes

- **Hidden consumer:** repo-wide grep proof per variant pasted in the PR body before deletion; `cargo build --workspace` + full core suite with CI's feature flag before commit.
- **Ratchet failures:** test-count guard handled via baseline override with justification; no other guard pins these variants (suppression guard already deleted; verified by filename + content grep).
- **CI divergence:** PR opened as draft → CI green → ready → auto-merge (squash), monitored to merge.
- **Concurrent main movement (observed during batch 1):** before push, fetch + merge origin/main; re-run scoped tests after any merge.

## Test Plan

- `cargo test -p tickvault-core --features daily_universe_fetcher` (CI's invocation; the only touched crate) — paste real `test result:` lines.
- `cargo build --workspace`, `cargo fmt --check`, CI-exact `cargo clippy --workspace --no-deps` (zero new warnings in touched files).
- Hook gates: banned-pattern scanner, plan-gate, pre-commit 9-gate chain, pre-push gates.

## Rollback

Single squash-merged PR; `git revert <sha>` restores variants, suppression block, and rule clauses verbatim. No config/schema/data changes.

## Observability

- Deletes only dead operator-facing surface: the 5 variants have had zero emitters since 2026-05-19, so no Telegram/log/metric output changes in practice.
- `tv_depth_rebalance_telegram_suppressed_total` (a counter that could only ever count suppressions of a never-emitted event) is removed; no dashboard/alert references it (grep-verified: name appears only in service.rs).
- PR body documents the rule-clause retirements so the operator sees the exact governance delta.

## Plan Items

- [x] Batch 2 — delete the 5 unconstructed depth-era NotificationEvent variants + DepthRebalanceLevels + suppression block + coverage render calls + rule-clause retirement
  - Files: crates/core/src/notification/events.rs, crates/core/src/notification/service.rs, crates/core/src/notification/mod.rs, crates/core/tests/event_formatting_coverage.rs, crates/core/tests/session_2026_04_22_regression_guard.rs (stale-comment fix), .claude/rules/project/observability-architecture.md
  - Tests: tickvault-core suite (deletion-only; 6 dead-variant test fns removed with baseline justification)
  - Evidence: `cargo test -p tickvault-core --features daily_universe_fetcher` → lib `test result: ok. 2353 passed; 0 failed` + 61 green suite lines, zero failures; `error_code_rule_file_crossref` → `ok. 3 passed`; fmt clean; CI-exact clippy → only the pre-existing `constituent_resolver.rs` warning. 3-agent adversarial review: hot-path PASS, security 0 issues, hostile no CRITICAL (HIGH = test-count baseline handled via documented override; MEDIUM = pre-existing rule-file/crossref question verified green; LOW = stale comment fixed in this diff).
