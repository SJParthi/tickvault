# Implementation Plan: Groww Auto-Scale Ladder — Plateau-Stop Gate

**Status:** APPROVED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator) — exam-fix directive 2026-07-06

> Guarantee matrices: this item cross-references the canonical 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` (per that
> file's cross-reference clause). Cold-path-only change — no hot-path
> allocation, no new tick-drop path, DEDUP keys untouched.

## Design

Evidence from the 2026-07-06 exam TSV: the fleet's subscribe-proof (ACK)
count pinned at 33 while the ladder kept ADVANCING to rungs 40/80/86;
pushing past the server's cap DEGRADED healthy connections (capturing fell
30→24) and multiplied reject churn. The ladder must DISCOVER the cap and
STOP AT it, not climb past it.

Changes (crates: `app` = `tickvault-app`, `storage` = `tickvault-storage`,
`api` = `tickvault-api` doc-comment only):

1. `crates/app/src/groww_scale_ladder.rs` — a PURE plateau-decision
   function `evaluate_plateau(baseline_proof, current_proof,
   pinned_evals_so_far, last_efficient_rung) -> PlateauVerdict`
   (`AdvanceAllowed` / `Watching` / `Plateau { measured_cap, rollback_to }`),
   confirmation constant `PLATEAU_CONFIRM_EVALS = 2`. New FSM state
   `LadderState::HaltedAtPlateau` (gauge 5, label `halted_at_plateau`) +
   event `LadderEvent::PlateauReached` (`Holding → HaltedAtPlateau`;
   sticky except failures — mirrors `HaltedAtCeiling`).
2. Runner wiring in the `Holding` arm: capture the fleet subscribe-proof
   count (status-file evidence, the same per-conn source the `/feeds`
   panel uses via `count_subscribe_proofs`) as a BASELINE when an advance
   dispatches; on subsequent Holding evaluations, if proof fails to grow
   past the baseline the advance stays BLOCKED (`Watching`); after 2
   consecutive non-growing evaluations (or an immediate REGRESSION), the
   ladder rolls back to the last efficient rung (last rung where
   proof >= conns, floor 1), records `groww_scale_audit` outcome
   `halted_at_plateau` with reason `plateau_cap_<N>`, emits ONE
   Telegram-routed `error!` (GROWW-SCALE-01 code, reason label `plateau`
   on `tv_groww_scale_rollbacks_total`): "ladder stopped at the server's
   limit: N connections acknowledged — holding there", and halts sticky.
3. `crates/storage/src/groww_scale_audit_persistence.rs` — new
   `ScaleOutcome::HaltedAtPlateau` (`halted_at_plateau`), classified
   verified-healthy so restart rehydration resumes AT the efficient rung
   (the measured cap lands in the audit row: reason + `to_conns`).
4. Summary TSV: pure `format_scale_summary_tsv` + best-effort writer to
   `<shards_root>/scale-summary-<IST-date>.tsv` (`metric\tvalue` lines,
   mirrors the parity comparer TSV) carrying `measured_cap` +
   `efficient_rung` so future runs can start near the cap.
5. `crates/app` `HEALTHY_OUTCOMES` gains `halted_at_plateau`;
   `crates/api/src/feed_state.rs` doc comment lists the new ladder label.

Existing GROWW-SCALE-01 (per-rung rollback) and GROWW-SCALE-02 (global
halve) semantics are UNCHANGED — the plateau gate only intercepts the
advance decision inside the Holding arm.

## Edge Cases

- SMOKE mode (market closed): the plateau gate is SKIPPED — no live
  subscribe evidence exists by design; a plateau verdict there would be a
  false signal.
- No advance yet (baseline `None`, e.g. resume at rung 1): the first
  advance is always allowed; the gate arms only after an advance.
- Regression (proof < baseline): plateau declared IMMEDIATELY (no window
  wait) — with FRESHNESS-gated counting (hardening below) this means a
  previously-healthy connection stopped re-writing its status for over
  `PLATEAU_PROOF_FRESHNESS_SECS` — the exam's real capture-degradation
  signature (capturing fell 30→24), now an actually-reachable arm.
- `last_efficient_rung == 0` (proof never reached conns): rollback target
  floors at 1 (never 0 connections).
- Stale status files (HARDENED 2026-07-06 after hostile review): the bare
  `exists()` count was monotone-nondecreasing across halves, restarts and
  days (undated path, never deleted in production) — a deterministic
  false-plateau latch, NOT mere noise. Three closures: (a)
  `count_subscribe_proofs` now counts only files (re)written within
  `PLATEAU_PROOF_FRESHNESS_SECS` (300s; the sidecar re-writes ≤1s apart
  while streaming — `note_emit` in `groww_sidecar.py` — and once at
  subscribe, so healthy conns always count); (b) fleet scale-down deletes
  the killed conn's status file (`remove_subscribe_proof` in
  `groww_bridge.rs`, called from the reconciler's KillNewestTo arm); (c)
  the ladder sweeps all status files at start
  (`remove_stale_subscribe_proofs`). Residual: a dying sidecar can
  re-write its file in the ~1s between abort and reap AFTER the delete —
  bounded by the 300s freshness expiry; documented, never a panic.
- Slow-but-healthy spawn wave (HARDENED 2026-07-06): a plateau is
  declared only after the post-advance watch spans
  `plateau_confirm_window_secs(gate_hold_minutes)` =
  `max(gate_hold_minutes*60, 300)` seconds AND `PLATEAU_CONFIRM_EVALS`
  consecutive pinned evaluations — a 60s token-read floor or a
  minutes-long venv re-provision can no longer produce a false sticky
  plateau in two 30s ticks. Honest envelope: a spawn wave slower than the
  full confirm window (default 900s) can still false-plateau; the
  gate-failure escape (`HaltedAtPlateau` + GateFailed → RollingBack)
  remains the recovery path.
- Gate failure from `HaltedAtPlateau` still rolls back (GROWW-SCALE-01
  path unchanged); after recovery the ladder may re-climb and re-measure
  the cap — bounded churn, each climb re-measures honestly.
- `measured_cap` = `max(baseline, current)` (equal on pinned, baseline on
  regression) — the highest count the server ever acknowledged.

## Failure Modes

- Audit write failure at the plateau transition → existing GROWW-SCALE-04
  best-effort contract (error! + counter, ladder continues in-memory;
  DEDUP-idempotent re-append).
- Summary TSV write failure → `error!` log (best-effort; the measured cap
  still lands in the audit row, which is the rehydration source).
- Restart mid-plateau: rehydration reads the newest verified-healthy row;
  `halted_at_plateau`'s `to_conns` = the efficient rung, so the next run
  starts near the measured cap instead of re-climbing blind.
- Fleet-wide failure while plateaued: GlobalFailure preempts every state
  (unchanged) → cooldown + halve.

## Test Plan

Unit tests (22-category coverage: unit + adversarial boundary), scoped per
`testing-scope.md` to the touched crates:

- `cargo test -p tickvault-app --lib --tests` —
  `test_plateau_growing_proof_allows_advance`,
  `test_plateau_pinned_two_evals_declares_plateau`,
  `test_plateau_pinned_requires_confirm_window_elapsed`,
  `test_plateau_confirm_window_is_hold_window_with_floor`,
  `test_plateau_regression_immediate_rollback_to_last_efficient`,
  `test_plateau_rollback_floor_is_one`,
  `test_plateau_confirm_evals_is_two`,
  `test_proof_is_fresh_boundaries`,
  `test_count_subscribe_proofs_ignores_stale_files`,
  `test_remove_stale_subscribe_proofs_clears_status_files`,
  `test_remove_subscribe_proof_removes_only_the_target_conn`,
  `test_fsm_plateau_reached_from_holding_and_sticky`,
  `test_resume_honors_halted_at_plateau`,
  `test_format_scale_summary_tsv_carries_measured_cap`,
  plus the extended FSM totality/gauge/gate-failure tests.
- `cargo test -p tickvault-storage --lib --tests` — extended
  `test_outcome_as_str_stable` + `test_verified_healthy_classification`.
- `cargo test -p tickvault-api --lib --tests` — doc-comment-only change,
  suite re-run for safety.
- `cargo fmt --check` + banned-pattern scanner.

## Rollback

Single revert of this branch's commit restores the prior ladder behavior
(climb-to-ceiling). No schema migration: `halted_at_plateau` is a new
SYMBOL value in an existing column — old readers ignore it; rehydration
falls back to the first rung when no known-healthy row parses (fail-soft,
unchanged). The summary TSV is an additive artifact; deleting it has no
runtime effect.

## Observability

- Audit row: `groww_scale_audit` outcome `halted_at_plateau`, reason
  `plateau_cap_<N>`, `to_conns` = efficient rung (DEDUP-keyed, feed-in-key
  unchanged).
- Metric: `tv_groww_scale_rollbacks_total{reason="plateau"}` (static
  label).
- Gauge: `tv_groww_ladder_state = 5` (`halted_at_plateau`).
- Telegram: ONE `error!` (GROWW-SCALE-01 code field) at the plateau
  transition — sticky state guarantees single emission per episode.
- Panel: `/api/feeds/health` `groww_scale.ladder_state` shows
  `halted_at_plateau`.
- Summary TSV: `data/groww/scale-summary-<date>.tsv` with `measured_cap`.

## Plan Items

- [x] Pure plateau decision fn + verdict enum + constant
  - Files: crates/app/src/groww_scale_ladder.rs
  - Tests: test_plateau_growing_proof_allows_advance, test_plateau_pinned_two_evals_declares_plateau, test_plateau_regression_immediate_rollback_to_last_efficient
- [x] FSM state/event + runner wiring + summary TSV
  - Files: crates/app/src/groww_scale_ladder.rs, crates/api/src/feed_state.rs
  - Tests: test_fsm_plateau_reached_from_holding_and_sticky, test_format_scale_summary_tsv_carries_measured_cap, test_resume_honors_halted_at_plateau
- [x] Storage outcome variant
  - Files: crates/storage/src/groww_scale_audit_persistence.rs
  - Tests: test_outcome_as_str_stable, test_verified_healthy_classification
- [x] Hardening 2026-07-06 (hostile review): freshness-gated proof count +
  status-file deletion on scale-down + ladder-start sweep + hold-window
  plateau confirmation
  - Files: crates/app/src/groww_scale_ladder.rs, crates/app/src/groww_bridge.rs, crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_proof_is_fresh_boundaries, test_count_subscribe_proofs_ignores_stale_files, test_remove_stale_subscribe_proofs_clears_status_files, test_remove_subscribe_proof_removes_only_the_target_conn, test_plateau_pinned_requires_confirm_window_elapsed, test_plateau_confirm_window_is_hold_window_with_floor
