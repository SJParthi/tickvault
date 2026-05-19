# Implementation Plan: PR #7 — Tighten SubscriptionScope to `Indices4Only`

**Status:** IN_PROGRESS (PR #7a = Slices 1+2 ready for review; PR #7b will follow with Slices 3-8)
**Date:** 2026-05-19
**Approved by:** Parthiban
**Branch:** `claude/aws-lifecycle-pr-7-indices4only-scope` (PR #7a)
**Predecessor:** PR #6b (#710 merged) — `LOCKED_UNIVERSE` static = 4 IDX_I SIDs
**Successor in 14-PR sequence:** PR #8 — option_chain module (heart-piece)

## Sub-PR split (operator-charter §H serial completion)

- **PR #7a** (this PR) — Slices 1 + 2: introduce `Indices4Only` LOCKED variant, switch default, wire planner + watchdog arms, 3 new ratchet tests, 11 legacy-coverage tests preserved via `legacy_full_universe_config()` helper. Workspace 7,211/7,211 green.
- **PR #7b** (follow-up after merge) — Slices 3 + 4: delete dead `subscribe_*_derivatives` / `subscribe_display_indices` flags; collapse `effective_main_feed_pool_size` signature.
- **PR #7c** — Slices 5 + 6: retire legacy SubscriptionScope variants entirely; add source-scan ratchet meta-guard.
- **PR #7d** — Slices 7 + 8: update CLAUDE.md + rule files; bump test-count baseline.

---

## Why this PR

PR #6b cemented the universe to 4 IDX_I SIDs via a static `FnoUniverse::locked_4_idx_i`.
But `SubscriptionScope` still carries 3 variants (`FullUniverse`,
`IndicesOnlyAllExpiries`, `IndicesUnderlyingsOnly`) — dead branches now that
movers/greeks/depth/Phase 2/universe-builder are all retired. The planner has
~5K LoC of branching against these dead variants + dead flags
(`subscribe_stock_derivatives`, `subscribe_index_derivatives`,
`subscribe_display_indices`).

Goal: collapse to a single LOCKED variant `Indices4Only`; delete dead branches;
delete sectoral/INDIA VIX display-index code paths; tighten the planner so it
can only emit 4 Ticker subscriptions (NIFTY/BANKNIFTY/SENSEX/INDIA VIX).

INDIA VIX stays IN the 4 (per operator-charter §I + websocket-connection-scope-lock).
The 25 sectoral / broad-market display indices are dropped entirely.

---

## What this PR FORBIDS forever (operator lock 2026-05-15 §I)

- `subscribe_stock_derivatives = true` → compile error (flag deleted)
- `subscribe_index_derivatives = true` → compile error (flag deleted)
- `subscribe_display_indices = true` for anything OTHER than the locked 4 → impossible (no such field; only locked set exists)
- Any sectoral index in the plan → impossible (filter is hardcoded to the 4 SIDs)
- BSE_EQ / NSE_FNO / BSE_FNO / MCX subscriptions → impossible (universe is 4 IDX_I only)

Mechanical ratchets enforce all of the above.

---

## Plan items (sliced into commits, like PR #6b)

### Slice 1 — Introduce `Indices4Only` variant + deprecate legacy
- [ ] Add `SubscriptionScope::Indices4Only` variant (the only LOCKED scope).
  - Files: `crates/common/src/config.rs`
  - Tests: `test_subscription_scope_default_is_indices4only`, `test_indices4only_serde_roundtrip`
- [ ] Make `Indices4Only` the `Default` (was `IndicesOnlyAllExpiries`).
- [ ] Keep legacy variants but mark `#[deprecated(...)]` so call sites flag.
- [ ] Update `config/base.toml` `[subscription] scope = "indices_4_only"`.

### Slice 2 — Planner branch collapse
- [ ] In `crates/core/src/instrument/subscription_planner.rs`:
  - Replace every `SubscriptionScope::IndicesUnderlyingsOnly` match arm with `SubscriptionScope::Indices4Only`.
  - Delete `SubscriptionScope::IndicesOnlyAllExpiries` and `SubscriptionScope::FullUniverse` arms (now unreachable).
  - `should_subscribe_stock_derivatives` → always returns `false` (then delete fn entirely once all callers gone).
  - `should_subscribe_index_derivatives` → always returns `false` then delete.
  - Sectoral/INDIA VIX display-index loop body collapses to a 4-SID hardcoded emission.
- [ ] Tests: `test_planner_emits_exactly_4_idx_i_sids_under_indices4only`,
  `test_planner_emits_zero_derivatives_under_indices4only`,
  `test_planner_emits_zero_nse_eq_under_indices4only`.

### Slice 3 — Delete dead `SubscriptionConfig` flags
- [ ] In `crates/common/src/config.rs`:
  - Delete `subscribe_stock_derivatives: bool`.
  - Delete `subscribe_index_derivatives: bool`.
  - Delete `subscribe_display_indices: bool`.
- [ ] Update every test that constructs `SubscriptionConfig` (drop the deleted fields).
- [ ] Tests: `test_subscription_config_has_no_derivatives_flags`.

### Slice 4 — Delete `effective_main_feed_pool_size` legacy branch + collapse to const-1
- [ ] `effective_main_feed_pool_size(Indices4Only, _) → 1` (4 SIDs fit on 1 conn).
- [ ] Delete the `configured` parameter once all call sites converge.
- [ ] Tests: `test_effective_main_feed_pool_size_is_always_one_under_indices4only`.

### Slice 5 — Retire legacy `SubscriptionScope` variants entirely
- [ ] Delete `SubscriptionScope::FullUniverse`.
- [ ] Delete `SubscriptionScope::IndicesOnlyAllExpiries`.
- [ ] Delete `SubscriptionScope::IndicesUnderlyingsOnly`.
- [ ] Keep ONLY `Indices4Only`. Single-variant enum (intentional — prevents accidental introduction of a new scope without going through `websocket-connection-scope-lock.md`).
- [ ] Update banned-pattern scanner + add ratchet meta-guard.
- [ ] Tests: `test_subscription_scope_has_exactly_one_variant`.

### Slice 6 — Ratchet: pin the lock mechanically
- [ ] Add `crates/core/tests/indices4only_scope_lock_guard.rs` — source-scan ratchet that:
  - Verifies no `FullUniverse` / `IndicesOnlyAllExpiries` / `IndicesUnderlyingsOnly` string appears in `crates/` (except in archived docs and this plan file).
  - Verifies `subscribe_stock_derivatives` / `subscribe_index_derivatives` / `subscribe_display_indices` are absent from `crates/`.
  - Verifies the planner emits EXACTLY 4 subscriptions for the only legal scope.

### Slice 7 — Update CLAUDE.md + rule files
- [ ] Update `.claude/rules/project/live-market-feed-subscription.md` — replace the FullUniverse / IndicesOnlyAllExpiries narrative with the Indices4Only LOCKED narrative.
- [ ] Update `.claude/rules/project/websocket-connection-scope-lock.md` reconnect-parity table if anything drifted.
- [ ] Update `.claude/rules/project/operator-charter-forever.md` §I (no semantic change, just rename references).
- [ ] Update `CLAUDE.md` "BOOT SEQUENCE" + "CONFIGURATION" sections.

### Slice 8 — Test-count baseline ratchet
- [ ] `.claude/hooks/.test-count-baseline` updated (test count moves up from new ratchets, down from deleted test sites).

---

## Z+ 15-row "100% everything" matrix

| Demand | Proof |
|---|---|
| Code coverage | Per-slice unit tests + integration test for planner |
| Audit coverage | N/A — no new typed events; planner is pure |
| Testing coverage | unit + property (planner output shape) + source-scan ratchet |
| Code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify |
| Performance | Planner moves from O(N) over universe to O(1) const emission |
| Monitoring | `tv_main_feed_pool_size{scope="indices_4_only"}` gauge updated |
| Logging | Boot log notes "Indices4Only scope locked — 4 SIDs" |
| Alerting | Existing alerts unchanged |
| Security | No new attack surface |
| Security hardening | Compile-time prevention of expanded scope |
| Bug fixing | 3-agent adversarial review on Slice 2 + Slice 5 diffs |
| Scenarios | 4 SIDs subscribed; 0 derivatives; 0 sectorals; 0 NSE_EQ |
| Functionalities | Every pub fn has call site + test (gates 6+11) |
| Code review | 3 agents on Slice 2 + Slice 5 |
| Extreme check | Slice 6 source-scan ratchet fails build on regression |

## Z+ 7-row Resilience matrix

| Demand | Envelope |
|---|---|
| Zero ticks lost | Unchanged — 5M-tick rescue ring still in place |
| WS never disconnects | Unchanged — same 1 main-feed + 1 order-update |
| Never slow/locked | Planner is now O(1) emit (faster than before) |
| QuestDB never fails | Unchanged |
| O(1) latency | Maintained — universe is a static const |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` unchanged |
| Real-time proof | 7-layer telemetry unchanged |

## Honest 100% claim

> "100% inside the LOCKED envelope: SubscriptionScope is now a single-variant
> enum `Indices4Only`. The planner emits EXACTLY 4 Ticker subscriptions
> (NIFTY/BANKNIFTY/SENSEX/INDIA VIX). Stock F&O / index F&O / sectoral / BSE /
> NSE_EQ subscriptions are compile-time impossible. Ratcheted by source-scan
> guard `indices4only_scope_lock_guard.rs`."

## Auto-driver explanation

> "Sir, before today the shop had 3 menus (FullUniverse / IndicesOnly /
> Underlyings). Today we throw away 2 menus + lock the remaining one to 4
> drinks only: NIFTY, BANKNIFTY, SENSEX, INDIA VIX. No more 'oh I'll add
> sectoral indices later'. No more 'oh I'll enable stock options'. The
> compiler itself refuses. The 4 drinks are the only drinks. Forever."

---

## Scenarios covered

| # | Scenario | Expected |
|---|---|---|
| 1 | Operator sets `scope = "full_universe"` in TOML | TOML parse fails — variant gone |
| 2 | Operator sets `subscribe_stock_derivatives = true` | TOML parse fails — field gone |
| 3 | Fresh boot with default config | Planner emits 4 IDX_I Ticker subs |
| 4 | Future Claude session tries to add new scope variant | Slice 6 ratchet blocks PR |
| 5 | Future code path tries to subscribe NSE_EQ | Planner has no path to emit it |

---

## Estimated commits / slices: 8
## Estimated LoC delta: ~500 lines deleted, ~150 added, net -350

## Rollout checklist (per slice)

- [ ] `cargo check -p <crate>` green
- [ ] `cargo test -p <crate>` green
- [ ] Banned-pattern scanner clean
- [ ] Pub-fn-test guard clean
- [ ] Commit with conventional message
- [ ] After all slices: open draft PR, enable auto-merge, subscribe to PR activity
