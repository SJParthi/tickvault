# Implementation Plan: M2 — full triage-rule coverage ratchet (exclusion mechanism + hygiene guards)

> **Archived 2026-06-10** on PR #1083 merge-prep. The plan-file conflict with
> parallel PRs #1081/#1082 (which installed the P2b plan as active-plan.md) was
> resolved by archiving this VERIFIED plan directly — archival on merge is the
> protocol end-state anyway.

**Status:** VERIFIED
**Date:** 2026-06-10
**Approved by:** Parthiban (operator task directive 2026-06-10: "Milestone 2 of
.claude/plans/autonomous-operations-100pct.md: full triage-rule coverage. …
Add a ratchet test: every ErrorCode variant has a triage rule OR a documented
exclusion line — build fails on a future variant added without one. Only
.claude/triage/ + one test file; plan still required. ONE PR, monitored to merge.")
**Authority:** `.claude/plans/autonomous-operations-100pct.md` Milestone 2.

**Per-item guarantee matrix:** this plan cross-references the canonical 15-row +
7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` per its
"or cross-reference it" clause. Item-specific deltas: no hot-path code touched
(YAML + one existing meta-guard test file); no new pub fn; no new dep; no schema
change; no tick path; no production crate `src/` change.

## Design

**Measured baseline (2026-06-10, evidence-first):** running the existing guard
`cargo test -p tickvault-common --test triage_rules_full_coverage_guard` shows
**6/6 tests green** — every one of the current ErrorCode variants already has a
rule in `.claude/triage/error-rules.yaml` (113 rule blocks ≥ variant count),
every non-`is_auto_triage_safe()` code escalates, and every `auto_fix`/
`auto_restart` rule points at a real script. The companion
`triage_rules_guard.rs` already pins: no ghost `code:` values, valid actions,
`runbook_path:` values resolve on disk. So Milestone 2's "add 46 missing rules"
clause is already satisfied by prior incremental PRs; the REMAINING gap is the
task's ratchet clause: the coverage guard has **no documented-exclusion
mechanism** and no hygiene guards around one.

**What this PR ships:**

1. `.claude/triage/error-rules.yaml` header — document the exclusion contract:
   a future variant that genuinely needs no triage rule must carry a line
   `# TRIAGE-EXEMPT: <CODE> — <reason>` (em-dash or hyphen separator). Also
   refresh the stale "Current rule count: 6 seeds" header comment to reflect
   full-coverage reality and point at the ratchet test names.
2. `crates/common/tests/triage_rules_full_coverage_guard.rs` — extend the
   all-variant ratchet so a variant passes if it has a rule **OR** a
   `TRIAGE-EXEMPT` line; add three hygiene guards:
   - every `TRIAGE-EXEMPT` line names a LIVE ErrorCode variant (stale
     exemptions for deleted variants fail the build — same rot class as the
     orphaned MOVERS rules cleaned up 2026-05-18);
   - every `TRIAGE-EXEMPT` line carries a non-empty reason;
   - a code may not be BOTH exempted and ruled (ambiguous intent), and a
     non-`is_auto_triage_safe()` code may NEVER be exempted (safety: Critical
     and other unsafe codes always need an escalate rule).

**Explicitly NOT done (honest envelope):** the YAML's per-rule `runbook_path`
values are deliberately MORE specific than `ErrorCode::runbook_path()` for ~15
codes (e.g. DH-901 cites `dhan/authentication.md` instead of the canonical
`dhan/annexure-enums.md`). Those are curated operator-context choices;
existence-on-disk is already ratcheted by `triage_rules_guard.rs`. Forcing
exact equality would destroy curated specificity for zero safety gain, so no
equality ratchet is added and no curated rule content is churned.

## Edge Cases

- A `TRIAGE-EXEMPT` line with a code that was later deleted from the enum →
  hygiene guard fails the build (stale exemption).
- A `TRIAGE-EXEMPT` line with no reason text → fails (undocumented exclusion).
- A code with both a rule block and an exemption line → fails (ambiguous).
- A Critical / non-auto-triage-safe code exempted → fails (safety invariant).
- Exemption line indentation / extra whitespace / `—` vs `-` separator →
  parser accepts leading whitespace and either separator; covered by tests.
- A future variant added with NEITHER rule NOR exemption → the (extended)
  all-variant test fails with an actionable message naming both options.
- Codes that are substrings of other codes (e.g. `DH-90` vs `DH-904`) →
  exemption matching is exact-token, mirroring the existing
  `yaml_contains_code` guard behaviour.

## Failure Modes

- False PASS via a comment that merely mentions a code (e.g. the existing
  "MOVERS-01/02/03 rules removed" prose) → prevented: only lines whose first
  token after `#` is exactly `TRIAGE-EXEMPT:` count as exemptions.
- Hook behaviour change risk → none: `.claude/hooks/error-triage.sh` parses
  only `code:` + `action:` lines; `# TRIAGE-EXEMPT:` is a YAML comment it never
  sees, and its no-matching-rule default is already fail-safe `escalate`.
- Collision with `triage_rules_guard.rs` ghost-code check → none: that test
  scans `code:` fields, not comments.
- Test-file regression in the 6 existing tests → full test binary re-run is
  the gate (see Test Plan).

## Test Plan

Scoped per `.claude/rules/project/testing-scope.md` (change is in
`crates/common/tests/` + `.claude/triage/`):

- `cargo test -p tickvault-common --test triage_rules_full_coverage_guard`
  (all existing 6 + new tests green)
- `cargo test -p tickvault-common --test triage_rules_guard` (companion guard
  unaffected)
- rustfmt (pinned 1.95.0 toolchain binary) clean on the touched test file
- `bash .claude/hooks/banned-pattern-scanner.sh` clean
- `bash .claude/hooks/plan-verify.sh` clean before VERIFIED
- Negative-path proof: new tests include inline unit coverage of the exemption
  parser (accept/reject cases) so the build fails on a malformed exemption.

## Rollback

Single revert of the one squash-merged commit restores the previous YAML header
and test file exactly; no data, schema, config, or runtime behaviour is touched
(tests + comments only). No migration, no state.

## Observability

No runtime emission change. The ratchet itself is the observability artifact:
CI fails with a named-variant message when a future ErrorCode lands without a
triage decision, which is the Milestone-2 contract ("no triage decision, no
auto-fix, no Telegram on that code" must be impossible silently). Coverage
summary remains printed by `coverage_summary_prints_breakdown`, now including
the exemption count.

## Plan Items

- [x] Document the TRIAGE-EXEMPT exclusion contract in the YAML header +
      refresh the stale "6 seeds" rule-count comment
  - Files: .claude/triage/error-rules.yaml
  - Tests: every_error_code_variant_has_a_triage_rule (extended with TRIAGE-EXEMPT support)

- [x] Extend the coverage ratchet with the exclusion mechanism + hygiene
      guards (stale-exemption, reason-required + not-both, unsafe-never-exempt)
  - Files: crates/common/tests/triage_rules_full_coverage_guard.rs
  - Tests: every_error_code_variant_has_a_triage_rule,
    triage_exemptions_must_name_live_codes_and_carry_reasons,
    exempted_codes_must_not_also_have_rules,
    non_auto_triage_safe_codes_must_never_be_exempted,
    exemption_parser_accepts_and_rejects_expected_forms

- [x] Adversarial-review hardening (3-agent pass on the diff; all 3 HIGH
      findings fixed inline)
  - Files: crates/common/tests/triage_rules_full_coverage_guard.rs, .claude/triage/error-rules.yaml
  - Tests: exemptions_must_match_the_pinned_allowlist,
    exemption_like_lines_must_parse_as_valid_exemptions,
    rule_count_matches_or_exceeds_variant_count
  - HIGH-1 block-scalar false positive → exemptions must be a single `#`
    at column 0 (YAML guarantees a column-0 `#` is never scalar content).
  - HIGH-2 silent rule→exemption swap → PINNED_EXEMPTIONS allowlist in
    the test (two-file change by design; empty today).
  - HIGH-3 count-test contradiction → `rules + exemptions >= variants`.
  - MEDIUM near-miss rot → `looks_like_exemption` lint (## / indented /
    lowercase / colon-gap forms fail loudly).
  - LOW separator mangling → strip ONE of em-dash/en-dash/hyphen; LOW
    duplicates → allowlist equality asserts no dups.
  - Security MEDIUM (pre-existing) → auto_fix_script containment guard
    (absolute paths + `..` components rejected).
  - Accepted residuals (documented): test-only `unwrap()` in repo_root
    (allowed in tests), BiDi in panic messages (test-only, CI-log
    cosmetic), pre-existing naive `code:` line-scan shared with
    triage_rules_guard.rs (ghost-code test fails loudly on real rot).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Future variant added, no rule, no exemption | build fails naming the code + both remedies |
| 2 | Future variant added with `# TRIAGE-EXEMPT: X — reason` | build passes |
| 3 | Exemption for deleted variant left behind | build fails (stale) |
| 4 | Exemption without reason | build fails |
| 5 | Code has rule AND exemption | build fails (ambiguous) |
| 6 | Critical (non-auto-triage-safe) code exempted | build fails (safety) |
| 7 | All current 113 rules unchanged | 6 pre-existing tests still green |
