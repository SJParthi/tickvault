# Implementation Plan: Per-Row Moneyness (ITM/ATM/OTM/UNKNOWN) — RAM-first decision source + write-time DB audit column

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator directive 2026-07-14, relayed verbatim via the coordinator session)

Operator directive (2026-07-14, relayed verbatim via the coordinator session):
moneyness classification (ITM/ATM/OTM/UNKNOWN) is CRITICAL and MANDATORY on
every option-chain and option-contract row — O(1) time + O(1) space + zero
allocation, computed in the RAM hot path AND stored precomputed in the DB;
the DB column is AUDIT-ONLY, the RAM snapshot is the decision source of
truth. ATM is GRID ROUNDING (round-half-up to the strike grid: NIFTY step
Rs.50, BANKNIFTY Rs.100, SENSEX Rs.100), never a strike-list scan.

Per-wave guarantee matrices: cross-reference per-wave-guarantee-matrix.md (15-row + 7-row matrices apply to this item).

Honest 100% claim: 100% inside the tested envelope, with ratcheted regression
coverage — total pure classifier (proptest never-panics + boundary tests +
CE/PE direction table pinned to the 2026-04-21 live capture: NIFTY spot
24536.40 → ATM 24550), DHAT zero-alloc proof (10K classifies + 10K snapshot
reads), Criterion budget `moneyness = 50` ns, ILP-line SYMBOL stamp tests on
both tables, RAM-first source-scan ratchet, wiring assertions on all 3 boot
legs. NOT claimed: BANKNIFTY/SENSEX strike steps as repo-verified facts
(operator directive is the authority; the step-drift counter is the live
cross-check); the per-minute chain-snapshot BUILD allocates on the cold
scheduled fire by design (the per-row classify + RAM read stay zero-alloc);
contract-leg moneyness is anchor-relative (≤5-min chain anchor), not
candle-minute-exact; old rows stay NULL forever (never backfilled).

## Plan Items

- [x] Item 1 — Moneyness enum + grid-rounded ATM + classifier + step table (crates/common)
  - Files: crates/common/src/moneyness.rs, crates/common/src/lib.rs
  - Tests: test_moneyness_as_str_and_parse_round_trip_for_every_variant, test_moneyness_labels_unique, test_classify_moneyness_ce_pe_direction_table, test_classify_moneyness_zero_spot_is_unknown, test_classify_moneyness_negative_spot_and_strike_is_unknown, test_classify_moneyness_nan_inf_extreme_price_overflow_guard, test_atm_strike_paise_midpoint_tie_half_up_boundary, test_atm_strike_paise_zero_negative_odd_step_is_none, test_price_to_paise_guarded_boundary, proptest_classify_moneyness_invariants
- [x] Item 2 — RAM chain-moneyness snapshot registry (crates/core/src/pipeline/chain_snapshot.rs)
  - Files: crates/core/src/pipeline/chain_snapshot.rs, crates/core/src/pipeline/mod.rs
  - Tests: test_publish_then_load_roundtrip, test_slot_isolation_across_feeds_and_underlyings, test_empty_sentinel_before_first_publish, test_age_secs_boundaries
- [x] Item 3 — option_chain_1m: moneyness SYMBOL column (manifest 23→24, .symbol stamp next to leg)
  - Files: crates/storage/src/option_chain_1m_persistence.rs
  - Tests: test_chain1m_append_row_stamps_moneyness_symbol (+ test_option_chain_1m_create_ddl_contains_expected_columns auto-covers via the manifest)
- [x] Item 4 — option_contract_1m_rest: moneyness SYMBOL + underlying_spot DOUBLE (CREATE + inline ALTER array + DDL test)
  - Files: crates/storage/src/option_contract_1m_rest_persistence.rs
  - Tests: test_contract1m_append_row_stamps_moneyness_and_underlying_spot, test_option_contract_1m_rest_create_ddl_contains_expected_columns extended
- [x] Item 5 — Boot-leg wiring: Dhan chain + Groww chain (classify per row, publish snapshot, counters, edge-latched warns) + Groww contract (classify from anchor, stamp underlying_spot, DB-only)
  - Files: crates/app/src/option_chain_1m_boot.rs, crates/app/src/groww_option_chain_1m_boot.rs, crates/app/src/groww_contract_1m_boot.rs
  - Tests: per-leg unit tests on the wiring helpers; crates/core/tests/chain_snapshot_ram_first_guard.rs wiring assertions
- [x] Item 6 — Proof artifacts: Criterion bench + budget, DHAT zero-alloc test, RAM-first ratchet
  - Files: crates/core/benches/moneyness.rs, crates/core/Cargo.toml, quality/benchmark-budgets.toml, crates/core/tests/dhat_moneyness.rs, crates/core/tests/chain_snapshot_ram_first_guard.rs
  - Tests: dhat_moneyness_classify_and_snapshot_read_zero_alloc, test_snapshot_module_has_no_db_or_network_tokens, test_scanner_detects_planted_token, test_banned_pattern_scanner_still_covers_strategy_selects, test_boot_legs_call_classify_and_chain_legs_publish_snapshot
- [x] Item 7 — Docs: rule-file §2f note + plan archives (spot-1m-diagnostics 2026-07-14, questdb-partition-s3-archive 2026-07-13)
  - Files: .claude/rules/project/rest-1m-pipeline-error-codes.md, .claude/plans/archive/2026-07-14-spot-1m-diagnostics.md, .claude/plans/archive/2026-07-13-questdb-partition-s3-archive.md
  - Tests: n/a (docs; the rule-file trigger list gains moneyness/chain_snapshot tokens)

## Design

- Pure math lives ONLY in `crates/common/src/moneyness.rs` (new module):
  `Moneyness` enum (Itm/Atm/Otm/Unknown) with `const fn as_str()` →
  "ITM"/"ATM"/"OTM"/"UNKNOWN" (the feed.rs `as_str` pattern);
  `strike_step_paise(underlying_symbol)` const table NIFTY→5_000,
  BANKNIFTY→10_000, SENSEX→10_000, else None; guarded rupees→paise
  conversion (finite, >0, < 1e7 rupees, ≥1 paise — else None);
  `atm_strike_paise(spot_paise, step_paise) = ((spot + step/2) / step) * step`
  — round-half-UP grid rounding (midway → higher strike), NEVER a scan
  (operator mandate); two-step API — ATM computed ONCE per (underlying,
  minute), then a per-row `classify_moneyness_paise(leg, strike_paise,
  spot_paise, atm_paise)` taking the precomputed ATM — plus the guarded f64
  convenience wrappers `classify_moneyness(leg, strike, spot, step_paise)`
  and `classify_moneyness_for(underlying, leg, strike, spot)`.
  Decision order: invalid spot/strike/leg/step → UNKNOWN; strike==atm → ATM;
  strike==spot (paise-exact, off-grid degenerate) → ATM; CE strike<spot ITM
  / strike>spot OTM; PE strike>spot ITM / strike<spot OTM. All comparisons
  in integer paise, never f64 equality. #[inline], zero alloc.
- RAM decision surface: `crates/core/src/pipeline/chain_snapshot.rs` —
  `OnceLock`-backed fixed 6-slot array (Feed::COUNT 2 × 3 underlyings:
  NIFTY=0, BANKNIFTY=1, SENSEX=2) of `arc_swap::ArcSwap<ChainMoneynessSnapshot>`
  seeded with an empty sentinel (minute_ts 0). Snapshot carries minute ts,
  fetched-at, underlying_spot f64 + spot_paise, atm_strike_paise,
  spot_missing flag, expiry, and rows Vec of (strike_paise, leg, moneyness,
  ltp_paise). `publish_chain_snapshot` is cold-path (may allocate — honest
  envelope); `load_chain_snapshot` returns the arc-swap Guard — lock-free,
  O(1), zero-alloc; `age_secs(now)` + `is_empty_sentinel()` are the §38.8
  decision-freshness hooks. arc-swap is already a core dep.
- Storage: `option_chain_1m` row gains `moneyness: &'static str`, manifest
  bumps 23→24 with ("moneyness","SYMBOL") — the existing per-column
  ALTER-ADD-IF-NOT-EXISTS loop self-heals; ILP write `.symbol("moneyness",…)`
  next to the `leg` symbol (tags-before-fields). `option_contract_1m_rest`
  gains `moneyness` SYMBOL AND `underlying_spot` DOUBLE (audit recompute —
  the anchor spot used); CREATE string + inline ALTER array + DDL test all
  updated. NEITHER column in any DEDUP key (label columns, the
  contract_security_id precedent).
- App wiring (all three cold-path legs): pre-loop spot_paise + ATM once;
  per-row classify; post-loop counters + observed-step cross-check + RAM
  snapshot publish on the two CHAIN legs (feed=dhan / feed=groww); the
  contract leg classifies from the ≤5-min chain anchor and stamps
  `underlying_spot`, DB-only, NO snapshot publish (the chain snapshot one
  watch-signal earlier is the strict-superset decision surface). ALL
  strike/price arithmetic stays inside the common fns — the boot/persistence
  files contain only fn calls (the
  `ratchet_chain1m_strike_is_parse_only_never_computed` ratchet stands).

## Edge Cases

- Dhan spot silently absent (`val_f64` defaults `last_price` to 0.0, NO
  vendor flag) ⇒ every row UNKNOWN + spot_missing snapshot + unknown counter.
- Groww `underlying_ltp_missing` / ltp ≤ 0 ⇒ same UNKNOWN path (mirrors the
  anchor-store gate).
- Spot NaN / ±inf / negative / sub-paise (0.004) / implausible (>1e7 rupees)
  ⇒ UNKNOWN — the guard makes the saturating f64→i64 cast unreachable.
- Spot below half a step (grid-rounds to 0 — the 2026-07-14 proptest
  counterexample: spot ₹1.00 on the ₹50 grid) ⇒ atm_strike_paise None ⇒
  UNKNOWN — fail-closed, never a bogus 0-paise ATM.
- Spot exactly midway between strikes ⇒ half-up: the HIGHER strike is ATM,
  deterministically (proof comment in the module).
- Spot exactly ON a grid strike ⇒ that strike is ATM for both CE and PE.
- Off-grid strike exactly equal to spot (25700.5-class hostile input) ⇒ ATM
  (degenerate arm — numerically at-the-money; ITM/OTM would be a direction
  lie, UNKNOWN would hide a certain fact).
- Spot far outside the listed range / vendor hole at the computed ATM ⇒ zero
  rows label ATM that minute (never a silently promoted substitute) +
  `tv_moneyness_atm_absent_total` + one edge-latched warn per day.
- Unknown underlying / unknown leg / corrupt step (≤0 or odd) ⇒ UNKNOWN.
- Contract-leg anchor absent/stale (>5 min) ⇒ the existing anchor gate
  already skips the underlying; a reached row with no anchor ⇒ UNKNOWN +
  underlying_spot 0.0.
- Contract BACKFILL rows classify against the CURRENT anchor —
  anchor-relative, auditable via the persisted `underlying_spot`.
- Read before first publish (boot, pre-09:16) ⇒ empty sentinel,
  `is_empty_sentinel()` true, age enormous — fail-closed by construction.
- DEDUP re-append of the same key ⇒ moneyness becomes the LATEST run's value
  (KEY-idempotent, not VALUE-idempotent — acceptable for an audit mirror).

## Failure Modes

- Classification can NEVER fail a fetch/persist leg: total pure fn, no
  unwrap/expect (compile-denied), UNKNOWN is the worst outcome.
- ensure-DDL failure at boot: existing CHAIN-03/SPOT1M-02 `ensure_*` stage
  arms; the first ILP `.symbol()` write auto-creates the SYMBOL column
  server-side — degraded-but-correct, next boot self-heals.
- ILP flush reject: existing discard-pending + `tv_*_rows_discarded_total`
  + CHAIN-03/SPOT1M-02 `stage="flush"` — one fire's rows, bounded, loud.
- Const step wrong / NSE grid change: ATM label misplaced or absent —
  ITM/OTM UNAFFECTED (they never use the step); `tv_moneyness_step_drift_total`
  fires from minute 1; fix = one const edit PR.
- Vendor lists a hostile off-grid strike: spurious drift warn — itself a
  correct "vendor data is weird this minute" signal (documented, accepted).
- Snapshot writer task dies: existing supervisor respawn counters; the
  snapshot ages honestly (`age_secs` grows) — never masked.
- DB flush failure is irrelevant to RAM: the snapshot publishes before/
  alongside the flush; the DB column is a write-only audit mirror.
- No new ErrorCode: fail-soft warn-level classification (reuses CHAIN-02 /
  SPOT1M-01 stage taxonomy) — a new variant would drag the full
  runbook/cross-ref/tag-guard chain for a log-sink-only signal.

## Test Plan

- crates/common (floor 99.5): enum roundtrip + uniqueness; table-driven
  CE/PE direction ratchet with the real capture numbers (NIFTY 24536.40 →
  ATM 24550; BANKNIFTY 48143.25 → ATM 48100; SENSEX 81050.00 midpoint →
  half-up 81100); financial-test-guard-compliant boundary tests (zero,
  negative, NaN/Inf/extreme overflow guard, midpoint tie half-up, odd/zero
  step); ~6-property proptest (single-ATM on-grid, never-both-ITM, leg-flip
  maps ITM↔OTM fixing ATM/UNKNOWN, never-panics totality over any::<f64>(),
  paise round-trip invariance, ATM distance ≤ step/2).
- crates/core: snapshot publish/read, slot isolation, sentinel, age tests;
  DHAT `crates/core/tests/dhat_moneyness.rs` (10K classifies + 10K snapshot
  reads, dhat_feed_presence template envelope); RAM-first ratchet
  `crates/core/tests/chain_snapshot_ram_first_guard.rs` (forbidden DB/
  network tokens in moneyness.rs + chain_snapshot.rs, comment-aware +
  scanner self-test + banned-pattern-scanner Cat-10 pin + boot-leg wiring
  assertions).
- crates/storage: ILP-line stamp tests on both tables; DDL tests extended.
- crates/app: wiring helper unit tests; the existing
  `ratchet_chain1m_strike_is_parse_only_never_computed` stays green.
- Bench: `crates/core/benches/moneyness.rs` (`moneyness/classify`,
  `moneyness/snapshot_read`), budget `moneyness = 50` ns (bidirectional
  substring collision verified against every existing key and bench name).
- Local: cargo fmt/clippy -D warnings/test --workspace (common touched ⇒
  workspace scope; QuestDB offline ⇒ DB-live tests #[ignore]d as usual).

## Rollback

- No config flag (the rho/close_to_data_ms precedent: additive nullable
  label columns need no toggle; config gates are for PIPELINES).
- Rollback = revert the PR. The live columns are NEVER dropped; a reverted
  binary stops sending the tags and new rows return to NULL — the pre-PR
  state. Old classified rows keep their values (harmless audit residue).
- The RAM snapshot registry is passive (zero consumers — §28 boundary);
  reverting removes the publishers and reads fail closed on the sentinel.

## Observability

- Counters (static labels): `tv_moneyness_unknown_total{feed}` (per UNKNOWN
  row), `tv_moneyness_atm_absent_total{feed}` (per chain fire with rows>0
  and no ATM match), `tv_moneyness_step_drift_total{feed}` (per chain fire
  where the observed finest adjacent step ≠ the const step).
- One edge-latched coalesced warn per (feed, underlying, day) for drift and
  atm-absent, on the EXISTING CHAIN-02 code with new stages
  (`moneyness_step_drift` / `moneyness_atm_absent`) — the anchor_stale latch
  precedent; log-sink-only, no page, no new ErrorCode.
- Snapshot publish emits one `debug!` per fire (minute, atm_paise, rows,
  spot_missing) — log-sink only.
- Rule-file note: `.claude/rules/project/rest-1m-pipeline-error-codes.md`
  §2f (dated 2026-07-14) documents the column on both tables, UNKNOWN
  semantics, the three counters, the RAM-is-decision-source / DB-write-only
  split + its enforcement ratchet, and the trigger-list tokens.
