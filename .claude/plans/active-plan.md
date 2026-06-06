# Implementation Plan: §28 indicator/strategy operator-boundary guard

**Status:** APPROVED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion "implement the guard test" (2026-06-06).
**Crate(s) touched:** none under `src/` — adds `crates/storage/tests/operator_boundary_indicator_strategy_guard.rs` (a test) + docs/plan files only. (Design-first wall does not trigger on `tests/`, but this plan exists for plan-enforcement + the per-wave guarantee matrix.)

## Context

`.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §28 (operator
boundary, 2026-05-27 verbatim "as of now don't even touch indicators and strategies
area dude okay?") PROMISES a mechanical ratchet:

> `crates/storage/tests/operator_boundary_indicator_strategy_guard.rs::test_indicator_engine_states_field_unchanged_since_2026_05_27`
> (source-scan: SHA-256 the relevant lines of `engine.rs`; any modification fails the
> build until the boundary is lifted)

Deep-research agent verification (2026-06-06) confirmed the test FILE DOES NOT EXIST —
the boundary is currently enforced only by human discipline, not mechanically. This
item ships the missing ratchet.

## Plan Items

- [x] Item 1 — Add `crates/storage/tests/operator_boundary_indicator_strategy_guard.rs`
  - Files: `crates/storage/tests/operator_boundary_indicator_strategy_guard.rs`
  - Tests: `test_indicator_engine_states_field_unchanged_since_2026_05_27`,
    `test_indicator_strategy_boundary_files_unchanged`,
    `test_boundary_manifest_covers_every_src_file`

## Design

The boundary is "do not touch `crates/trading/src/indicator/` or
`crates/trading/src/strategy/`". The strongest mechanical enforcement is a
content-pin of EVERY `.rs` file under both directories. Any edit (even a comment)
flips the pin → build fails → forces a conscious boundary-lift (re-bless + rule
edit).

- **Hash:** inline FNV-1a 64-bit (offset `0xcbf29ce484222325`, prime
  `0x100000001b3`). Chosen over `sha2` because new workspace deps need operator
  approval (CLAUDE.md), and FNV-1a is deterministic + version-stable for a
  persisted pin (unlike `std::hash::DefaultHasher`). This is regression-evidence,
  not adversarial crypto, so FNV-1a is sufficient.
- **Manifest:** a `const BOUNDARY_FILES: &[(path, fnv1a64, byte_len, line_count)]`
  baked from the current tree (computed 2026-06-06).
- **Repo-root resolution:** `CARGO_MANIFEST_DIR` (`crates/storage`) → `parent().parent()`
  — same pattern as `crates/core/tests/indices4only_scope_lock_guard.rs`.
- **`test_indicator_engine_states_field_unchanged_since_2026_05_27`** (the §28-named
  test): asserts `engine.rs` still contains the exact `states: Vec<IndicatorState>,`
  field line AND its (fnv, len) matches the pin — satisfies the §28 "states field
  unchanged" contract specifically.
- **`test_indicator_strategy_boundary_files_unchanged`**: every manifest file matches
  its pinned (fnv, len).
- **`test_boundary_manifest_covers_every_src_file`**: the manifest is neither missing
  a file that exists on disk nor referencing a file that was deleted (catches a NEW
  file added to the frozen dirs, or a deletion).

## Edge Cases

- **New `.rs` added to a frozen dir** → `test_boundary_manifest_covers_every_src_file`
  fails (disk has a file the manifest doesn't) — boundary still enforced.
- **A frozen file deleted** → same test fails (manifest references a missing file).
- **Reformatting / comment-only edit** → fnv changes → fails. Intended: §28 is
  "don't even touch".
- **Line-ending/whitespace drift** → fnv + byte_len change → fails. Intended.
- **Test run from a different CWD** → repo root is derived from `CARGO_MANIFEST_DIR`,
  not CWD, so it is stable.

## Failure Modes

- The test only READS files; it cannot mutate the repo. Worst case is a false RED
  when the operator legitimately edits the frozen area — resolved by re-blessing the
  manifest AND updating §28 (the intended "lift the boundary" ritual). The failure
  message spells this out.
- If `CARGO_MANIFEST_DIR` resolution ever breaks (repo restructure), the test fails
  closed (missing files) rather than silently passing.

## Test Plan

- `cargo test -p tickvault-storage --test operator_boundary_indicator_strategy_guard`
  → all 3 tests green on the current (un-edited) tree.
- Negative check (manual, not committed): touch a byte in `engine.rs` → test goes RED;
  revert → GREEN. (Verified locally during implementation, not part of the committed
  suite.)
- `cargo build --workspace` stays green (test-only addition).

## Rollback

- Single test file + plan/docs. `git revert <sha>` removes the guard entirely; zero
  runtime/production impact (it is a dev/test-time ratchet only).

## Observability

- This is a build-time ratchet, not a runtime path — no Prometheus/Telegram/audit
  surface applies. Its "signal" is a failing `cargo test` / CI check, which is the
  correct and only observability for a source-scan guard (mirrors
  `indices4only_scope_lock_guard.rs`, `dedup_segment_meta_guard.rs`). N/A for the
  7-layer runtime telemetry by design.

## Per-Item Guarantee Matrix (per `.claude/rules/project/per-wave-guarantee-matrix.md`)

| Demand | This item |
|---|---|
| 100% testing | 3 tests cover field-pin, all-file-pin, and add/delete drift |
| 100% scenarios | edge cases above (add/delete/reformat/whitespace/CWD) each map to a test arm |
| 100% code review | adversarial self-review of the guard logic before PR |
| O(1)/perf | N/A — test-time only, not a hot path; no runtime allocation introduced |
| zero-loss / WS / QuestDB | N/A — does not touch tick/WS/DB paths |
| uniqueness/dedup | N/A — no storage key |
| honest envelope | guard detects ANY content change to the 12 frozen files; it does NOT prevent edits, it FAILS THE BUILD on them (the §28 "until the boundary is lifted" mechanism) |
