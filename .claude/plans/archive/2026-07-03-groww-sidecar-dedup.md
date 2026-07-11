# Implementation Plan: Groww sidecar snapshot change-dedup + timestamp-advancing liveness

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — live-bug directive 2026-07-03 ("build the groww-sidecar fix PR from today's forensics")

> Forensics: 2026-07-03 live incident — 530K duplicate `feed='groww'` rows
> 09:00–09:17 IST, zero groww candles. Verified root cause: the sidecar's
> `on_index` callback re-emits ALL ~25 subscribed indices' snapshot entries on
> EVERY NATS callback with zero change-dedup (~525 duplicate rows/sec, all
> frozen at the 09:00:00.183 pre-open print). The aggregator's 09:15 session
> gate (INTENDED + test-pinned, `test_groww_pre_open_minute_is_gated_intended`)
> correctly discards pre-09:15-stamped ticks — the Rust candle engine is NOT
> touched by this plan. Second (masking) bug: liveness counts the duplicates as
> live ticks, so a frozen feed looks alive and FEED-STALL-01 never fires.

## Design

Two minimal, independent fixes — Python sidecar emit-path + Rust bridge
liveness gate. No change to the candle engine, the session gate, the WAL
chain, or any Dhan path.

1. **Sidecar change-dedup** (`scripts/groww-sidecar/groww_sidecar.py`): a
   module-level bounded dict `_LAST_EMITTED[(kind, exchange, segment, token)]
   = (ts_millis, price)` consulted by a pure helper `dedup_should_emit(cache,
   key, ts_millis, price)` in BOTH emit paths (`emit_ltp_records` — stocks —
   and `emit_index_records` — indices; both have the identical
   flatten-the-whole-snapshot pattern). An entry identical in BOTH `tsInMillis`
   and price to the last-emitted one is skipped (counted via `note_dedup()`;
   `deduped` added to the status-file record). O(1) dict lookup per entry;
   naturally bounded by the subscribed universe (~768 + 25 keys). This kills
   the ~25x duplicate flood and the wasted per-row fsync.
2. **Timestamp-advancing liveness** (`crates/app/src/groww_bridge.rs`): the
   parse-time `record_feed_liveness` stamp (which feeds
   `FeedHealthRegistry::last_tick_age_secs`, read by the FEED-STALL-01 stall
   watchdog `should_restart_on_stall`) is gated by a new pure helper
   `liveness_ts_advanced(&mut self.liveness_max_exchange_ts_nanos, wake_max)`:
   liveness is stamped ONLY when the max `exchange_timestamp` (ts_ist_nanos)
   seen across parsed lines ADVANCES past the max ever seen this bridge
   lifetime. A feed re-dumping a frozen snapshot (same ts, same or changing
   price) no longer counts as alive; the sidecar dedup alone would leave the
   frozen-ts-with-changing-price case masked, so both halves are needed.
   The stamped VALUE stays the wall-clock receipt time (consistent with the
   `/api/feeds/health` age math); ordering vs the flush arm is unchanged
   (parse-time, ratchet `test_parse_time_liveness_is_recorded_before_flush`).

3. **Python stall detector on advancing timestamps** (feed-death forensics
   2026-07-03: snapshot froze 09:07:55, decode count climbed to 1.16M "alive"
   records for 31 min, neither stall layer fired until Groww closed the socket
   at 09:38:53): `_stall_recovery_loop` now keys on the ADVANCE of
   `MAX_TS_MILLIS_SEEN` (a module watermark advanced by the pure
   `ts_watermark_advanced` for EVERY decoded record — emitted, deduped, or
   dropped) instead of the decoded count, through the SAME unchanged pure
   `should_force_reconnect` decision. A frozen-snapshot re-dump now
   force-closes the NATS socket within the deadline during market hours.
   Fresh-but-dropped records still count as socket-alive (a mapping bug must
   not cause a reconnect storm).
4. **note_drop reason breakdown + capped samples**: `note_drop(reason,
   exchange, segment, token, detail)` with per-reason counters
   (`no_price_field` / `sid_map_miss` / `coerce_error` / `unmapped_segment` —
   the last was previously a fully SILENT subtree skip) carried in the status
   file (`dropped_reasons`) + at most 5 sample stderr lines per reason
   (market-data identifiers only). Needed to attribute the live incident's
   dropped=276,077 (started exactly at the first get_ltp() callback). Code
   analysis verdict: a STOCK sid_map miss cannot drop (numeric-token fallback);
   the breakdown will name the real reason on the next live session.
5. **Security clamp (High)**: botocore/boto3/urllib3/s3transfer loggers clamped
   to WARNING — the DEBUG root logger was printing the SigV4 CanonicalRequest
   incl. the full `x-amz-security-token` to stderr → CloudWatch on every
   sidecar relaunch SSM read.

The sidecar's own in-process stall recovery also becomes honest via the
watermark: dedup skips are neither emits nor drops, and even a
frozen-ts-with-changing-price flood no longer counts as liveness.

Guarantee matrices: this plan cross-references the canonical 15-row + 7-row
matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` (Per-Item
Guarantee Matrix); the resilience rows relevant here: no new tick-drop path
(dedup skips only EXACT (ts, price) re-dumps of already-captured data; the
first print of every distinct value is still captured-at-receipt + fsynced),
O(1) per entry, bounded memory, WAL/ring/spill/DLQ untouched.

## Edge Cases

- Genuine LTP change with identical price but advancing `tsInMillis` → dedup
  key includes ts_millis → EMITTED (never swallowed).
- Same ts_millis, changed price (two prints in the same millisecond snapshot
  slot) → EMITTED (price differs).
- Sidecar restart → `_LAST_EMITTED` resets → one duplicate burst re-emitted →
  absorbed by the `ticks` DEDUP UPSERT keys (idempotent), not data corruption.
- Bridge restart → `liveness_max_exchange_ts_nanos` resets to 0 → first parsed
  wake (even a byte-0 re-tail of old lines) stamps liveness once; the stall
  watchdog needs a full sustained window of silence, so detection is delayed by
  at most one stall window after restart — acceptable, honest.
- Byte-0 re-tail mid-lifetime (file shrank) → replayed old lines have
  ts ≤ max seen → liveness NOT stamped by replay (correct — replay is not
  fresh delivery).
- `tsInMillis = 0` records → `ts_ist_nanos = 0` never advances the max;
  bridge `validate_groww_tick` already rejects out-of-range timestamps.
- IST-midnight NDJSON rotation → next day's ts > yesterday's max → advances.
- Index vs stock token collision (BSE SENSEX token "1" vs a hypothetical BSE
  stock token "1") → dedup key carries a `kind` discriminant ("ltp"/"idx").
- Non-float price payloads → coerced via `float()` inside the existing
  try/except; a coercion failure is counted as a drop exactly as before.

## Failure Modes

- Dedup dict grows only with distinct subscribed (kind, exchange, segment,
  token) tuples → bounded by the watch set (Groww hard cap 1000) — no
  unbounded memory.
- If Groww starts sending genuinely new data every callback, dedup admits
  everything → behavior identical to today (no regression path).
- If the liveness gate were wrong-way (too strict), the worst case is the
  stall watchdog killing + relaunching a healthy sidecar (FEED-STALL-01 is
  auto-triage-safe, storm-bounded, and re-auth/re-subscribe is idempotent) —
  but a healthy in-market feed advances tsInMillis every second across ~768
  instruments, so the gate cannot starve on real data.
- Status-file `deduped` field: the Rust `GrowwStatusLine` parser uses serde
  default field semantics (unknown fields ignored) — no parse break.
- Honest envelope: candles for pre-09:15-stamped groww data are intentionally
  never formed (session gate, test-pinned); today's zero-candle window was the
  pre-open snapshot flood + the intended gate. This plan does NOT change that.

## Test Plan

- Python (new, `scripts/groww-sidecar/test_dedup.py`, runnable via
  `python3 -m unittest` with a stubbed `growwapi` module — no SDK needed):
  `test_first_emit_allowed`, `test_identical_ts_and_price_skipped`,
  `test_advancing_ts_same_price_emitted`, `test_same_ts_changed_price_emitted`,
  `test_keys_are_independent`, `test_cache_bounded_by_distinct_keys`,
  `test_kind_discriminates_index_vs_stock`.
- Python `--selftest` harness: `_selftest_dedup` added alongside
  `_selftest_redaction` / `_selftest_self_heal`.
- Rust (`cargo test -p tickvault-app --lib`): `test_liveness_ts_advanced_first_tick`,
  `test_liveness_ts_advanced_frozen_ts_does_not_stamp`,
  `test_liveness_ts_advanced_replay_lower_ts_does_not_stamp`,
  `test_liveness_ts_advanced_monotonic_updates`,
  `test_liveness_gate_is_wired_into_drain` (source-scan ratchet), and the
  existing `test_parse_time_liveness_is_recorded_before_flush` stays green.
- Scoped per `testing-scope.md`: `cargo test -p tickvault-app --lib` + fmt +
  scoped clippy. CI runs the full battery post-push. The Python unittest file
  is NOT wired into CI (no Python harness exists in CI today) — noted in the
  PR body.

## Rollback

Single revert of this PR's squash commit restores the pre-fix sidecar +
bridge. No schema change, no config change, no migration: the `deduped`
status field disappears harmlessly (serde-default), the dedup map and the
liveness max are process-local state. The `ticks` DEDUP keys make any
re-emission after rollback idempotent.

## Observability

- `deduped` count carried in the sidecar status file (visible via the bridge
  status reads + the existing emitted/dropped counters).
- Liveness gating is observable through the existing
  `tv_feed_sidecar_stall_restart_total{feed}` (FEED-STALL-01) — a frozen
  snapshot now surfaces as a stall restart instead of being masked.
- No new ErrorCode: FEED-STALL-01 (`feed-stall-watchdog-error-codes.md`)
  already documents the kill+relaunch path this fix un-masks.
- Existing per-feed health endpoint (`last_tick_age_secs`) now reflects
  FRESH delivery, not re-dumps — the metric's meaning sharpens, no rename.

## Plan Items

- [x] Item 1 — Sidecar change-dedup in both emit paths + `note_dedup` counter + status field
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: _selftest_dedup, test_identical_ts_and_price_skipped
- [x] Item 2 — Python unittest file for the pure dedup helper
  - Files: scripts/groww-sidecar/test_dedup.py
  - Tests: test_first_emit_allowed, test_advancing_ts_same_price_emitted, test_same_ts_changed_price_emitted
- [x] Item 3 — Bridge timestamp-advancing liveness gate + unit tests + wiring ratchet
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_liveness_ts_advanced_frozen_ts_does_not_stamp, test_liveness_gate_is_wired_into_drain
- [x] Item 4 — Groww official docs pack (operator-uploaded 2026-07-03) into docs/groww-ref/
  - Files: docs/groww-ref/00-INDEX.md, docs/groww-ref/01-introduction-auth-ratelimits.md, docs/groww-ref/07-feed-websocket-streaming.md, docs/groww-ref/09-instruments-csv.md, docs/groww-ref/12-sdk-exceptions.md, docs/groww-ref/13-annexures-enums.md, docs/groww-ref/README.md
  - Tests: N/A — docs only
- [x] Item 5 — Python stall detector keys on advancing max tsInMillis watermark
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: test_frozen_ts_does_not_advance, test_older_replayed_ts_does_not_advance, test_garbage_ts_never_raises_and_never_advances
- [x] Item 6 — note_drop per-reason breakdown + capped samples + status-file dropped_reasons
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: N/A — counter/logging plumbing; the pure decisions it feeds are tested above (TEST-EXEMPT class)
- [x] Item 7 — botocore/boto3/urllib3 log-level clamp (x-amz-security-token leak)
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: N/A — logging config one-liner; verified by absence of botocore DEBUG lines on next relaunch

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww re-dumps 25 frozen index snapshot entries per NATS callback | 1 emission per distinct (ts, price) per instrument; rest skipped + counted |
| 2 | Index value ticks forward every second in-session | every change emitted; candles form once ts ≥ 09:15 IST (intended gate) |
| 3 | Feed freezes mid-session (ts stops advancing) | liveness stops advancing → FEED-STALL-01 kill+relaunch fires (no longer masked) |
| 4 | QuestDB outage with healthy feed | parse-time liveness still stamps on advancing ts — no false stall kill (2026-07-02 fix preserved) |
| 5 | Sidecar restart | dedup map resets; one duplicate burst absorbed by ticks DEDUP keys |
