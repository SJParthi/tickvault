# Implementation Plan: Rules-Tree Diet Phase 2 — error-code runbooks out of the auto-load path

**Status:** VERIFIED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) — standing pre-authorization relayed via the coordinator session 2026-07-20 (context-size incident: 3 sessions killed by ~777K auto-loaded base context; Phase 1 = PR #1683)

## Design

Move every pure-runbook rule file — all `.claude/rules/project/*-error-codes.md`
EXCEPT `cadence-error-codes.md` (live sessions editing it) — plus
`dual-instance-lock-2026-07-04.md` (the RESILIENCE-01/03 runbook) verbatim into a
new non-auto-loaded `docs/error-runbooks/` directory. Update the
`runbook_path()` literals in `crates/common/src/error_code.rs` (file:
`crates/common/src/error_code.rs`) to the new `docs/error-runbooks/<name>.md`
paths, and widen `crates/common/tests/error_code_rule_file_crossref.rs`
(`load_all_rule_text`) to also scan `docs/error-runbooks/` so both crossref
directions keep passing. Leave one small pointer file
`.claude/rules/project/error-runbooks-index.md` in the auto-load tree so every
session knows where the runbooks live. Nothing is deleted; moves are verbatim
`git mv`.

- Files: `crates/common/src/error_code.rs`,
  `crates/common/tests/error_code_rule_file_crossref.rs`,
  `crates/app/tests/wave_2c_item7_boot_race_and_clock_skew.rs` (one
  runbook-path equality assert), 47 moved `.md` files, `docs/runbooks/*.md`
  link updates, `.claude/rules/project/error-runbooks-index.md` (new).

## Edge Cases

- A runbook_path pointing at a NON-moved file (daily-universe, groww-scope,
  scope-lock, cadence, noise-lock, deploy-provenance, etc.) must stay
  untouched — the sed targets only the moved basenames.
- `docs/runbooks/*.md` reference moved rule paths; `runbook_cross_link_guard`
  requires every referenced path to resolve — those references are rewritten
  to `docs/error-runbooks/...` in the same commit.
- `error_code.rs::test_every_variant_has_non_empty_runbook_path` pins the
  `.claude/` prefix — widened to accept `docs/error-runbooks/` as well.
- Guard tests that read rule-file CONTENT by literal path were enumerated by
  grep: only `cadence_composition_contract_guard.rs` (file NOT moved) and the
  wave-2-c assert in the app test (updated). All other literal readers target
  non-moved files.
- The reverse crossref check scans the widened set; moved files carry the same
  code strings, so no allowlist change is needed.

## Failure Modes

- A missed runbook_path literal → `every_runbook_path_exists_on_disk` fails
  (caught pre-commit by running the crossref test).
- A live variant whose only mention was in a moved file → forward check fails
  unless the widened scan covers `docs/error-runbooks/` (it does).
- A docs/runbooks link left pointing at the old path →
  `runbook_cross_link_guard` fails; fixed by the link rewrite pass + verified
  by running the full `tickvault-common` test suite.
- Any recursive `.claude/rules` dir-walker other than the crossref test was
  grepped for (`read_dir`/`walk` over `.claude/rules`): only the crossref test
  walks it.

## Test Plan

- `cargo test -p tickvault-common` (full suite — crossref, runbook cross-link,
  paging drift, completion status, lattice, wording guards)
- `cargo test -p tickvault-storage` (full suite — instance/daily-universe/
  dedup/boot-step guards)
- `cargo test -p tickvault-app --test wave_2c_item7_boot_race_and_clock_skew`
  (the updated runbook-path assert)
- `cargo fmt --check`, `bash .claude/hooks/pre-push-gate.sh`

## Rollback

Single revert of the Phase-2 commit restores every file to
`.claude/rules/project/` and every path literal — `git revert <sha>` is clean
because moves are verbatim and no content was edited beyond path literals and
the two test relaxations. Phase 1 (PR #1683) is unaffected.

## Observability

No runtime behavior change (path strings + docs only). Proof artifacts: the
crossref test's three assertions stay green on the new layout;
`mcp__tickvault-logs__find_runbook_for_code` continues to resolve codes (it
searches `docs/runbooks/` + `.claude/rules/` by content and the index file
points at `docs/error-runbooks/`). Before/after byte counts of
`.claude/rules/**/*.md` are recorded in the PR body.

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — all 15 + 7 rows apply to every item in
this plan. Docs/path-relocation scope: most rows are satisfied by the existing
ratchets (crossref, runbook cross-link, triage guards) staying green on the
new layout; no new tick-drop path, no hot-path code, no runtime behavior
change is introduced (relocation + path literals only).

## Plan Items

- [x] Move 47 runbook files verbatim to `docs/error-runbooks/`
  - Files: 46 × `*-error-codes.md` (not cadence) + `dual-instance-lock-2026-07-04.md`
  - Tests: every_runbook_path_exists_on_disk
- [x] Update `runbook_path()` literals + prefix unit test
  - Files: crates/common/src/error_code.rs
  - Tests: test_every_variant_has_non_empty_runbook_path
- [x] Widen crossref scan to docs/error-runbooks/
  - Files: crates/common/tests/error_code_rule_file_crossref.rs
  - Tests: every_error_code_variant_appears_in_a_rule_file, every_rule_file_code_has_an_enum_variant
- [x] Fix the wave-2-c runbook-path assert
  - Files: crates/app/tests/wave_2c_item7_boot_race_and_clock_skew.rs
  - Tests: (same file)
- [x] Index pointer + docs/runbooks link rewrite
  - Files: .claude/rules/project/error-runbooks-index.md, docs/runbooks/*.md
  - Tests: runbook_cross_link_guard
