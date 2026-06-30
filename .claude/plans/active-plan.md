# Implementation Plan: Persist Dhan 429 WS cooldown so it survives a process restart

**Status:** VERIFIED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — go-ahead given this session 2026-06-30

> Guarantee matrices: this item carries the 15-row + 7-row matrices by
> cross-reference to `.claude/rules/project/per-wave-guarantee-matrix.md`
> (mandatory per `per-item-guarantee-check.sh`). Evidence below.

## Design

**Confirmed root cause (pre-audited, not re-investigated):** Dhan rate-limits the
main-feed WS connect with HTTP 429. The 60s→300s `rate_limit_streak` backoff
(`connection.rs`) is in-memory only. When all connections are down for
`POOL_HALT_SECS=300` the pool watchdog returns Halt and the BOOT-ON path calls
`std::process::exit(2)` so a supervisor restarts the process. That restart wipes
the in-memory `rate_limit_streak` to 0, and the fresh process reconnects with a
0ms first retry (`compute_reconnect_base_delay_ms(0)=0`) straight back into Dhan's
still-active 429 window → instant 429 → 300s → exit → restart → infinite loop.
There is NO persisted cooldown today.

**Fix (FIX B — persist the cooldown so it survives a restart):** a tiny on-disk
JSON record of the last 429 hit + the current backoff floor. On boot, BEFORE the
first Dhan WS pool spawn, read it; if cooldown remains, `tokio::time::sleep` first
(capped at `WS_RATE_LIMIT_BACKOFF_CAP_MS`). Fail-open: a missing/corrupt/stale file
NEVER blocks boot.

Changed crates: **tickvault-core** (new persistence module + write wiring at the 429
site), **tickvault-common** (1 new `ErrorCode` variant), **tickvault-app** (boot
read+wait wiring).

New module `crates/core/src/websocket/rate_limit_cooldown.rs`:
- `cooldown_file_path()` — `$TV_WS_WAL_DIR`-sibling `./data/ws-rate-limit-cooldown.json`
  (same base-dir convention as `ws_wal_dir()`).
- `record_rate_limit_hit(now_epoch_ms, streak, floor_ms)` — best-effort std-fs write
  (atomic write to a tmp file + rename). A write error logs `error!(code=WS-GAP-08)`
  and returns — NEVER blocks the WS loop.
- `remaining_cooldown_ms(last_hit_epoch_ms, floor_ms, now_epoch_ms) -> u64` — PURE.
  Returns how long to still wait; 0 if expired, 0 if the record is older than the cap
  (stale), 0 if `now < last_hit` (clock went backwards). Cap-bounded by the caller.
- `read_cooldown() -> Option<PersistedCooldown>` — reads + parses; `None` on
  missing/corrupt/stale (fail-open).

Wiring:
- WRITE: at the 429 classification site in `connection.rs` (~line 1289), after
  `rate_limit_streak.fetch_add`, compute the new streak + floor and call
  `record_rate_limit_hit(...)` (best-effort).
- READ+WAIT: in `crates/app/src/main.rs` `start_dhan_lane`, immediately BEFORE the
  main-feed WS pool is created/spawned, read the cooldown; if `remaining > 0`,
  `info!` + increment `tv_ws_rate_limit_cooldown_waited_total` + `tokio::time::sleep`
  (the wait is already ≤ cap because `remaining_cooldown_ms` clamps to the floor and
  the floor itself ≤ cap; the caller additionally `.min(WS_RATE_LIMIT_BACKOFF_CAP_MS)`).

Observability: new `ErrorCode::WsGap08RateLimitCooldown` ("WS-GAP-08", Severity::Low,
runbook `wave-2-error-codes.md`); counter `tv_ws_rate_limit_cooldown_waited_total`.

## Edge Cases

- Missing file (first ever boot) → `read_cooldown()=None` → no wait. Boot normal.
- Corrupt/truncated JSON → parse fails → `None` → no wait (fail-open, never block).
- Stale file (older than cap) → `remaining_cooldown_ms=0` → no wait.
- Clock skew backwards (`now < last_hit`) → `remaining=0` (saturating).
- Floor larger than cap → caller `.min(cap)` bounds the actual sleep ≤ 300s.
- Write failure (disk full / read-only) → logs WS-GAP-08, never blocks the WS loop.
- Concurrent writers (multiple conns): each writes the same file; last-writer-wins is
  fine — it's a single advisory cooldown, not per-conn state. Atomic tmp+rename avoids
  torn reads.

## Failure Modes

- A bad/huge file can never hang boot: the sleep is `min(remaining, cap)` ≤ 300s.
- A persisted cooldown is advisory only — it never gates correctness, only the first
  connect timing. If the file is wrong, worst case is one extra/short wait, self-healing
  on the next successful connect (streak resets in-memory; file goes stale).
- No new tick-drop path; the WS read/reconnect engine + 429 backoff math are untouched.

## Test Plan

Crate: tickvault-core (`cargo test -p tickvault-core`), tickvault-common
(`cargo test -p tickvault-common` for the ErrorCode invariants).
- `remaining_cooldown_ms`: active (returns positive), expired (0), exactly-at-boundary,
  zero floor (0), now<last_hit (0), stale-older-than-cap (0).
- `read_cooldown`: corrupt-file fail-open returns None; round-trip write→read returns Some.
- ErrorCode invariants (existing tests cover unique code_str, FromStr roundtrip,
  runbook path exists, prefix pattern, all() exhaustive) — the new variant flows through.
- Cross-ref test `error_code_rule_file_crossref.rs` satisfied by the rule-file mention.

## Rollback

Single logical commit on a feature branch. `git revert <sha>` removes the module,
the write call, the boot read/wait, and the ErrorCode variant. No schema/data
migration, no config flag — the on-disk file is advisory and ignored if absent.

## Observability

- `error!(code = ErrorCode::WsGap08RateLimitCooldown.code_str(), ...)` on a persist
  write failure (→ 5-sink + Telegram).
- `info!(code = WS-GAP-08, ...)` operator-readable line when boot waits out a cooldown.
- Counter `tv_ws_rate_limit_cooldown_waited_total` (static label) incremented on each
  boot-time wait.
- New runbook section in `.claude/rules/project/wave-2-error-codes.md` (WS-GAP-08).

## Plan Items

- [x] Add `ErrorCode::WsGap08RateLimitCooldown` (+ code_str/severity/runbook/all) and a
  `.claude/rules/project/wave-2-error-codes.md` WS-GAP-08 section
  - Files: crates/common/src/error_code.rs, .claude/rules/project/wave-2-error-codes.md
  - Tests: existing error_code invariant tests + error_code_rule_file_crossref
- [x] New module `rate_limit_cooldown.rs` (pure `remaining_cooldown_ms`, `read_cooldown`,
  `record_rate_limit_hit`, `cooldown_file_path`) + unit tests
  - Files: crates/core/src/websocket/rate_limit_cooldown.rs, crates/core/src/websocket/mod.rs
  - Tests: test_remaining_cooldown_ms_active, test_remaining_cooldown_ms_expired_returns_zero,
    test_remaining_cooldown_ms_stale_record_is_zero, test_remaining_cooldown_ms_clock_backwards_is_zero,
    test_read_cooldown_corrupt_file_is_none, test_record_rate_limit_hit_round_trip_write_read,
    test_cooldown_file_path_is_under_data_dir
- [x] Wire WRITE at the 429 site in connection.rs
  - Files: crates/core/src/websocket/connection.rs
  - Tests: test_record_rate_limit_hit_round_trip_write_read
- [x] Wire READ+WAIT before the main-feed pool spawn in main.rs start_dhan_lane
  - Files: crates/app/src/main.rs
  - Tests: test_remaining_cooldown_ms_active
