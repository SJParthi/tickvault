# Implementation Plan: Groww live-tick durability + candle generation — ILP reconnect + replay (zero-drop + candles seal in Groww-only mode)

**Status:** APPROVED
**Date:** 2026-06-30
**Approved by:** Parthiban (operator) — grounded directive this session: "harden the ILP flush to reconnect + replay on a connection error before falling through to the spill/DLQ net. The operator wants ZERO drop at the open." + URGENT EXPANSION (same session): "make candles seal in Groww-only mode (Dhan OFF) — 3.6M groww ticks but 0 candles_1m."

## URGENT EXPANSION — Part B root cause (candle generation, the operator's #1 blocker)

**Symptom (live 09:16 IST):** `ticks` feed='groww' = 3,629,720 rows (flowing +
persisting), but `candles_1m` feed='groww' = 0 rows. Dhan OFF, Groww ON.

**Root cause (file:line) — NOT the coordinator's leading hypothesis.** The Groww
bridge IS fully wired into the candle path: `groww_bridge.rs:714` calls
`self.aggregator.consume_tick(..., FeedStrategy::GROWW, ...)` with a seal closure
that routes via `crate::seal_routing::route_seal(SealRouteParams { feed:
Feed::Groww, ... }, ...)` (`groww_bridge.rs:720-737`) → `global_seal_sender()`.
`route_seal` (`seal_routing.rs:144`) stamps `feed=Feed::Groww` into the
`BufferedSeal` and `try_send`s it; the seal-writer drains it via
`ShadowCandleWriter::append_seal` → plain `candles_1m` tagged `feed=groww`
(`shadow_candle_writer.rs:203-214`). The wiring is CORRECT.

**The actual bug:** `ShadowCandleWriter` (the candle ILP writer the seal-writer
drains into) has NO reconnect — same class as the tick-flush bug, but worse.
`shadow_candle_writer.rs:75-95`: the `ilp_conf_string` field is retained "for
the reconnect logic" but is `#[allow(dead_code)]` with "no callers in this
slice" — **the reconnect was never implemented.** `ShadowCandleWriter::flush()`
(`shadow_candle_writer.rs:260-278`) returns `Err` when disconnected and never
rebuilds the sender. At the 09:00 open-burst the SAME QuestDB socket reset
(broken pipe) that hit the tick writer also broke the candle ILP sender; once
broken, `questdb::Sender` is `must_close()`/`connected=false`, so EVERY
subsequent `flush()` returns `SocketError` → `drain_once`
(`seal_writer_task.rs:172-190`) rescues every sealed candle to spill/DLQ
(logging `AGGREGATOR-SEAL-01`) and ZERO rows ever reach `candles_1m` for the
rest of the session. This affects BOTH feeds; Groww is the visible victim
because Dhan is OFF. It is the SAME reconnect-on-broken-pipe fix as Part A,
applied to the candle writer — NOT an aggregator redesign.

**Guarantee matrix:** this item carries the operator-mandated 15-row + 7-row
guarantee matrix by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (per-item-guarantee-check
hook accepts the cross-reference form). The honest-100% envelope wording is in
the Failure Modes section below.

## Diagnosis (cite file:line)

- **Failing flush call:** `crates/storage/src/groww_persistence.rs:276` —
  `GrowwLiveTickWriter::flush()` calls `sender.flush(&mut self.buffer)`. On a
  broken-pipe / connection-reset, questdb-rs returns
  `questdb::Error { code: ErrorCode::SocketError, msg: "Could not flush buffer: ..." }`
  (questdb-rs-6.1.0 `error.rs:118-122` + `sender/mod.rs:170-178`,
  `map_io_to_socket_err`). This is the EXACT error the operator saw
  ("Could not flush buffer: Broken pipe (os error 32)").
- **Bridge call site:** `crates/app/src/groww_bridge.rs:763-770` — `match
  self.live_writer.flush()` → on `Err` logs `error!(?err, "groww bridge: shared
  ticks (feed=groww) flush failed")`. NO retry within the wake.
- **Writer is Groww-specific** (NOT shared with Dhan): Dhan uses
  `TickPersistenceWriter` (`tick_persistence.rs`); Groww uses the separate
  `GrowwLiveTickWriter` (`groww_persistence.rs`). The fix is Groww-scoped — but
  it mirrors the Dhan writer's existing drop-and-reconnect pattern
  (`tick_persistence.rs:580-587`).
- **Current behavior on flush Err (confirmed, NOT loss-on-this-path):** questdb
  `Sender::flush(buf)` clears the buffer ONLY on `Ok` —
  `flush_impl(buf,...)?` then `buf.clear()` (`sender/mod.rs:268-272`). On a
  `SocketError` the `?` returns early, so the buffer is RETAINED with all rows.
  The current writer already sets `self.sender = None` and returns `Err`; the
  NEXT wake's `flush()` reconnects via the retained `ilp_conf` and re-sends the
  SAME buffer. So on THIS in-process path no tick is lost — the buffer holds the
  rows until the next wake reconnects. The ~0.28% "dropped at open" is the
  producer-side capture-at-receipt / sidecar spill net counter (lock §32), the
  durable floor downstream of the socket, NOT this DB flush path.
- **Why the fix still matters (honest framing):** the current design defers
  recovery to the NEXT wake (next tick), so during a quiet post-burst moment the
  retained rows sit unflushed and the open-bell socket reset causes a visible
  recovery LAG (and the spill round-trip the operator wants to avoid). Doing the
  reconnect + replay INLINE, in the SAME flush call, recovers within the same
  wake — zero recovery lag, no reliance on a follow-up tick, fewer scary
  `error!` lines.

## Plan Items

- [x] Item 1 — Add a pure connection-error classifier + bounded backoff schedule
  to `groww_persistence.rs`. `is_connection_error(&questdb::Error) -> bool`
  returns true ONLY for `ErrorCode::SocketError` (broken pipe / reset / not
  connected) — NOT for `InvalidApiCall`, `InvalidName`, `ServerFlushError`, etc.
  (those are not transport faults; retrying re-sends a bad row). Named
  constants `GROWW_FLUSH_RECONNECT_MAX_RETRIES = 3` and
  `GROWW_FLUSH_RECONNECT_BACKOFF_MS = [50, 100, 200]`.
  - Files: crates/storage/src/groww_persistence.rs
  - Tests: test_is_connection_error_true_for_socket_error,
    test_is_connection_error_false_for_non_socket_error,
    test_reconnect_backoff_schedule_caps_at_three

- [x] Item 2 — Rework `GrowwLiveTickWriter::flush()` into reconnect+replay. On a
  flush Err that is a connection error: drop the sender, reconnect via retained
  `ilp_conf`, re-flush the SAME buffer (rows retained — confirmed), bounded to
  N retries with the backoff sleep BETWEEN attempts. Return a typed outcome so
  the caller knows recovered-vs-gave-up: `flush()` returns
  `Result<GrowwFlushOutcome>` where `GrowwFlushOutcome { persisted: usize,
  reconnected: bool }`. A NON-connection error returns `Err` immediately (no
  retry). After N exhausted retries it returns `Err` (buffer + pending RETAINED
  — the spill/DLQ net stays the last resort). Increment static-label counters
  `tv_groww_ilp_reconnect_attempts_total` and
  `tv_groww_ilp_reconnect_recoveries_total`.
  - Files: crates/storage/src/groww_persistence.rs
  - Tests: test_flush_outcome_struct_fields, test_non_connection_error_no_retry,
    test_disconnected_flush_retains_buffer_and_pending (existing — still holds),
    test_flush_when_disconnected_errors_not_panics (existing — still holds)

- [x] Item 3 — Update the bridge call site to consume `GrowwFlushOutcome`. On
  `Ok(outcome)`: record `outcome.persisted` ticks; if `outcome.reconnected`,
  log `info!` (recovered — NOT a scary error). On `Err` (retries exhausted →
  falling through to spill): keep the `error!(... "flush failed")` so
  error_level_meta_guard stays satisfied and the operator is paged on a true
  give-up.
  - Files: crates/app/src/groww_bridge.rs
  - Tests: covered by storage unit tests for the writer; the bridge match arm is
    a thin consumer (TEST-EXEMPT cold-path tokio driver, like the existing one).

- [x] Item 4 (Part B) — Add reconnect to `ShadowCandleWriter::flush()` so a
  broken-pipe candle flush rebuilds the ILP sender from the retained
  `ilp_conf_string` and re-flushes the SAME (retained) buffer, bounded retries +
  backoff, BEFORE `drain_once` rescues the seals to spill. This restores
  `candles_1m` generation for BOTH feeds (Groww visible today). Mirrors the
  Part A pattern. The `#[allow(dead_code)]` on `ilp_conf_string` is removed (it
  is now read by the reconnect path). On flush Err, set `sender = None` so the
  reconnect path runs; buffer retained (questdb `flush` doesn't clear on Err) so
  replay re-sends the same candles; DEDUP `(ts, security_id, segment, feed)`
  collapses any partial write — idempotent.
  - Files: crates/storage/src/shadow_candle_writer.rs
  - Tests: test_shadow_writer_reconnect_on_connection_error_retries,
    test_shadow_writer_retains_buffer_and_pending_after_failed_flush,
    test_shadow_writer_non_connection_error_no_retry (reuse the shared
    `is_connection_error` classifier from groww_persistence)

- [x] Item 5 (Part B) — End-to-end test: a synthetic Groww tick crossing a
  minute boundary produces a sealed `BufferedSeal` tagged `feed=groww` that the
  `ShadowCandleWriter` serialises to the `candles_1m` table with `feed=groww` on
  the wire. Proves the tick → aggregator → seal → candle path emits a
  Groww-tagged candle (drives the aggregator with synthetic groww ticks, asserts
  a seal with feed=Groww, asserts the candle writer's ILP bytes carry
  `candles_1m` + `feed=groww`). Dhan parity: a Dhan seal still stamps
  `feed=dhan` (existing test stays green).
  - Files: crates/storage/tests/groww_candle_generation_e2e.rs
  - Tests: test_groww_tick_seals_candle_to_candles_1m_with_feed_groww,
    test_dhan_tick_seals_candle_with_feed_dhan_same_path

## Scope: ALL 21 timeframe tables + feed-agnostic (operator clarification 2026-06-30)

**Architecture verified (one common engine → all 21 TFs):**
`MultiTfAggregator::consume_tick` (`multi_tf_aggregator.rs:365`) loops
`for tf in TfIndex::ALL` (the SINGLE common 21-TF engine) and calls
`on_seal(tf, sealed_state)` for EVERY TF whose bucket boundary crossed — so ONE
tick into the one common entry fans out to ALL 21 timeframes. `ShadowCandleWriter::append_seal`
dispatches each seal to `seal.tf.table_name()` (`tf_index.rs:175` → `candles_1m`,
`candles_2m`, … `candles_1d` — 21 tables, `TF_COUNT=21`) and stamps `feed` on
EVERY row (`shadow_candle_writer.rs:append_row` `.symbol("feed", row.feed)`), no
per-TF and no per-feed branch. There is NO separate path for any TF — all 21
flow from the one aggregator. (Groww routes all 21 incl. D1: the bridge sets
`drop_d1: false`; the Dhan path drops D1 at the write boundary per
`live-feed-purity.md` rule 10, which is a Dhan policy, not a separate engine.)

- [x] Item 6 (Part B) — Generality test across BOTH dimensions:
  (a) ACROSS TIMEFRAMES: write one seal for EVERY `TfIndex::ALL` (21) tagged an
  arbitrary novel feed and assert each lands in its OWN `candles_<tf>` table
  tagged with that feed — proving the writer covers all 21 TF tables generically.
  (b) ACROSS FEEDS from the aggregator: drive the aggregator so a single tick
  seals MULTIPLE TFs at once (cross a boundary that closes M1 + larger TFs), each
  carrying the tick's feed.
  - Files: crates/storage/src/shadow_candle_writer.rs (writer-side all-21),
    crates/storage/tests/groww_candle_generation_e2e.rs (aggregator multi-TF)
  - Tests: test_candle_writer_covers_all_21_tf_tables_for_arbitrary_feed,
    test_one_tick_seals_multiple_timeframes_from_common_engine

## Design

The fix lives entirely in `GrowwLiveTickWriter::flush()` where the RAW
`questdb::Error` is available BEFORE it is wrapped in `anyhow` — so the
connection-error class (`ErrorCode::SocketError`) can be matched precisely. The
buffer is naturally retained on a questdb flush error (`Sender::flush` only
`buf.clear()`s on `Ok`), so re-flushing the SAME buffer through a freshly
reconnected sender re-sends exactly the rows that did not land. Replay is
idempotent because the shared `ticks` DEDUP key is
`(ts, security_id, segment, capture_seq, feed)` — a partial write that reached
QuestDB before the pipe broke collapses on replay (same `capture_seq`), never
double-counts. Retries are bounded (3) with short exponential backoff
(50/100/200ms, total ≤ 350ms) so a sustained outage degrades to the existing
spill/DLQ net instead of stalling the wake forever.

## Edge Cases

- Buffer empty at flush → existing pending-gate in the bridge (`pending() > 0`)
  means flush isn't called on an empty buffer; the writer's reconnect path is a
  no-op flush of an empty buffer anyway (questdb returns `Ok` on empty bytes).
- Sender already `None` at entry (never connected / dropped by a prior wake) →
  reconnect FIRST, then flush, then the same retry loop applies — unchanged from
  today plus the new bounded retry.
- A non-connection error (e.g. `InvalidName` from a schema drift) → NO retry,
  return `Err` immediately so a genuinely bad row is not re-hammered.
- Reconnect itself fails (QuestDB down) → counts as one failed attempt; after N
  exhausted, return `Err`, buffer retained, spill net is the floor.
- Partial write before the pipe broke → DEDUP collapses the replay (idempotent).

## Failure Modes

- **Sustained QuestDB outage:** retries exhaust in ≤350ms, `flush()` returns
  `Err`, buffer + pending RETAINED, bridge logs `error!` (give-up → spill is the
  last resort, unchanged). NEVER an infinite loop, NEVER a hot-path stall beyond
  the bounded backoff total.
- **Honest 100% envelope (per per-wave-guarantee-matrix.md §"honest 100% claim"
  + operator-charter §F):** "100% inside the tested envelope: a transient
  QuestDB ILP socket reset (broken pipe / connection reset) is recovered IN the
  same flush wake via a bounded reconnect + idempotent replay (≤3 attempts,
  50/100/200ms backoff); replay is collapse-safe via the
  `(ts, security_id, segment, capture_seq, feed)` DEDUP key. Beyond the bounded
  retries, the existing producer capture-at-receipt spill/DLQ net catches every
  payload as recoverable text — never an infinite loop, never a silent drop."

## Test Plan

- Pure unit tests (no live QuestDB) for the classifier + backoff schedule + the
  `GrowwFlushOutcome` struct + the no-retry-on-non-connection-error path + the
  existing buffer-retention invariants (already green, must stay green).
- `cargo test -p tickvault-storage` (covers the writer + the
  `per_item_guarantee_matrix_guard` + `error_level_meta_guard` +
  `dedup_segment_meta_guard`).
- `cargo test -p tickvault-app` (the bridge call-site compiles + the bridge's
  existing tests stay green).
- `bash .claude/hooks/banned-pattern-scanner.sh`, `pub-fn-test-guard.sh`,
  `plan-gate.sh`, `pre-push-gate.sh` all exit 0.

## Rollback

Single-commit revert restores the prior `flush() -> Result<usize>` +
drop-and-reconnect-next-wake behavior. No schema change, no migration, no
config flag — the change is internal to the writer + its one call site, so a
`git revert <sha>` is clean. The running production process is a separate
already-built binary (operator redeploys on their own schedule), so reverting
the repo has no live-feed impact.

## Observability

- New static-label counters: `tv_groww_ilp_reconnect_attempts_total` (each
  reconnect attempt) and `tv_groww_ilp_reconnect_recoveries_total` (each wake
  recovered without falling to spill). Zero-alloc emission (static labels).
- `info!` on a recovered reconnect (NOT `error!` — avoids re-spamming the
  operator for a self-healed transient). `error!(... "flush failed")` retained
  ONLY on the true give-up-to-spill path (error_level_meta_guard Rule 5).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Broken pipe at open-burst, QuestDB recovers immediately | reconnect on attempt 1, replay the retained buffer, `Ok(outcome{reconnected:true})`, `info!`, ticks recorded, zero drop |
| 2 | QuestDB down for the whole wake | 3 attempts exhaust (≤350ms), `Err`, buffer retained, bridge `error!` → spill net |
| 3 | Schema-drift / InvalidName error | NO retry, `Err` immediately |
| 4 | Partial write then pipe break | replay re-sends; DEDUP collapses the already-landed rows (idempotent) |
| 5 | Sender None at entry (cold) | reconnect-first then flush, same bounded retry |
