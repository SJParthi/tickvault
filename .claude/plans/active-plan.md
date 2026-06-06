# Implementation Plan: NTM Sub-PR #10b — boot turn-on of real NTM data

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06 (niftyindices failure = **Degrade + alert**) +
standing "once merged, go ahead with the plan always".
**Crate(s) touched:** `core` (`instr_fetch_runner.rs` — thread `ntm_map`), `app`
(`daily_universe_boot.rs` — thread `ntm_map`; `main.rs` — best-effort niftyindices fetch +
`fetch_ntm_constituency_map` helper). Feature: `daily_universe_fetcher`.

## Context

#10a (merged) made the orchestrator NTM-capable (`build_universe_from_bytes(bytes, ntm_map)` +
`resolve_ntm_rows` bridge + `NTM-CONSTITUENCY-01`), with all callers passing `None`. #10b is the
**turn-on**: fetch the real niftyindices constituents at boot and thread them down so the ~750-stock
NTM union actually flows on the single main-feed WS.

## Design

- **`main.rs::fetch_ntm_constituency_map(today)`** (new private async helper) — builds
  `ConstituencyDownloader` + `build_constituency_map(&dl, NTM_CONSTITUENCY_SLUGS, today)` wrapped in
  `tokio::time::timeout(NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS = 30s)`. **CRITICAL fix (hostile review):**
  the SUBSCRIPTION uses the **NTM slug ONLY** (§31 item 4) — NOT the full ~49-index
  `INDEX_CONSTITUENCY_SLUGS`, whose deduped union ≈ the entire NSE (~1,900 stocks) would breach
  `MAX_DAILY_UNIVERSE_SIZE` and HALT boot on a *healthy* fetch. **Degrade+alert**: on downloader-init
  failure OR timeout OR an empty map it logs `error!(code = NTM-CONSTITUENCY-01)` (reason =
  downloader_init / timeout / fetch_or_parse) and returns `None`; NEVER blocks boot.
- **Threading:** `cold_build_daily_universe` → `run_daily_universe_boot(.., ntm_map)` (app) →
  `run_daily_universe_fetch_runner(wrapped, max_attempts, ntm_map.as_ref())` (core) →
  `build_universe_from_bytes(bytes, ntm_map)`. `Option<&IndexConstituencyMap>` is `Copy`, so the
  runner's retry closure re-uses it across attempts with no clone.
- The Dhan CSV path stays **fail-closed §4** (infinite retry); ONLY the niftyindices NTM layer
  degrades. Subscription set = union of tracked-index constituents (NTM dominates), bounded by the
  `[100, 1200]` envelope (`INSTR-FETCH-04` halts if exceeded — a data anomaly, not a degrade).

## Edge Cases

- niftyindices fully down / DNS / 5xx → empty map → `None` + Critical alert → core universe.
- Some slugs fail, NTM ok → map non-empty → resolve over what's present (best-effort).
- `>0.5%` constituents dangling vs Dhan rows → `resolve_ntm_rows` degrades to empty (#10a) + alert.
- All constituents are F&O underlyings → both-flags set, no dup (#5 fold).
- Union > 1200 → `INSTR-FETCH-04` fail-closed HALT.
- Weekend/holiday → existing daily orchestrator gate; fetch best-effort.

## Failure Modes

- NTM source down → DEGRADE + `NTM-CONSTITUENCY-01` Critical (operator-chosen); core trades.
- Dhan CSV down → §4 infinite-retry fail-closed (UNCHANGED — NTM policy does not relax this).
- niftyindices slow → bounded by the downloader's connect/read timeouts (#3 §18 hardening); cold path.

## Test Plan

`cargo test -p tickvault-core --features daily_universe_fetcher` + `-p tickvault-app` (scoped; full
workspace = CI). Threading is param-only — covered by recompiling the existing runner (8) + boot (11)
suites with the new arg (all callers updated). The resolve+bridge + degrade behaviour is already
unit-tested in #10a (`resolve_ntm_rows_*`, `build_universe_with_ntm_map_threads_through`).
`fetch_ntm_constituency_map` is a thin best-effort wrapper over already-tested `build_constituency_map`
(network I/O — TEST-EXEMPT by integration boundary; degrade branches are straight-line + logged).

## Rollback

Degrade-by-design: a niftyindices outage OR `git revert` both fall back to the proven core universe
(the new fetch returns `None` ⇒ identical to pre-#10b boot). No migration, no schema change.

## Observability

`NTM-CONSTITUENCY-01` Critical → Telegram via the 5-sink error pipeline on degrade; boot `info!`
logs `indices` + `unique_stocks` on success and the per-phase build log (#5) shows
`index_constituent_count` non-zero. `tv_universe_size{kind="index_constituent"}` (gauge, #5) becomes
non-zero on a healthy NTM boot.

## Plan Items

- [x] Item 1 — `run_daily_universe_fetch_runner` gains `ntm_map: Option<&IndexConstituencyMap>`;
  retry closure threads it into `build_universe_from_bytes`
  - Files: `crates/core/src/instrument/instr_fetch_runner.rs`
  - Tests: `runner_wires_build_universe_from_bytes`, `runner_wires_tokio_time_sleep`
- [x] Item 2 — `run_daily_universe_boot` gains `ntm_map: Option<IndexConstituencyMap>`; passes
  `.as_ref()` to the runner
  - Files: `crates/app/src/daily_universe_boot.rs`
  - Tests: `test_run_daily_universe_boot_no_universe_bails`,
    `test_run_daily_universe_boot_valid_csv_reaches_reconcile_then_fails_closed`
- [x] Item 3 — `main.rs::fetch_ntm_constituency_map` best-effort fetch + degrade alert; wired into
  `cold_build_daily_universe`
  - Files: `crates/app/src/main.rs`
  - Tests: `test_run_daily_universe_boot_valid_csv_reaches_reconcile_then_fails_closed` (boot path recompiles + green; helper is a network-boundary best-effort wrapper)
- [x] Item 4 — adversarial review on the #10b diff (hot-path CLEAN; hostile found **1 CRITICAL + 1
  MEDIUM**, BOTH fixed inline): CRITICAL = subscribe NTM-slug-only (was full ~49-index list →
  envelope breach → boot HALT on healthy fetch); MEDIUM = added `NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS`
  total-fetch timeout so a stalled niftyindices degrades instead of delaying boot. New constants
  `NTM_CONSTITUENCY_SLUGS` + `NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS`.
  - Files: `crates/common/src/constants.rs`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | niftyindices healthy | universe = core + ~750 NTM; both-flags on overlap; `index_constituent_count` > 0 |
| 2 | niftyindices down | core universe + `NTM-CONSTITUENCY-01` Critical; trading continues |
| 3 | union > 1200 | `INSTR-FETCH-04` fail-closed HALT |
| 4 | revert / source outage | exact pre-#10b core universe |
