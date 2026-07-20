# Implementation Plan: moneyness combined step label — "ITM-1/OTM+1" in the moneyness column; moneyness_depth REMOVED (operator ruling 2026-07-20, final)

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) — same-day rulings 2026-07-20, final verbatim: "under moneyness column itself make it as itm-1 or otm+1, don't create new column — what is this moneyness_depth column, just remove it" + "it should be ITM -1 or ITM -2 or something like that" + "how about one extra column to mention everything like this — for example NIFTY 28 JUL 25000 CALL"

## Per-Item Guarantee Matrix

See `per-wave-guarantee-matrix.md` — the full 15-row 100% Guarantee Matrix
and the 7-row Resilience Demand Matrix apply to every item in this plan.
Confirmed: all 15 + 7 rows hold for each item below (cold-path label/DDL
change: no new tick-drop path, no hot-path allocation — the label type is
a stack buffer; DEDUP keys untouched; ratcheted label-law + consistency
tests fail the build on regression; the drop-column migration is
marker-gated + idempotent + SEBI-safe: one label column removed on an
explicit dated operator order, never a table drop, never a data row
deletion).

## Design

Final spec (after two same-day refinements — sign-flip, then step counts,
then combined-label single-column): the `option_chain_1m.moneyness`
SYMBOL column ITSELF carries the classic chain notation — `ITM-1`,
`ITM-2`, …, `ATM`, `OTM+1`, `OTM+2`, … (`UNKNOWN` unchanged; bare
`ITM`/`OTM` as the honest fallback for off-grid strikes). The Jul-17
SIGN convention (ITM minus / OTM plus) matches the notation and stays.
The step is computed integer-exactly in the single arithmetic home
`crates/common/src/moneyness.rs`: NEW `moneyness_step_index(leg,
strike_paise, atm_paise, step_paise) -> StepIndexOutcome`
(Aligned/NotAligned/Invalid) anchored on the SAME grid machinery the
classifier uses (`strike_step_paise` const table + `atm_strike_paise`
round-half-up anchor), plus NEW zero-alloc `MoneynessStepLabel` (24-byte
stack buffer) + `moneyness_step_label(class, step)`. The
`moneyness_depth` DOUBLE column is REMOVED end-to-end: row-struct field
dropped, writer stops emitting, CREATE DDL + ALTER-manifest entries
deleted, and a one-shot marker-gated `ALTER TABLE … DROP COLUMN
moneyness_depth` migration runs at ensure time (house
`index_constituency` marker pattern, `data/state/` marker). NEW
`contract` SYMBOL display column ("NIFTY 28 JUL 25000 CALL", Dhan-web
style) derived at write time from fields already on the row via the pure
`contract_label` fn. `classify_chain_legs` produces per-row
`row_labels: Vec<MoneynessStepLabel>` (replacing `row_depth`) +
`misaligned_rows`; all 4 write sites (Dhan/Groww boot legs + both
cadence executors) stamp the label. Misaligned (off-grid) strikes are a
LOUD path: `tv_moneyness_step_misaligned_total{feed}` + an edge-latched
CHAIN-02 `stage="moneyness_step_misaligned"` warn — never silent
rounding.

## Edge Cases

- Grid-ATM anchor strike → step 0 → bare "ATM" (label precedence
  unchanged); off-grid strike==spot degenerate → class ATM, NotAligned →
  bare "ATM".
- Off-grid ITM/OTM strike → NotAligned → bare "ITM"/"OTM" (direction
  honest, step honestly absent) + counter + latched warn.
- Rows beyond the ATM±10 plan (the operator screenshot's 232-step
  strike) label honestly (`ITM-232`), never clamped.
- `Aligned(0)` under an ITM/OTM class is structurally impossible (grid
  anchor ⇒ ATM precedence) — defensive bare-class arm, never "ITM-0".
- UNKNOWN rows (missing spot / unknown underlying / bad leg) keep the
  bare "UNKNOWN" label; step Invalid is the quiet path.
- i64 extremes: checked subtraction, 24-byte label buffer fits
  "OTM+" + 20 digits exactly.
- contract label: day without leading zero, month uppercase 3-letter,
  whole strikes as integers, CALL/PUT words; unknown leg falls back to
  the raw label; extreme/negative expiry timestamps stay total.
- Historic rows: pre-2026-07-20 rows keep bare 4-value labels and read
  NULL in `contract`; the depth column's historic values are DELETED by
  the DROP (explicitly ordered).
- Fresh table (post-2026-07-20 DDL): the DROP rejects with
  invalid-column → treated as already-migrated → marker written.

## Failure Modes

- QuestDB down at migration time → CHAIN-03 `stage="depth_column_drop"`
  error, marker NOT written, retried next boot; ensure/DDL never
  blocked.
- A missed downstream label consumer matching exactly "ITM"/"OTM" —
  workspace grep found NONE outside the module docs (`WHERE
  moneyness='ATM'` doc examples remain valid values); the RAM snapshot
  is enum-based and untouched.
- Misaligned vendor strikes silently rounded — impossible by
  construction (NotAligned is a distinct verdict; the label falls back
  bare and the loud counter/warn fires).
- Label/class disagreement — the consistency-law test pins Itm ⇒ index
  < 0, Otm ⇒ index > 0, grid-ATM ⇒ 0 over a grid sweep, and the glyph
  comes from the class.
- Parse-only-strike ratchet: all step arithmetic lives in common; the
  boot/persistence files only call it (`contract_label` formats, never
  computes, a strike).

## Test Plan

- Files: crates/common/src/moneyness.rs,
  crates/storage/src/option_chain_1m_persistence.rs,
  crates/app/src/option_chain_1m_boot.rs,
  crates/app/src/groww_option_chain_1m_boot.rs,
  crates/app/src/dhan_cadence_executor.rs,
  crates/app/src/groww_cadence_executor.rs
- Tests: test_moneyness_step_index_ce_pe_sign_convention (4 quadrants,
  ATM 0, ±10 plan edges, screenshot ITM-232/OTM+232 on the real
  BANKNIFTY grid), test_moneyness_step_index_guards_misalignment_and_extremes
  (Invalid guards, NotAligned never rounded, i64 totality),
  test_moneyness_step_label_law (exact literals ITM-1/ITM-2/OTM+1/OTM+2
  /ATM/UNKNOWN/bare fallbacks/buffer extremes),
  test_moneyness_step_sign_is_consistent_with_classification (grid
  sweep + label glyph round-trip),
  test_chain1m_combined_label_no_depth_column_and_contract_tag,
  test_contract_label_format_law (exact literals incl. BANKNIFTY +
  SENSEX + fractional strike + no-leading-zero day).
- Gates: `cargo test -p tickvault-common`, FULL
  `cargo test -p tickvault-storage`, `cargo test -p tickvault-app`,
  `cargo fmt --check`, `bash .claude/hooks/pre-push-gate.sh`.

## Rollback

Single-commit revert restores the Jul-17 depth column semantics in code;
the live table's dropped `moneyness_depth` column would be re-added
empty by the reverted self-heal manifest (historic depth values are
unrecoverable — their deletion is the operator's explicit order, stated
in the PR body). The marker file gates re-runs; deleting it re-attempts
the (idempotent) DROP. No DEDUP key change anywhere — revert is a pure
`git revert` plus optional marker cleanup.

## Observability

No new ErrorCode: the misaligned-strike loud path reuses CHAIN-02
(`stage="moneyness_step_misaligned"`, edge-latched per feed/underlying/
day — the house moneyness_atm_absent/step_drift precedent) +
`tv_moneyness_step_misaligned_total{feed}`; the migration failure path
reuses CHAIN-03 (`stage="depth_column_drop"` on
`tv_chain1m_persist_errors_total`). Both log-sink-only per the rest-1m
§3 delivery boundary. Post-merge verification:
`select moneyness, contract, count(*) from option_chain_1m where ts > dateadd('h', -1, now()) group by moneyness, contract`
— labels must read ITM-1/OTM+1 style from the merge minute and
`moneyness_depth` must be absent from `SHOW COLUMNS`.

## Plan Items

- [x] Rule first: rewrite the 2026-07-20 section to the final
      combined-label law (honest intra-day spec evolution) — Files:
      .claude/rules/project/rest-1m-pipeline-error-codes.md
- [x] common: StepIndexOutcome + moneyness_step_index +
      MoneynessStepLabel + moneyness_step_label; depth fns removed —
      Files: crates/common/src/moneyness.rs — Tests:
      test_moneyness_step_index_ce_pe_sign_convention,
      test_moneyness_step_index_guards_misalignment_and_extremes,
      test_moneyness_step_label_law,
      test_moneyness_step_sign_is_consistent_with_classification
- [x] app: classify_chain_legs emits row_labels + misaligned_rows; the 4
      write sites stamp the label; misaligned counter + latched warn —
      Files: crates/app/src/option_chain_1m_boot.rs,
      crates/app/src/groww_option_chain_1m_boot.rs,
      crates/app/src/dhan_cadence_executor.rs,
      crates/app/src/groww_cadence_executor.rs — Tests:
      test_classify_chain_legs_realistic_chain (updated),
      test_record_chain_moneyness_observability_smoke (updated)
- [x] storage: row struct label type; depth field/DDL/manifest removed;
      contract column + contract_label; marker-gated DROP COLUMN
      migration — Files:
      crates/storage/src/option_chain_1m_persistence.rs — Tests:
      test_chain1m_combined_label_no_depth_column_and_contract_tag,
      test_contract_label_format_law
