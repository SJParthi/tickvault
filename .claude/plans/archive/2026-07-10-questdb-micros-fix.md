# Implementation Plan: QuestDB micros SQL literals — 15:31 cross-verify + tick-conservation windows

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — zero-manual month-verdict directive 2026-07-10; data-integrity fix class

> **Guarantee matrix:** this plan cross-references the mandatory 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` (applies in
> full). Data-integrity fix in `crates/app` (tickvault-app) — cold-path SQL
> builders only, zero hot-path change, zero new tick-drop path, DEDUP keys
> untouched, O(1) unchanged (pure string builders).

## Design

Two VERIFIED pre-existing bugs on main (hand-off:
`main-nanos-verdict.md`, round-2 hostile review of scoreboard PR-A, proven
live on the pinned QuestDB 9.3.5): a bare integer literal compared against a
TIMESTAMP column is interpreted as epoch **MICROSECONDS**; both files embedded
**NANOSECOND** bounds, so their WHERE windows sat ~year 58502 and matched zero
rows.

1. `crates/app/src/cross_verify_1m_boot.rs::our_candles_select_sql` — convert
   the WHERE bounds to micros (`day_start_ist_nanos / 1_000`, end = start +
   86_400 × `MICROS_PER_SEC`); the fn PARAMETER stays nanos (in-memory
   convention, caller `main.rs` unchanged). Lockstep second fix: project
   `(ts / 1) * 1000 AS ts_nanos` so the parsed minute key is genuinely nanos
   and joins `intraday_utc_secs_to_ist_minute_nanos` keys in
   `diff_minute_candles` (WHERE-only would still yield compared=0).
2. `crates/app/src/tick_conservation_boot.rs::build_conservation_ticks_count_sql`
   — micros bounds via new `MICROS_PER_IST_DAY` const; `NANOS_PER_IST_DAY`
   retained for the Rust-side NDJSON counter
   (`count_groww_ndjson_lines_for_ist_day`, nanos-vs-nanos, correct as-is).
3. Shared helper (verdict's preferred shape — PR-A #1473 merged to main
   mid-task): `feed_scoreboard_boot::day_bounds_micros` promoted to
   `pub(crate)` and REUSED by `build_conservation_ticks_count_sql` (same
   `target_ist_day: u64` signature — one micros source, no per-file nanos
   re-derivation can regress). `our_candles_select_sql` keeps a local
   nanos→micros conversion (its input is a nanos instant, not a day number).
4. Rule-11 audit of the empty-set path: NO false-OK exists today —
   `classify_run_status` returns BLIND on `compared == 0` (ratcheted tests),
   the CROSS-VERIFY-1M-02 `error!` fires with `status=BLIND`, and the
   `CrossVerify1mSummary` Telegram PASS arm requires `compared > 0`
   (`events.rs:1665`). No honesty change needed; noted here as verified.

## Plan Items

- [x] Fix `our_candles_select_sql` micros WHERE bounds + `(ts / 1) * 1000`
      nanos-aligned key, with regression-lock doc comment
  - Files: crates/app/src/cross_verify_1m_boot.rs
  - Tests: test_our_candles_select_sql_micros_window_and_nanos_key, our_candles_select_sql_scopes_to_instrument_and_day
- [x] Fix `build_conservation_ticks_count_sql` micros bounds via the shared
      `day_bounds_micros` helper (promoted to pub(crate)), with
      regression-lock doc comment
  - Files: crates/app/src/tick_conservation_boot.rs, crates/app/src/feed_scoreboard_boot.rs
  - Tests: test_build_conservation_ticks_count_sql_feed_filtered
- [x] Verify the compared==0 path reports honestly (BLIND + non-PASS Telegram)
      — confirmed already-honest, no code change
  - Files: crates/app/src/cross_verify_1m_boot.rs
  - Tests: test_classify_run_status_blind_when_zero_compared

## Edge Cases

- `day_start_ist_nanos` not multiple of 1_000: integer division floors —
  callers only ever pass midnight-second × 1e9, so exact.
- `target_ist_day` adversarially huge: `try_from`/`saturating_mul` bound it
  (existing APPROVED comments preserved).
- Micros→nanos key rescale overflow: 2026-era micros ≈ 1.78e15 × 1000 =
  1.78e18 < i64::MAX — safe through year ~2262.
- Groww NDJSON counter must NOT be converted (nanos-vs-nanos in Rust) — doc
  comment on `NANOS_PER_IST_DAY` now bans embedding it in SQL.
- Existing digit-magnitude tests assert the 19-digit nanos literals are ABSENT
  so a partial revert cannot pass.

## Failure Modes

- If QuestDB semantics ever changed to nanos literals, the window would be
  1000× too early → 0 rows again → the EXISTING BLIND/Partial classifications
  fire (fail-loud, never false-OK).
- A WHERE-only fix (missed key rescale) → compared stays 0 → BLIND page — the
  new ratchet test pins BOTH halves so this cannot merge.
- Malformed dataset rows: `parse_our_candles_dataset` skip behaviour unchanged.

## Test Plan

- `cargo test -p tickvault-app` full (lib + integration) — includes the new
  `test_our_candles_select_sql_micros_window_and_nanos_key` digit-magnitude
  ratchet, the updated `our_candles_select_sql_scopes_to_instrument_and_day`,
  and the hardened `test_build_conservation_ticks_count_sql_feed_filtered`
  (16-digit micros literals asserted, 19-digit nanos literals banned).
- `cargo fmt --check` + CI-form `cargo clippy --workspace --no-deps -- -D warnings -W clippy::perf`.
- Hooks: banned-pattern scanner, plan-gate, plan-verify, pub-fn guards,
  test-count ratchet (count increases by +1).

## Rollback

Single-commit revert (`git revert`) restores the prior nanos builders; no
schema, config, or data migration involved — the tables and callers are
untouched. The regression-lock tests would fail on revert (intended).

## Observability

- No new metrics/codes needed: the fixed queries feed EXISTING observability —
  CROSS-VERIFY-1M-01 mismatch detection becomes able to fire for the first
  time; `tick_conservation_audit.db_rows` becomes truthful; Groww
  `classify_groww_outcome` can now reach Balanced.
- Regression locks: doc-comment + digit-magnitude unit tests in both files
  (house style of the scoreboard branch's micros ratchets).
- Post-merge live proof: `mcp__tickvault-logs__questdb_sql` count with micros
  bounds returns >0 on a trading day (already proven live in the verdict file).
