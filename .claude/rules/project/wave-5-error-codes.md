---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/core/src/instrument/depth_dynamic_top_volume_selector.rs"
  - "crates/app/src/depth_dynamic_pipeline_v2.rs"
  - "crates/core/src/instrument/subscription_planner.rs"
  - "crates/app/src/main.rs"
  - "crates/core/src/pipeline/volume_monotonicity_guard.rs"
  - "crates/core/src/parser/quote.rs"
  - "crates/app/src/boot_smoke_test.rs"
  - "crates/core/src/instrument/phase2_readiness_check.rs"
  - "crates/core/src/notification/events.rs"
---

# Wave 5 Error Codes

> **Authority:** This file is the runbook target for the four ErrorCode
> variants added in the Wave 5 hardening implementation
> (`.claude/plans/active-plan-wave-5-indices-only.md` Item 9). Cross-ref
> test `crates/common/tests/error_code_rule_file_crossref.rs` requires
> every variant in `crates/common/src/error_code.rs::ErrorCode` to be
> mentioned in at least one rule file under `.claude/rules/`.

> **[ARCHIVED 2026-07-20]** DEPTH-20-DYN-03 + DEPTH-200-DYN-01 (RETIRED 2026-07-06 — movers runtime deleted 2026-05-19; variants deleted; historical audit) — moved verbatim to `docs/rules-archive/wave-5-error-codes-archive.md` (context-size incident; content unchanged).
## PREVCLOSE-03 — boot-time prev-close routing assertion failed (Item 13)

**Trigger:** at boot, the subscription plan is scanned against the per-segment
prev-close routing matrix from `live-market-feed.md`:

| Segment | Required feed mode | Where prev_close lives |
|---|---|---|
| IDX_I | Ticker | standalone code 6 packet |
| NSE_EQ | Quote OR Full | bytes 38-41 (Quote) / 50-53 (Full) of the packet |
| NSE_FNO — **STALE pre-§36 row, superseded 2026-07-08** (hostile-review r3: edited inline so no future runtime-assertion implementer honors it; the ONLY live NSE_FNO subscriptions are the §36 FUTIDX Quote rows below — all monthly serials since 2026-07-10) | ~~Full~~ | historical: bytes 50-53 of the Full packet (the Full requirement served the deleted OI/depth consumers; ratchet retired `#[cfg(any())]`, PR #7b) |
| BSE_FNO — **STALE pre-§36 row, superseded 2026-07-08** (same carve-out; the ONLY live BSE_FNO subscriptions are the §36 SENSEX futures, Quote rows below — all monthly serials since 2026-07-10) | ~~Full~~ | historical: bytes 50-53 of the Full packet |
| NSE_FNO/BSE_FNO §36 FUTIDX (Quote) — 2026-07-08; all monthly serials since 2026-07-10 (§36.7) — **the AUTHORITATIVE row for these segments** | Quote | bytes 38-41 (Ticket #5525125, `dhan_locked_facts.rs`; OI via separate code-5 packet, currently uncaptured) |

If any subscribed instrument has a `(segment, feed_mode)` pair outside the
allowed cell of this matrix, PREVCLOSE-03 fires Severity::Critical and the
boot HALTS — refusing to start a pipeline that loses prev_close for half
the universe is cheaper than recovering from a corrupted day's snapshot.

**Triage:**

1. The Telegram event names the offending instrument (`security_id`,
   `display_label`) + actual feed_mode + expected feed_mode for its segment.
2. Most common cause: operator set `[subscription] feed_mode = "Quote"` in
   `config/base.toml`, which silently downgrades NSE_FNO + BSE_FNO from
   Full and loses the bytes-50-53 prev_close. Set back to `"Full"`.
3. Less common: a code-side regression where `make_derivative_instrument`
   stamps the wrong mode. The 3 ratchet tests in
   `subscription_planner::tests::test_idx_i_subscriptions_use_ticker_mode`,
   `test_nse_eq_subscriptions_use_quote_or_full`, and
   `test_nse_fno_bse_fno_subscriptions_use_full_mode` fail the build first
   in this case.

**Auto-triage safe:** NO (Severity::Critical halts boot — operator action
required).

**Source (Item 13):** the 3 unit-level ratchet tests in
`crates/core/src/instrument/subscription_planner.rs::tests`. The runtime
boot emission in `crates/app/src/main.rs` is a follow-up (the tests pin
the contract today; runtime emission is purely defence-in-depth for the
case where someone bypasses the unit tests).

## VOLUME-MONO-01 — cumulative volume monotonicity breach (Item 26 L1)

> **⚠ EMIT SITE DELETED 2026-07-17 (stage-2 dead-WS sweep):**
> `crates/core/src/pipeline/volume_monotonicity_guard.rs` was DELETED with
> the dead Dhan tick chain (its only feeder, `tick_processor.rs`, had zero
> production callers after the live-WS retirements). The
> `Volume01MonotonicityBreach` variant is RETAINED (no ErrorCode deletions
> in the sweep); the code can no longer fire. Historical audit below.

> **[ARCHIVED 2026-07-20]** VOLUME-MONO-01 body (emit site deleted 2026-07-17; variant retained; historical audit) — moved verbatim to `docs/rules-archive/wave-5-error-codes-archive.md` (context-size incident; content unchanged).
> **[ARCHIVED 2026-07-20]** DEPTH200-SMOKE-01 + PHASE2-READY-01 (subsystems deleted — depth-200 feeds 2026-05-19, Phase 2 dispatcher 2026-05-19; ErrorCode variants no longer exist; historical audit) — moved verbatim to `docs/rules-archive/wave-5-error-codes-archive.md` (context-size incident; content unchanged).
## Cross-references

- `.claude/plans/active-plan-wave-5-indices-only.md` Items 4, 5, 6, 9
- `.claude/rules/project/wave-4-shared-preamble.md` (charter)
- `.claude/rules/project/per-wave-guarantee-matrix.md` (mandatory matrix)
- `crates/common/src/error_code.rs::ErrorCode` (`CorePin01PinningFailedAtBoot`,
  `CorePin02WorkerDrifted`, `Depth20Dyn03TopGainersEmpty`,
  `Depth200Dyn01TopGainersEmpty`, `PrevClose03BootRoutingAssertion` variants)
