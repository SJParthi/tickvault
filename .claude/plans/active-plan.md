# Implementation Plan: NTM Sub-PR #5 — union NTM constituents into the subscription + role flags

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06: chose the **two-flags faithful** role
model ("is_fno_underlying + is_index_constituent, both set if both") + standing "if merged, go
ahead" after #1038.
**Crate(s) touched:** `core` (`daily_universe.rs`, `instrument_snapshot.rs`),
`app` (`daily_universe_boot.rs`, `prev_day_ohlcv_boot.rs`, `today_instrument.rs`,
`lifecycle_reconcile_orchestrator.rs` — match/literal sites). Feature path is the existing
`daily_universe_fetcher` build.

## Context

§31 item 3–5 + §31.1: the live subscription becomes the **NTM union** (indices + F&O
underlyings + ~750 NTM constituent stocks), all Quote mode on the existing single main-feed WS
(2-WS lock unchanged). Each subscribed stock carries a **role tag** so "F&O only" is an **O(1)
filter**, not a second download/WebSocket (§31 item 5). A stock can be **both** an F&O underlying
**and** an NTM constituent — §31.1(6) says `fno_underlying` **and/or** `index_constituent` — so
the model is two independent flags, lossless.

This PR wires the **assembly** (`build_daily_universe`) + the **warm-snapshot round-trip**. The
real constituent data flows at boot in **Sub-PR #10** (orchestrator); until then the new
`ntm_constituents` arg is passed **empty**, so live boot is byte-for-byte unchanged. The
SID→CsvRow bridge from the resolver output is Sub-PR #10's job — this PR does NOT touch the
merged `constituent_resolver.rs`.

## Design

- **`InstrumentRole`** gains `IndexConstituent` (cash-only NTM stock, NSE_EQ EQUITY) +
  `as_str()` label `"index_constituent"`. Primary classes stay mutually exclusive:
  `Index` (IDX_I value) · `FnoUnderlying` (equity that is an F&O underlying, with or without NTM
  membership) · `IndexConstituent` (equity that is ONLY an NTM constituent).
- **`SubscriptionTarget`** gains two `bool` flags — `is_fno_underlying`, `is_index_constituent` —
  the lossless membership set. Invariants (asserted by tests): `Index` ⇒ both false;
  `FnoUnderlying` ⇒ `is_fno_underlying == true`; `IndexConstituent` ⇒
  `is_fno_underlying == false && is_index_constituent == true`.
- **`build_daily_universe(indices, fno, fno_contracts, ntm_constituents: Vec<CsvRow>)`** — new
  4th arg. Pass order unchanged (indices → F&O underlyings), then **Pass 4 — NTM fold**:
  build `HashMap<(security_id, NseEquity), idx>` over the equity targets already pushed; for each
  constituent row, if its `(sid, NSE_EQ)` matches an existing `FnoUnderlying` target → set that
  target's `is_index_constituent = true` (the **both** case, no new row); else push a new
  `IndexConstituent` target (`is_fno_underlying=false, is_index_constituent=true`). Intra-batch
  dedup by `(sid, seg)` so a repeated constituent row doesn't double-push (I-P1-11).
- **O(1) extract helpers** on `DailyUniverse`: `fno_underlying_count()` /
  `index_constituent_count()` (filter on the flags) — the operator's "separately extractable"
  guarantee, used by the boot observability gauges.
- **Snapshot** (`instrument_snapshot.rs`): `SnapshotTarget` gains `is_fno_underlying` +
  `is_index_constituent` (serde `#[serde(default)]` ⇒ old same-day snapshots deserialize with
  `false`); `from_universe`/`to_universe` carry the flags; `parse_role` learns
  `"index_constituent"`; fail-closed-on-unknown-role preserved.

## Edge Cases

- Constituent SID == an F&O underlying SID (same NSE_EQ) → flag the existing target, **no dup**.
- Constituent SID collides with an `Index` SID across segments (I-P1-11: sid alone not unique) →
  keyed on `(sid, NSE_EQ)`, so the IDX_I index is never touched.
- Duplicate constituent rows in the input Vec → deduped by `(sid, seg)`; first wins.
- `ntm_constituents` empty (today, pre-#10) → universe identical to current; size still bounded.
- Universe size with full NTM (~750+) → within `MAX_DAILY_UNIVERSE_SIZE = 1200` (Sub-PR #2).
- Constituent row whose `security_id` doesn't parse / wrong segment → it's a pre-resolved NSE_EQ
  row by contract; build still keys defensively on the string `(security_id, segment)` and
  won't panic.

## Failure Modes

- Universe out of `[100,1200]` after the fold → existing `BuildError::UniverseSizeOutOfBounds`
  (boot HALTS, fail-closed) — unchanged path, now also covers the NTM expansion.
- Old snapshot without flags → `#[serde(default)]` false; role still drives subscription, so the
  warm plan is correct (F&O-only filter falls back to `role == FnoUnderlying` for old snapshots).
  Documented; next cold build re-stamps flags.
- Unknown role label in a snapshot → `to_universe` returns `None` (fail-closed) — unchanged.

## Test Plan

`cargo test -p tickvault-core` + workspace **test-compile** (the #1037 lesson — `cargo check`
skips test code; literal/match sites live in test code too).

- `daily_universe.rs`: fold-dedup (both-case sets both flags, no dup row); pure-constituent adds
  `IndexConstituent`; `Index` ⇒ both flags false; `fno_underlying_count`/`index_constituent_count`
  correct incl. the both-case; empty `ntm_constituents` ⇒ unchanged; envelope still bounds; role
  invariants hold; `as_str`/`parse` round-trip for the new variant.
- `instrument_snapshot.rs`: flags survive `from_universe`→`to_universe`; old snapshot (no flag
  fields) deserializes with `false`; unknown-role still fails closed; new `"index_constituent"`
  label round-trips.
- Updated existing literal/match sites compile + pass (today_instrument, lifecycle_reconcile,
  subscription_planner, prev_day_ohlcv_boot, daily_universe_boot).

## Rollback

Pure additive on the assembly + snapshot model; live boot passes `ntm_constituents = vec![]`
until Sub-PR #10, so reverting #5 (or passing empty forever) restores exact current behavior.
`git revert` is clean — no migration, no persisted-schema dependency in this PR.

## Observability

`daily_universe_boot.rs` adds boot gauge `tv_universe_size{kind="index_constituent"}` and keeps
`kind="index"`/`kind="fno_underlying"` (now flag-derived counts). No new error code (no new
failure class — size violations reuse `INSTR-FETCH-04`). Telegram/audit additions land with the
real data flow in Sub-PR #10.

## Plan Items

- [x] Item 1 — data model: `InstrumentRole::IndexConstituent` + `as_str`; `SubscriptionTarget`
  two flags; role invariants
  - Files: `crates/core/src/instrument/daily_universe.rs`
  - Tests: `instrument_role_as_str_is_stable_wire_format`
- [x] Item 2 — `build_daily_universe` 4th arg + Pass-4 NTM fold (dedup, both-case flag)
  - Files: `crates/core/src/instrument/daily_universe.rs`
  - Tests: `ntm_fold_both_case_sets_both_flags_no_dup`,
    `ntm_fold_pure_constituent_adds_role`, `ntm_fold_empty_is_unchanged`,
    `ntm_fold_dedups_repeat_rows`, `ntm_fold_does_not_touch_index_values_across_segments`,
    `ntm_fold_drops_non_nse_eq_constituent_row_fail_closed`,
    `fno_underlying_count_includes_both_case`, `index_constituent_count_includes_both_case`
- [x] Item 3 — snapshot flags round-trip + `parse_role`
  - Files: `crates/core/src/instrument/instrument_snapshot.rs`
  - Tests: `test_snapshot_flags_round_trip`, `test_snapshot_old_format_defaults_flags_false`,
    `test_parse_role_inverse_of_as_str`
- [x] Item 4 — fix all match/literal sites + boot gauge
  - Files: `crates/app/src/daily_universe_boot.rs`, `crates/app/src/prev_day_ohlcv_boot.rs`,
    `crates/app/src/today_instrument.rs`, `crates/app/src/lifecycle_reconcile_orchestrator.rs`,
    `crates/core/src/instrument/subscription_planner.rs`
  - Tests: existing suites recompile + pass

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | NTM stock that is also an F&O underlying | one target, role=FnoUnderlying, BOTH flags true |
| 2 | NTM stock not in F&O | new target, role=IndexConstituent, only is_index_constituent |
| 3 | empty ntm_constituents (today) | universe == current; live boot unchanged |
| 4 | full NTM (~750) | size ≤ 1200; "F&O only" = is_fno_underlying (O(1)) |
| 5 | old warm snapshot (no flags) | deserializes flags=false; role drives subscription |
