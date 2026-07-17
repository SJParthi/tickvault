# Implementation Plan: WS-REINJECT-01 Paging-Surface Retirement (evidence-audit Fix PR C)

**Status:** IN_PROGRESS
**Date:** 2026-07-17
**Approved by:** Coordinator evidence-audit directive (Fix PR C, direction RETIRE — MEDIUM finding: dead paging filter with an orphaned emit path)

## Plan Items

- [x] Remove the dead `ws-reinject-01` CloudWatch log-filter/alarm map entry
  - Files: deploy/aws/terraform/error-code-alarms.tf
  - Tests: (drift guard) error_code_paging_filter_drift_guard
- [x] Move WS-REINJECT-01 to "Retired paging entries" in the observability doc
  - Files: .claude/rules/project/observability-architecture.md
  - Tests: error_code_paging_filter_drift_guard
- [x] Dated RETIRED banner on the runbook rule file
  - Files: .claude/rules/project/ws-reinject-error-codes.md
  - Tests: every_runbook_path_exists_on_disk (crossref)
- [x] Delete the orphaned emit module + mod decl + comment truth-sync
  - Files: crates/app/src/wal_reinject.rs (deleted), crates/app/src/lib.rs, crates/app/src/boot_helpers.rs
  - Tests: cargo test -p tickvault-app --lib
- [x] Delete the now-unused WAL_REINJECT_* constants with a dated note
  - Files: crates/common/src/constants.rs
  - Tests: cargo test -p tickvault-common --lib
- [x] Delete the `WsReinject01Aborted` ErrorCode variant per the C4-sweep house pattern
  - Files: crates/common/src/error_code.rs (variant, code_str, severity arm, runbook arm, all(), catalogue count 161→160, prefix allowlist, dedicated contract test), crates/common/tests/error_code_rule_file_crossref.rs (REVERSE_CHECK_ALLOWLIST)
  - Tests: test_all_list_length_matches_catalogue_size, test_code_str_follows_expected_prefix_pattern, every_error_code_variant_appears_in_a_rule_file, every_rule_file_code_has_an_enum_variant

## Design

The 2026-07-17 coordinator evidence audit found the `ws-reinject-01` CloudWatch
paging filter DEAD: its only emitter, `crates/app/src/wal_reinject.rs`
(`reinject_wal_frames`), has ZERO production callers on main — the STAGE-C.2b
re-injection call sites died with the Dhan live-WS lane (Phase C, 2026-07-13),
and main.rs now count-residuals the staged WAL frames
(`tv_ws_frame_wal_reinjected_dropped_total`) + `confirm_replayed()` instead.
A filter with no possible emit site is a dead filter per the paging drift
guard's own doctrine (the ws-gap-07 / feed-stall-01 / cross-verify-1m
precedent). Direction RETIRE: remove the tf map entry, move the doc paging-list
mention to "Retired paging entries", banner the rule file, delete the orphaned
module + its constants (tickvault-common) + the ErrorCode variant per the
C4-sweep house pattern (rule file retained satisfies the forward crossref;
the code string joins REVERSE_CHECK_ALLOWLIST — the WS-GAP-05..09 mechanism).

## Edge Cases

- The drift guard's self-test fixtures reference `WsReinject01Aborted` only
  inside synthetic-source STRING LITERALS (no compile dependency) — left
  untouched; guard stays green.
- Historical precedent comments citing ws-reinject-01 (tf ok_recovery
  rationale rows, main.rs growth-storm class comments, constants.rs dated
  note) are RETAINED — they are dated audit history, not live references.
- The retained `tv_ws_frame_wal_reinjected_dropped_total` counter sites in
  main.rs are the count-residuals coverage — deliberately kept.
- Catalogue count recounted against fresh main (161 after SPOT-XVERIFY-01/02)
  → 160 with a dated bump note.

## Failure Modes

- Forward crossref would fail if the rule file were deleted — it is RETAINED
  as historical audit (banner convention), so `every_runbook_path_exists_on_disk`
  needs no allowlist change.
- Reverse crossref would fail on the retained code string without the
  REVERSE_CHECK_ALLOWLIST entry — added with justification.
- Prefix-pattern test would fail on a future variant reusing WS-REINJECT- —
  the removed-families comment makes re-adding conscious, never silent.
- A stale count assert fails the build loudly (test_all_list_length_matches_catalogue_size).

## Test Plan

- `cargo test -p tickvault-common --lib error_code` (26 tests incl. catalogue
  count, prefix pattern, roundtrip)
- `cargo test -p tickvault-common --test error_code_rule_file_crossref`
- `cargo test -p tickvault-common --test error_code_paging_filter_drift_guard`
- `cargo test -p tickvault-app --lib` (module deletion leaves no dangling refs)
- `cargo clippy -p tickvault-app -p tickvault-common --no-deps`

## Rollback

Single revert of the one squash commit restores the module, variant,
constants, tf entry, and doc text verbatim — no data migration, no runtime
state, no deployed-alarm dependency (the filter never matched anything, so
removing it changes zero live paging behaviour).

## Observability

Retained coverage: main.rs staged-WAL count-residuals via
`tv_ws_frame_wal_reinjected_dropped_total{ws_type}` + `confirm_replayed()`;
the WAL durable floor (WS-SPILL-01/02) is untouched. The retirement itself is
documented in `observability-architecture.md` "Retired paging entries" +
dated notes in `error-code-alarms.tf` — no paging behaviour changes (the
filter was structurally dead).
