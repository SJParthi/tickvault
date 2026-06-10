# Coverage-gaps plan — operator decisions + next-session handoff (2026-06-10)

> Continuation pointer for `.claude/plans/research/coverage-gaps.md` (merged #1076).
> Session 2026-06-10 delivered 5 merged PRs: #1076 (report), #1079 (P0 gate
> fail-closed + real floors), #1081 (P2a, 12 tests), #1082 (P2b, 6 tests),
> #1085 (P2c, 9 tests). 27 new tests, zero production lines changed.

## Operator decisions (2026-06-10, via AskUserQuestion — dated authority)

| Item | Decision | Meaning for the next PR |
|---|---|---|
| **P4 — scope-locked dead planner code (~456 lines)** | **DELETE IT** | Remove the unreachable `should_subscribe_index_derivatives` / `should_subscribe_stock_derivatives` branches in `crates/core/src/instrument/subscription_planner.rs` in a single reviewed PR. The 2-WS scope lock (`websocket-connection-scope-lock.md`) already forbids re-enabling; cite THIS dated decision in the PR per the lock's §"Operator re-approval protocol". Keep the `const fn → false` guards + their ratchet tests. |
| **P1 — app crate at 63%** | **DESIGN PROPOSAL FIRST** | Write a design DOC (no code): extract boot steps into testable functions + CLI exit-code tests (`--check-trading-day`, `tv_doctor --format json`). Operator reviews before ANY boot-code refactor. |

## Honest P2d finding (verified this session)

The report's small-file tail (parser files, watchdog edges) is dominated by
`panic!("wrong variant")` arms INSIDE existing tests — never-taken
test-failure branches that llvm-cov counts but that can never execute on a
green run. NOT actionable production gaps. Real remaining uncovered
production code = Cat A (boot binary), Cat B (live-network loops — needs the
P3 mock-server harness), and the deep `tick_processor` loop arms
(needs frame-level tests via the existing `make_ticker_frame` harness —
sizeable, do in a fresh session).

## Next-session queue (in order)

1. **P4 PR** — delete the dead planner branches (operator-approved above).
2. **P1 design doc** — boot-step extraction proposal (docs-only PR).
3. **P3** — local mock-WS/REST harness tests for `api_client` /
   `option_chain/client` / `order_update_connection` / `ip_verifier`
   (axum + tokio-tungstenite are already pinned workspace deps — no new deps).
4. **tick_processor loop arms** — frame-level tests via the module's existing
   harness.
5. **Re-measure** coverage (CI artifact or local `cargo llvm-cov` at the then-current
   SHA with `CARGO_PROFILE_DEV_DEBUG=0` — full-debuginfo builds blow the ~29 GB
   disk quota) and **ratchet the floors UP** in
   `quality/crate-coverage-thresholds.toml` per its ratchet rule.

## Operational notes for the next session

- Artifact downloads from GitHub are network-blocked in remote sessions
  (Azure blob host not allowlisted) — reproduce locally at the same SHA instead.
- App-token merges do NOT trigger main-push CI; the post-merge Coverage & Perf
  job runs only on operator-triggered pushes.
- PRs go "behind" fast on this repo — fix is local `git merge origin/main` +
  real push (server-side update-branch does NOT retrigger CI).
- The pre-commit gate's invariant tests have a 60s timeout — warm the build
  cache (`cargo test -p tickvault-storage --test dedup_uniqueness_proptest`)
  after any `cargo clean`.
