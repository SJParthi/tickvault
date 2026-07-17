# Implementation Plan: Dead Live-WS Deletion Sweep — Stage 1 (zero-wiring modules)

**Status:** APPROVED
**Date:** 2026-07-17
**Approved by:** operator directive 2026-07-17 via coordinator (dead live-WS
deletion sweep, stage 1 = the zero-wiring slice of the recon's PR sequencing;
recon manifests: `recon-dead-ws.md` + `recon-feed-separation.md`, session
scratchpad, main @ 2a97fac)

## Design

Stage 1 deletes ONLY modules with ZERO production callers that need no
main.rs boot-path surgery and no shared-file edits colliding with the
sibling order-side branch (`claude/groww-order-position-push`). Every
deletion was re-verified with `rg` on this branch before removal (the recon
is input, not gospel). Touched crates: **tickvault-app**, **tickvault-core**,
**tickvault-trading** (files under `crates/app/src`, `crates/core/src/pipeline`,
`crates/trading/src/{in_mem,candles}`).

Deleted (7 production modules + 1 dhat test + 1 bench, ~3,400 LoC):

| File | Why dead (verified) |
|---|---|
| `crates/app/src/wal_reinject.rs` | own PR-C2 comments: "retained un-consumed pending the Phase C module cleanup"; zero non-test callers of `reinject_wal_frames` |
| `crates/app/src/bar_cache_loader.rs` | only reference = its `lib.rs` declaration; reads retired shadow tables with a feed-blind union (feed-separation recon GAP-1) |
| `crates/trading/src/in_mem/bar_cache.rs` | only writer was the deleted loader; only other consumers = its own dhat test + bench |
| `crates/trading/src/in_mem/pct_change_cache.rs` | only reference = `in_mem/mod.rs` declaration |
| `crates/trading/src/candles/boundary_calc.rs` | zero callers anywhere (one doc-comment in the also-deleted bar_cache.rs) |
| `crates/core/src/pipeline/first_seen_set.rs` | zero code callers (comments only) |
| `crates/core/src/pipeline/boot_ordering_gate.rs` | zero code callers since PR-C2 (comments + one degenerate main.rs string-scan test, retired truthfully) |
| `crates/trading/tests/dhat_bar_cache_lookup.rs` + `crates/trading/benches/bar_cache_lookup.rs` | die with bar_cache; ci.yml DHAT lane + `bar_cache_lookup` budget key updated in lockstep |

Lockstep retirements: the `ws-reinject-01` CloudWatch filter+alarm
(`error-code-alarms.tf` dated note + `observability-architecture.md`
"Retired paging entries" + aws-budget cost note) — its only emit site died
with `wal_reinject.rs`; a filter with no possible emit site is a dead
filter per the paging drift guard.

Deliberately NOT touched (sibling-collision / later-stage): ErrorCode
variants (`WsReinject01Aborted`, `PrevClose02` retained), `WAL_REINJECT_*`
constants, `crates/common/src/{error_code,config,constants}.rs`,
`crates/common/src/instrument_registry.rs` (DEFERRED — 1,695 LoC + common
lib.rs churn + I-P1-11 guard + core proptest/dhat/bench/budget-key edits =
its own stage), the whole tick pipeline/storage chain (stage 2), the
aggregator (stage 4), and every DO-NOT-TOUCH item in the recon §2 table.

## Edge Cases

- A deleted module re-exported symbols (`BarCache`, `CompactBar`,
  `PctChangeCache`, `bar_cache_clear_before_threshold`): re-verified zero
  external consumers before removing the `in_mem/mod.rs` re-exports.
- ci.yml DHAT drift list AND the trading DHAT step's expected-target count
  (3→2) both updated — either alone fails CI loudly.
- `[[bench]] bar_cache_lookup` removed from `crates/trading/Cargo.toml`
  (strictly unavoidable — Criterion benches are explicitly declared) and
  the `bar_cache_lookup` budget key removed from
  `quality/benchmark-budgets.toml` in the same commit (never orphan a key).
- The observability-architecture paging paragraph is machine-parsed
  (tokens between "Filtered+alarmed codes" and "Everything else"): the
  retired code string was removed from that paragraph and only named in
  the "Retired paging entries" paragraph (outside the parse window).

## Failure Modes

- A missed live caller → build breaks at `cargo check`/clippy (run before
  push); rg sweeps above found only comments.
- The paging drift guard (`error_code_paging_filter_drift_guard.rs`) would
  fail on a dead tf filter → the tf entry was retired in the same PR
  (ws-gap-07 / feed-stall-01 precedent).
- The error-code crossref tests require every variant mentioned in a rule
  file → `ws-reinject-error-codes.md` and `wave-1-error-codes.md` keep
  their variant mentions (banners added, mentions preserved).
- Pre-push test-count ratchet baseline is per-machine/gitignored; deleting
  test files lowers the count — the baseline is corrected to the true tree
  value (standing pre-approval for deletion sweeps).

## Test Plan

- `cargo fmt --check` clean.
- `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf` clean.
- Scoped tests per touched crate: `cargo test -p tickvault-core`,
  `cargo test -p tickvault-trading`, `cargo test -p tickvault-app`
  (crates/common untouched → no workspace escalation required; storage
  untouched).
- Guard updates verified truthful: `phase2_9_l14_hard_fail.rs` becomes a
  tombstone (its surviving assertion pinned a deleted module);
  `ip_monitor_wiring_guard.rs` / `boot_helpers.rs` /
  `feed_lag_monitor.rs` stale doc-comments corrected factually; no
  unrelated assertion weakened.

## Rollback

Single revert of the squash-merge commit restores every deleted file, the
tf filter entry, the ci.yml DHAT lane rows, the bench + budget key, and
the mod declarations — no data migration, no config flip, no runtime
state involved (all deleted code had zero production callers, so rollback
is byte-identical-behavior either way).

## Observability

No live metric, alarm, log line, or Telegram path changes behavior: every
deleted emit site was unreachable (zero callers). The one observability
surface change is the RETIREMENT of the dead `ws-reinject-01` filter+alarm
(−1 alarm ≈ −$0.10/mo, dated notes in `error-code-alarms.tf`,
`observability-architecture.md`, `aws-budget.md`). The load-bearing WAL
floor (`ws_frame_spill.rs`), the 15:40 TICK-CONSERVE-01 audit, the seal
chain, and the `tv_ws_frame_wal_reinjected_dropped_total` residue-archiver
counter in main.rs are all untouched.

## Plan Items

- [x] Delete the 7 zero-wiring modules + dhat test + bench
  - Files: crates/app/src/wal_reinject.rs, crates/app/src/bar_cache_loader.rs, crates/trading/src/in_mem/bar_cache.rs, crates/trading/src/in_mem/pct_change_cache.rs, crates/trading/src/candles/boundary_calc.rs, crates/core/src/pipeline/first_seen_set.rs, crates/core/src/pipeline/boot_ordering_gate.rs, crates/trading/tests/dhat_bar_cache_lookup.rs, crates/trading/benches/bar_cache_lookup.rs
  - Tests: cargo check + scoped crate suites (deletion PR — no new tests)
- [x] Remove mod declarations + re-exports + [[bench]] + budget key + ci.yml DHAT lane rows
  - Files: crates/app/src/lib.rs, crates/trading/src/in_mem/mod.rs, crates/trading/src/candles/mod.rs, crates/core/src/pipeline/mod.rs, crates/trading/Cargo.toml, quality/benchmark-budgets.toml, .github/workflows/ci.yml
  - Tests: cargo fmt/clippy/scoped suites
- [x] Retire the ws-reinject-01 paging filter + docs in lockstep
  - Files: deploy/aws/terraform/error-code-alarms.tf, .claude/rules/project/observability-architecture.md, .claude/rules/project/ws-reinject-error-codes.md, .claude/rules/project/wave-1-error-codes.md, .claude/rules/project/aws-budget.md
  - Tests: error_code_paging_filter_drift_guard + error_code_rule_file_crossref (in `cargo test -p tickvault-common`? — they live in crates/common/tests, exercised via the common suite; run explicitly)
- [x] Truthful guard/comment updates
  - Files: crates/core/tests/phase2_9_l14_hard_fail.rs, crates/app/tests/ip_monitor_wiring_guard.rs, crates/app/src/boot_helpers.rs, crates/core/src/pipeline/feed_lag_monitor.rs, crates/trading/tests/dhat_multi_tf_consume_tick.rs
  - Tests: scoped crate suites
