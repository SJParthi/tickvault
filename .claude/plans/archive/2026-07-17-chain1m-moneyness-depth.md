# Implementation Plan: option_chain_1m moneyness_depth DOUBLE companion column

**Status:** DRAFT
**Date:** 2026-07-17
**Approved by:** pending

> **Per-Item Guarantee Matrix:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row + 7-row
> matrices apply verbatim to every item below; per-item deltas are noted in
> the Test Plan + Observability sections — no new hot-path code, no new
> tick-drop path, no DEDUP-key change, arithmetic single-homed in
> `crates/common/src/moneyness.rs`).

## Plan Items

- [x] Depth arithmetic in the single moneyness home (tickvault-common)
  - Files: crates/common/src/moneyness.rs
  - Tests: test_moneyness_depth_paise_ce_pe_sign_convention,
    test_moneyness_depth_paise_guards_and_overflow_boundary,
    test_depth_paise_to_rupees_conversion,
    test_moneyness_depth_sign_is_consistent_with_classification

- [x] Schema + writer: moneyness_depth DOUBLE column, ILP-sparse, NOT in DEDUP
  key; Dhan close_to_data_ms threading via append_row_ext (tickvault-storage)
  - Files: crates/storage/src/option_chain_1m_persistence.rs
  - Tests: test_chain1m_append_row_stamps_moneyness_depth_when_some_omits_when_none,
    test_chain1m_append_row_ext_groww_stamps_feed_rho_and_latency,
    test_option_chain_1m_dedup_key_shape (moneyness_depth absent from key),
    test_option_chain_1m_create_ddl_contains_expected_columns

- [x] Dhan chain leg: compute per-row depth at classify time, persist via
  append_row_ext(row, None, Some(close_to_data_ms)) (tickvault-app)
  - Files: crates/app/src/option_chain_1m_boot.rs
  - Tests: option_chain_1m_boot unit suite (79) +
    crates/app/tests/option_chain_1m_wiring_guard.rs (parse-only strike ratchet)

- [x] Groww chain leg: same depth computation, persist via
  append_row_ext(row, Some(rho), Some(close_to_data_ms)) (tickvault-app)
  - Files: crates/app/src/groww_option_chain_1m_boot.rs
  - Tests: groww_option_chain_1m_boot unit suite (35) +
    crates/app/tests/groww_chain_1m_wiring_guard.rs

- [x] Rule-file dated notes (§2d close_to_data_ms/rho correction, §2g
  moneyness_depth contract, §4 trigger list)
  - Files: .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: n/a — docs (guards re-run green: dedup_segment_meta_guard,
    chain_snapshot_ram_first_guard, partition_retention_coverage_guard)

## Design

`option_chain_1m` stores the SYMBOL `moneyness` classification but no numeric
distance. This change adds `moneyness_depth` DOUBLE (rupees), a signed,
leg-normalized distance stamped at write time on BOTH feed legs (Dhan +
Groww). Sign convention: negative = ITM-direction, positive = OTM-direction,
0 = strike at spot; CE depth = strike − spot, PE depth = spot − strike.
Computation is integer-paise (`moneyness_depth_paise(leg, strike_paise,
spot_paise) -> Option<i64>` with ≥1-paise positivity guards + checked
subtraction), converted to rupees only at the write boundary
(`depth_paise_to_rupees`). Both fns live in `crates/common/src/moneyness.rs`
— the single arithmetic home — so the boot legs stay parse-only and the
strike ratchet stays green. The column is NOT in the DEDUP key and is
ILP-sparse (written only when Some; NULL otherwise). The writer gains
`append_row_ext(&row, rho: Option<f64>, close_to_data_ms: Option<i64>)`;
the Dhan leg now threads its already-computed `close_to_data_ms` (previously
dropped at persist time); `rho` stays None on Dhan FOREVER (the Dhan
option-chain response carries delta/theta/gamma/vega only — no rho field to
parse; documented in the rule file). The RAM `ChainMoneynessSnapshot`
publish path is unchanged (DB audit column only).

## Edge Cases

- Unparsable leg label ("XX", garbage) → depth None → column NULL (mirrors
  moneyness = UNKNOWN; never fabricated).
- Strike or spot < 1 paise (zero/negative/absent-spot 0.0-silent shape) →
  None → NULL.
- Overflow: with both operands ≥ 1, checked_sub result is bounded
  ±(i64::MAX − 1) — structurally unreachable overflow; boundary test pins
  the widest legal pairs still computing Some.
- ATM exact match → depth Some(0.0) while moneyness = ATM band — sign
  consistency test pins classify == Itm ⇒ depth < 0, Otm ⇒ depth > 0.
- Pre-existing rows: ALTER-ADD self-heal leaves them NULL forever — never
  backfilled (at-write observation).
- DEDUP re-append (backfill/sweep repair): latest-run-wins on the label
  column, exactly like close_to_data_ms.
- Half-paise divergence: the depth derives from `price_to_paise_guarded`
  (rounded paise) while the persisted `strike` DOUBLE is the raw parsed
  f64 — a sub-paise strike would make `moneyness_depth` differ from
  (stored strike − stored spot) by < 1 paise; real NSE strikes are
  whole-paise, so the divergence is nil in practice.

## Failure Modes

- Persist failure: unchanged CHAIN-03 semantics (discard-pending poisoned
  buffer defense; the new column adds no failure class — a None depth simply
  omits the ILP field).
- Ensure-DDL failure: the pre-existing HTTP-CLIENT-01-class duplicate-row
  window applies unchanged; the new column rides the same
  `ALTER TABLE ADD COLUMN IF NOT EXISTS` self-heal.
- A wrong depth can never enter the DEDUP identity (not in key), so a bug
  fix re-append heals rows in place.
- No new ErrorCode, no new counter — classification-quality degrades stay
  the existing CHAIN-02 stages.

## Test Plan

Testing-scope honesty: `crates/common` was touched, so the canonical
scope per testing-scope.md is WORKSPACE escalation — run locally as the
block-scoped suites below (disk constraint on the dev box); the full
workspace battery is covered by CI on the PR.

Block-scoped per testing-scope.md:
- tickvault-common moneyness suite: 28 passed (4 new).
- tickvault-storage option_chain_1m_persistence suite: 15 passed (1 new +
  2 rewritten for the append_row_ext signature).
- tickvault-app option_chain_1m_boot: 79 passed; groww_option_chain_1m_boot:
  35 passed; option_chain_1m_wiring_guard: 10 passed;
  groww_chain_1m_wiring_guard: 5 passed.
- Guards: dedup_segment_meta_guard (7), chain_snapshot_ram_first_guard (6),
  partition_retention_coverage_guard (3), partition_archive_guard (7).
- fmt + clippy (-D warnings -W clippy::perf) on common/storage/app.

## Rollback

Revert the branch commits. The DB column is additive + nullable: an old
binary against a new-schema table simply stops writing the column (NULL);
a new binary against an old-schema table self-heals via ALTER-ADD at the
next ensure. No data migration, no DEDUP-key change, nothing to unwind in
QuestDB.

## Observability

No new metrics/alerts/ErrorCode by design: depth is an audit companion to
the existing moneyness column, whose UNKNOWN/atm-absent/step-drift signals
(`tv_moneyness_unknown_total`, `tv_moneyness_atm_absent_total`,
`tv_moneyness_step_drift_total` + the CHAIN-02 stages) already cover the
classification-quality surface — a NULL depth co-occurs with those exact
signals. The rule file (`rest-1m-pipeline-error-codes.md` §2g 2026-07-17
paragraph) documents the contract; queryability is direct
(`select moneyness, moneyness_depth from option_chain_1m ...`).
