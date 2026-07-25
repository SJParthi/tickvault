# Implementation Plan: Cadence adversarial-verification fix PR (H1+H2partial+H3+M2+M4+M9+M11 code; H4/H5/M13 rule constraints)

**Status:** IN_PROGRESS
**Date:** 2026-07-20
**Approved by:** Parthiban (operator) via coordinator relay 2026-07-20 — "fix scope locked": audit verdict 0 CRITICAL; code scope H1+H2partial+H3+M2+M4+M9(+M11 if contained); H4/H5/M13 land as binding rule-file constraints for the T+4s build.

Audit basis: the 6-dimension adversarial verification of the REST cadence engine
(scratchpad `cadence-audit/VERDICT.md` + dimA..dimF tables, 2026-07-20). Crates
touched: `tickvault-core` (`crates/core/src/cadence/{assembly,runner}.rs`),
`tickvault-app` (`crates/app/src/{dhan_cadence_executor,groww_cadence_executor,dhan_intraday_parse}.rs`).

## Design

- **H1** — malformed/wrong-shape 2xx JSON on the SPOT legs collapsed to the
  benign `Empty` class. Fix: pure discriminators — Dhan
  `zero_candle_body_is_malformed` (`dhan_intraday_parse.rs`: only valid JSON
  with all 6 columnar arrays at equal length ZERO is well-formed-empty;
  anything else that parses to zero candles — incl. the 2026-07-15
  float-timestamp incident shape — is malformed) consumed by
  `dhan_spot_zero_candle_class`; Groww `groww_spot_zero_candle_class`
  (`malformed_rows > 0` on a zero-candle parse). Malformed ⇒ audit `Error`
  class `malformed_body` + `CadenceFetchError::Malformed` (non-arming terminal
  → the existing `fetch_failed` stage; zero runner changes).
- **H2 partial** — response-vs-request echo validation is VERIFIED structurally
  impossible on all 4 legs (no echo fields exist). Compensating plausibility =
  the H3 bands; Limitation documented in `cadence-error-codes.md` §0d.
- **H3 (+M14 absorbed)** — chain minute stamp is the request's own stamp
  (vendor-stale chains pass freshness; no vendor timestamp exists). Honest
  proxy: `spots_diverge_paise` in `assembly.rs` (integer BPS compare,
  `CADENCE_SPOT_DIVERGENCE_MAX_BPS = 50` — the OPTION-CHAIN-04 0.5%
  precedent; non-positive operands ⇒ no verdict). Two ADVISORY sites in
  `runner.rs`: `decide_lane` chain-embedded vs lane spot ⇒
  `chain_spot_divergence` flag + `tv_cadence_chain_spot_divergence_total`;
  completion path own-vs-other-lane OwnFetch spots ⇒
  `cross_source_spot_divergence` flag +
  `tv_cadence_cross_source_spot_divergence_total`. Never blocking, never
  arming.
- **M2** — Dhan chain `legs.is_empty()` gains the Groww-parity split via
  `dhan_chain_empty_class` (`strike_count + invalid_strikes > 0` ⇒
  `leg_shape_drift`/`Error`/`Malformed`, else `empty_chain`/`Empty`).
- **M4** — `cross_fill_from` spot arm refuses a donor with `spot_paise <= 0`.
- **M9** — pure `unaccounted_session_tail(last_boundary)`; the day loop's
  no-joinable-boundary arm accounts the dropped session tail ONCE per day
  (latched, day-flip reset): counter += missed + CADENCE-03
  `final_boundary_missed` error!.
- **M11 (contained)** — the runner's outer `tokio::time::timeout` bound in
  `bound_spot_fetch`/`bound_chain_fetch` becomes executor budget +
  `CADENCE_EXECUTOR_TAIL_GRACE_MS` (1_500ms); the executor's own
  `deadline_epoch_ms` is unchanged (network Timeout still typed there); the
  outer cancel becomes a wedge backstop that can no longer sever a completed
  persist from its audit append + fold handoff.
- **H4/H5/M13** — no code; binding constraints in `cadence-error-codes.md`
  §0d for the T+4s retry build (gate routing for every retry; chain offsets ≥
  the 3s per-key gate — T+4000 is the earliest legal; race classes pinned;
  re-pull boundary-collision + ladder/429 interaction stated; pre-warm names
  endpoint + budget authority + no-REST-lock KEEP row or TLS-only).

## Edge Cases

- Discriminator: n>0 body with every row value-unparseable ⇒ malformed; n==0
  well-formed ⇒ empty; non-JSON / missing arrays / length mismatch ⇒
  malformed. Groww GA-FAILURE envelope keeps its earlier Transport arm (sniff
  precedes the parser).
- Band: either operand ≤ 0 ⇒ false; exactly-at-band ⇒ false (strictly
  greater fires); saturating i64 math.
- Cross-source check: OwnFetch-vs-OwnFetch same-minute cells only.
- M9: `last_boundary = None` (post-close boot) never claims a tail; latch
  resets at the IST day flip; non-trading days never fire.
- M11: `timeout_ms.max(1)` clamp preserved; grace saturating.

## Failure Modes

- False-malformed on a legit empty day would create `fetch_failed` noise —
  prevented by the strict well-formed-empty carve-out + fixture tests.
- Divergence flap on vendor lag — advisory only (stage + counter; no
  decision/ladder/page change).
- M9 double-count with in-session `boundary_skipped` — impossible (the None
  arm only runs when no joinable boundary exists; once-per-day latch).
- M11 delays wedge detection by ≤1.5s per leg — bounded, documented; gate/
  dispatch instants unchanged (zero-429 properties untouched).
- All new counters use static labels (no allocation at emission).

## Test Plan

- `dhan_intraday_parse.rs::zero_candle_body_discriminates_malformed_vs_empty`
- `dhan_cadence_executor.rs::dhan_spot_zero_candle_class_splits_malformed`,
  `dhan_chain_empty_vs_leg_shape_drift_split`
- `groww_cadence_executor.rs::groww_spot_zero_candle_malformed_vs_empty`
- `assembly.rs::spots_diverge_paise_band_boundaries`,
  `cross_fill_refuses_nonpositive_donor_spot`
- `runner.rs::unaccounted_session_tail_cases`,
  `executor_tail_grace_is_bounded_and_additive`,
  `divergence_flags_alone_never_degrade` +
  `real_degrade_excludes_divergence_stages` (divergence is advisory
  info-level only — decoupled from `any()`/`stages()` per the 2026-07-20
  adversarial review; supersedes the drafted
  `stages_include_divergence_flags`)
- Scoped: `cargo test -p tickvault-core -p tickvault-app`; full battery via CI
  (All Green on the exact final head per merge-gate-lock §3.2).

## Rollback

Single revert of the squash-merge restores prior behavior; no schema/config/
state changes (stages, counters, and audit `class` values are additive; the
M11 grace is one constant).

## Observability

New counters `tv_cadence_chain_spot_divergence_total{lane,underlying}` +
`tv_cadence_cross_source_spot_divergence_total{underlying}`; existing
`tv_cadence_boundary_skipped_total` reused for the tail; new flags
`chain_spot_divergence` / `cross_source_spot_divergence` (advisory
info-level only — decoupled from the CADENCE-01 degrade stages/`any()`
per the 2026-07-20 adversarial review; coalesced `info!` + counters) +
`final_boundary_missed` (CADENCE-03); new audit class `malformed_body`
(outcome `Error`). All documented in `cadence-error-codes.md` §0d + the two
stage tables in the same PR.

## Plan Items

- [x] H1 — malformed-vs-empty split on both spot legs
  - Files: crates/app/src/dhan_intraday_parse.rs, crates/app/src/dhan_cadence_executor.rs, crates/app/src/groww_cadence_executor.rs
  - Tests: zero_candle_body_discriminates_malformed_vs_empty, dhan_spot_zero_candle_class_splits_malformed, groww_spot_zero_candle_malformed_vs_empty
- [x] H2 partial — echo-validation Limitation documented
  - Files: .claude/rules/project/cadence-error-codes.md
  - Tests: (docs; compensating band tested under H3)
- [x] H3 (+M14) — advisory spot-divergence band, both sites
  - Files: crates/core/src/cadence/assembly.rs, crates/core/src/cadence/runner.rs
  - Tests: spots_diverge_paise_band_boundaries, stages_include_divergence_flags
- [x] M2 — Dhan chain empty-vs-drift split
  - Files: crates/app/src/dhan_cadence_executor.rs
  - Tests: dhan_chain_empty_vs_leg_shape_drift_split
- [x] M4 — paise-0 donor cross-fill guard
  - Files: crates/core/src/cadence/assembly.rs
  - Tests: cross_fill_refuses_nonpositive_donor_spot
- [x] M9 — final-boundary tail accounting
  - Files: crates/core/src/cadence/runner.rs
  - Tests: unaccounted_session_tail_cases
- [x] M11 — executor persist-tail grace on the runner's outer bound
  - Files: crates/core/src/cadence/runner.rs
  - Tests: executor_tail_grace_is_bounded_and_additive
- [x] H4/H5/M13 — T+4s binding constraints + stage/class doc rows
  - Files: .claude/rules/project/cadence-error-codes.md
  - Tests: (rule file; enforced in review per §0d wording)

## Guarantee matrices

Per-item 15-row + 7-row matrices: cross-referenced to
`.claude/rules/project/per-wave-guarantee-matrix.md` (classification/
visibility hardening; zero hot-path allocation — all new checks are O(1)
integer compares on the cold per-minute decide path; DEDUP keys, WAL/ring/
spill chain, WS locks untouched). Honest 100% claim: 100% inside the tested
envelope, with ratcheted regression coverage — the new classifiers and bands
are pure functions with boundary unit tests; beyond the envelope the existing
fetch-audit forensic chain records every raw outcome.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan float-timestamp wire drift recurs (2026-07-15 class) | `malformed_body`/Malformed → `fetch_failed`, never 14 silent `empty_no_rows` days |
| 2 | Vendor-stale Dhan chain, fresh own spot | >0.5% embedded-vs-own ⇒ `chain_spot_divergence` stage + counter; decision proceeds |
| 3 | Cross-broker spot divergence (both OwnFetch) | `cross_source_spot_divergence` stage + counter |
| 4 | Paise-0 guard-failed donor spot | cross-fill refused; borrower skips honestly |
| 5 | Runner stalls 15:28→15:35 IST | counter += tail + one CADENCE-03 `final_boundary_missed`; day-flip re-arms |
| 6 | Spot persist completes at deadline−1ms | grace lets audit row + fold complete; no persisted-but-Timeout mislabel |
