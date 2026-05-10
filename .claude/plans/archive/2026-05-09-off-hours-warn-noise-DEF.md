# Implementation Plan: Off-hours WARN noise reduction (queue items D + E + F)

**Status:** VERIFIED
**Date:** 2026-05-09 (revised 2026-05-10 per operator feedback)
**Approved by:** Parthiban — `AskUserQuestion` 2026-05-09 23:48 IST chose "D + E + F together"; revised 2026-05-10 00:45 IST chose "Real fix: gate legacy ATM block on !v2" for E + F (operator quote: "we don't need this atm selection right dude now we have changes it as top n volume right dude")
**Branch:** `claude/off-hours-warn-noise-DEF-x7H2k`
**Triggering incident:** operator's live mock-day boot 2026-05-09 22:33–22:35 IST emitted three off-hours `warn!` lines that are not actionable post-close.

## Plan Items

- [x] **D. Gate prev_oi_cache zero-entries WARN on `is_within_trading_session_ist()`**
  - Files: `crates/app/src/main.rs` (line 3528 area)
  - Behaviour: inside `if count == 0 { ... }` branch — emit `warn!` only when in trading session (real signal: candles_1d empty during a session means OI panels read 0%). Outside session emit `info!` (fresh deploy / weekend / pre-open is expected). Counter `tv_prev_oi_cache_empty_total` keeps incrementing in both cases (operator can still query it).
  - Tests: existing `tickvault-app` lib + integration test suite (564 + binaries) stays green.

- [x] **F+. Gate the ENTIRE legacy boot ATM-wait + select_depth_instruments + spawn block on `!config.features.depth_dynamic_pipeline_v2`** (subsumes original E + F WARN gates)
  - Files: `crates/app/src/main.rs` (boot depth setup block at lines 3845-5052)
  - Behaviour: under v2 (the live default since 2026-05-09 per `config/base.toml:305`), depth-20 + depth-200 are owned by `spawn_depth_dynamic_pool` (top-N volume cohort over `movers_1m`, NOT ATM). The legacy block was producing pure noise: 5-min/10-s LTP wait, ATM selection whose result was discarded (the v2 cutover at the spawn branch was already a no-op `else if depth_dynamic_pipeline_v2`), and 3 off-hours WARN lines on every cold boot. Replaced the inner block guard so when v2 is on, a single `info!` notes the skip and the entire wait+selection+spawn loops are bypassed.
  - This kills the source of:
    - `depth ATM: off-market-hours boot — mandatory index LTPs not available` (was main.rs:3937 area, item E)
    - `no valid spot price for depth ATM selection` × N (was depth_strike_selector.rs:231, item F)
  - Out-of-scope (left unchanged):
    - The 09:13 IST anchor task (~main.rs:6125) — still calls `select_depth_instruments` for the historical-anchor visibility Telegram, and `anchor_single_side_enabled` already gates dispatch to false under v2 (line 6158). Separate concern; if operator wants this retired too, follow-up PR.
  - Tests: existing source-scan ratchets in the depth dispatcher guard files still pass; the `select_depth_instruments` + `InitialSubscribe20/200` strings still appear in main.rs (via the untouched 09:13 anchor task).

## Scenarios

| # | When | Before | After |
|---|---|---|---|
| 1 | Saturday off-session boot at 22:33 IST (today's repro), v2 on | 3× `warn!` (`prev_oi_cache zero` + `depth ATM off-market` + 2× `no valid spot price`) | 0 warns — single `info!` "depth boot setup: legacy ATM-based path SKIPPED" + 1 `info!` "prev_oi_cache loaded zero entries (off-hours / weekend boot — expected)" |
| 2 | In-session boot, v2 on, `candles_1d` empty (fresh deploy) | `warn!` × 1 (PR-OI cache empty) + 3× depth WARNs | 1 `warn!` (PR-OI cache empty — real signal) + 0 depth WARNs (v2 owns depth, no ATM block runs) |
| 3 | In-session boot, v2 OFF (operator manually disables flag) | unchanged warns | unchanged (legacy block still runs, WARNs still fire — that's correct for v2-off mode) |
| 4 | Sunday boot at 09:30 IST, v2 on | 3× `warn!` (false alarm — Sunday is non-trading even though 09:30 is "in market hours") | 0 warns (v2 path skips legacy block; D's `is_within_trading_session_ist` correctly suppresses prev_oi WARN on weekend) |
| 5 | 09:13 IST anchor task fires under v2 | runs, calls select_depth_instruments, dispatches to empty `anchor_cmd_senders` (no-op silently because `anchor_single_side_enabled = false`) | unchanged — out of scope, tracked as separate cleanup if operator wants |

## Honest 100% claim qualifier (per `wave-4-shared-preamble.md` §8)

100% inside the tested envelope:
- Under v2 (`depth_dynamic_pipeline_v2 = true`, the live default), the boot legacy ATM block is structurally bypassed — `select_depth_instruments` is never called from boot. Result: 0 boot warns from that path, ratcheted by `cargo test -p tickvault-app --tests` (`no_boot_depth_subscribe_guard.rs::dispatcher_at_0913_calls_select_depth_instruments` still passes because the 09:13 anchor task remains untouched).
- Under v2-off (legacy mode), behaviour unchanged.
- D's prev_oi_cache gate uses `is_within_trading_session_ist()` (the same Wave-Holiday-Gate helper used by PR #542 item B); 5 unit tests in `crates/common/src/market_hours.rs::tests` cover the helper itself.

Beyond the envelope: NSE-specific weekday holidays (Republic Day etc.) are not covered by `is_within_trading_session_ist` (it only folds in weekend); a 10:00 IST holiday boot will still produce the prev_oi_cache `warn!`. Documented gap; threading `TradingCalendar` is a separate concern.

## Verification

- [x] `cargo check -p tickvault-app -p tickvault-core` — green
- [x] `cargo test -p tickvault-app --lib` — 564 passed
- [x] `cargo test -p tickvault-app --tests` — all binaries green
- [x] `bash .claude/hooks/plan-verify.sh`
