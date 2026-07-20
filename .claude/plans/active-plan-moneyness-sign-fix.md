# Implementation Plan: moneyness_depth sign flip — ITM positive (operator ruling 2026-07-20)

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) — standing pre-authorization, ruling 2026-07-20 ("ITM depth must be POSITIVE, OTM NEGATIVE — flip it"; live evidence: spot 57800.9 / strike 81000 showed ITM=−23199.1 / OTM=+23199.1)

## Design

The signed `moneyness_depth` companion column (2026-07-17) shipped with the
inverted sign convention (ITM negative). The single arithmetic home is
`crates/common/src/moneyness.rs::moneyness_depth_paise` (tickvault-common
crate). The flip inverts both match arms — CE: `spot − strike`,
PE: `strike − spot` — so `classify == Itm ⇒ depth > 0` and
`Otm ⇒ depth < 0`. All downstream code (`option_chain_1m_boot.rs`
`classify_chain_legs`, both cadence executors, the Groww boot leg, the
storage `OptionChain1mRow`) is a pure pass-through of the fn's output —
no sign-dependent logic exists downstream (verified by grep), so only doc
comments + one storage sample-row fixture change there. The 2026-07-17
ruling section in `rest-1m-pipeline-error-codes.md` is bannered SUPERSEDED
(text kept for audit) and a dated 2026-07-20 section records the new law.

## Edge Cases

- ATM-labeled grid strike still carries its signed distance (label
  semantics untouched); the sample 24550-vs-24536.40 CE depth flips
  +13.60 → −13.60.
- Strike paise-exactly at spot → Some(0), both legs (unchanged).
- One-paise boundary: smallest nonzero distance carries the correct sign
  in all 4 quadrants (new test).
- i64 extremes: checked_sub arms swapped; totality unchanged (both
  operands ≥ 1 bound the result by ±(i64::MAX − 1)).
- NULL semantics unchanged: unparsable leg / guarded-invalid operands →
  None → column unwritten.
- Historical rows (2026-07-17 → merge minute) keep the OLD sign — label
  column, never backfilled; rows self-correct from the merge minute.

## Failure Modes

- A missed downstream sign consumer would silently invert meaning — the
  workspace grep for `moneyness_depth` / `leg_depth` / `row_depth` found
  ONLY pass-throughs (persistence, snapshot struct docs, ILP stamping);
  no classification derivation, portal, or digest reads the sign.
- The consistency-law test (`test_moneyness_depth_sign_is_consistent_with_classification`)
  fails the build if the label and the depth sign ever disagree again.
- Mixed-sign rows across the cutover minute are an accepted, documented
  audit artifact (rule-file 2026-07-20 section).

## Test Plan

- Files: crates/common/src/moneyness.rs, crates/storage/src/option_chain_1m_persistence.rs
- Tests re-pinned to the new law:
  - `test_moneyness_depth_paise_ce_pe_sign_convention` (4-quadrant + ATM
    distance + the operator-screenshot pair 57800.9/81000)
  - `test_moneyness_depth_sign_is_consistent_with_classification`
    (stored-depth consistency law: Itm ⇒ >0, Otm ⇒ <0)
  - `test_moneyness_depth_paise_guards_and_overflow_boundary` (extremes swapped)
  - NEW `test_moneyness_depth_one_paise_boundary` (one-tick boundary, 4 quadrants)
  - `test_chain1m_append_row_stamps_moneyness_depth_when_some_omits_when_none`
    (storage fixture −7.2)
- Gates: `cargo test -p tickvault-common`, `cargo test -p tickvault-storage --lib`,
  `cargo fmt --check`, `bash .claude/hooks/pre-push-gate.sh`.

## Rollback

Single-commit revert restores the 2026-07-17 convention exactly (the fn,
docs, tests, and rule-file section are all in one commit). Rows written
under the new convention would then carry the flipped sign — the same
bounded cutover-artifact class, in reverse. No schema change, no DEDUP
key change, no config change — revert is a pure `git revert`.

## Observability

No new ErrorCode, counter, or alert — the column is an audit label
(NOT in the DEDUP key, ILP-sparse), and the flip changes no failure
mode. The existing chain-leg observability (CHAIN-02 family,
`tv_chain1m_*` counters) is untouched. The consistency-law ratchet is
the mechanical regression guard; the rule-file 2026-07-20 section is
the operator-facing record. Verification query post-merge:
`select moneyness, moneyness_depth from option_chain_1m where feed='dhan' order by ts desc limit 20`
— ITM rows must read positive from the merge minute.

## Plan Items

- [x] Rule first: banner the 2026-07-17 section SUPERSEDED + add the dated
      2026-07-20 law — Files: .claude/rules/project/rest-1m-pipeline-error-codes.md
- [x] Flip both arms of `moneyness_depth_paise` + doc/decision table —
      Files: crates/common/src/moneyness.rs — Tests: test_moneyness_depth_paise_ce_pe_sign_convention
- [x] Re-pin ratchet + consistency-law + extremes; add one-paise boundary
      test — Files: crates/common/src/moneyness.rs — Tests: test_moneyness_depth_one_paise_boundary,
      test_moneyness_depth_sign_is_consistent_with_classification,
      test_moneyness_depth_paise_guards_and_overflow_boundary
- [x] Update downstream doc comments + storage sample fixture —
      Files: crates/app/src/option_chain_1m_boot.rs,
      crates/storage/src/option_chain_1m_persistence.rs —
      Tests: test_chain1m_append_row_stamps_moneyness_depth_when_some_omits_when_none
