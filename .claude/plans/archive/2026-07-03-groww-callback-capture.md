# Implementation Plan: Groww per-callback instant capture — our-side latency ≤1ms typical

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator directive 2026-07-03: "if ticks receivable within 1-3ms we must achieve the same latency")

> **Guarantee matrices:** this plan cross-references
> `.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical 15-row
> 100% Guarantee Matrix + 7-row Resilience Demand Matrix). Python sidecar +
> one Rust bridge field — no schema change, no new table, no strategy/order
> path; the applicable rows are proven in the Test Plan + Observability
> sections below. Honest 100% claim (§F wording): 100% inside the tested
> envelope — our-side ADDED latency ≤1ms TYPICAL / ≤5ms p99 under GIL +
> group-commit-fsync jitter (the honest CPython envelope); the EXTERNAL Groww
> server+NATS+network floor (~50–150ms, design study §4) is UNCHANGED and now
> SEPARATELY measurable via the new capture_ns column. Capture durability
> keeps the ring→spill→DLQ + WAL floor: one fsync per group-commit batch
> (bounded worst-case loss window = one in-flight batch on power loss,
> vs one record before — the notify-visible NDJSON row is written pre-fsync
> either way).

## Plan Items

- [x] Per-callback NATS hook (variant c2) + bounded queue + writer thread + group-commit
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: CaptureHookTests (test_dedup.py), _selftest_callback_capture
- [x] Walker demoted to reconcile sweep (GROWW_RECONCILE_INTERVAL_MS) + kill-switch (GROWW_CALLBACK_CAPTURE)
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: CaptureResolverTests, CaptureEmitTests::test_walker_reconcile_dedups_against_hook_emission
- [x] capture_ns NDJSON field (Python) + Rust bridge per-row received_at from capture_ns
  - Files: scripts/groww-sidecar/groww_sidecar.py, crates/app/src/groww_bridge.rs
  - Tests: test_parse_groww_tick_line_with_capture_ns, test_row_received_at_prefers_plausible_capture_ns, test_row_received_at_falls_back_without_capture_ns, test_row_received_at_falls_back_on_implausible_capture_ns, CaptureEmitTests::test_capture_emit_writes_capture_ns_row
- [x] Observability: CAPTURE-STATS stderr heartbeat + status-file counters (hook_captured / hook_dropped_full / reconcile_emitted)
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: covered by CaptureEmitTests + selftest (bounded-cadence line; additive status fields ignored by the Rust reader per serde defaults)

## Design

**Ground truth:** the 2026-07-03 latency study (variant c2 chosen, c1/walker
fallback). Today the sidecar's NATS callbacks are O(1) dirty-flag sets and a
walker drains the SDK snapshot every 200ms — our-side added latency averages
~100ms (walker wait) + a 768-decode full-tree walk. The SDK
(growwapi==1.5.0) invokes `self.callback(subject, data)` PER MESSAGE
(`nats_client.py:197`) where `callback` is a plain instance attribute read at
call time — hooking it captures the EXACT raw protobuf payload + subject per
message.

1. **Hook (NATS consumer thread, enqueue-only ~µs):** `make_capture_hook`
   wraps `feed._nats_client.callback`; stamps `time.time_ns()` as
   `capture_ns`, `put_nowait((subject, payload, capture_ns))` on a BOUNDED
   `queue.Queue(maxsize=CAPTURE_QUEUE_MAX=65536)`, then ALWAYS delegates to
   the original callback (SDK snapshot + dirty flags untouched). On
   `queue.Full` it drops to a counter — it never blocks the SDK consumer
   (the #1344 starvation class cannot recur). Re-attached each reconnect
   cycle (fresh GrowwFeed per cycle) BEFORE subscribing.
2. **Writer thread (process-lifetime daemon):** blocking `get()` →
   `get_nowait()` drain up to CAPTURE_BATCH_MAX=512 → per message: topic-map
   lookup (subject → kind/exchange/segment/token/security_id/canonical,
   built via the SDK's OWN `FeedConstants.get_live_price_topic` /
   `get_live_index_topic` so subjects + META match the SDK snapshot keys
   exactly) → ONE protobuf decode via the SDK's own `get_data_dict` (same
   leaf-dict shape as the walker) → the SAME `dedup_should_emit` cache and
   the SAME dedup keys as the walker → `_write_record(..., capture_ns=...,
   sync=False)` → `note_emit`. One `flush()+fsync` group-commit per batch.
   Watermark advance + RAW-TICK-PROBE observation preserved per record (the
   #1356 probe sees hook-captured ticks).
3. **Walker demotion:** with the hook attached, `_snapshot_walker_loop`
   sleeps `GROWW_RECONCILE_INTERVAL_MS` (default 5000ms, clamp
   [1000, 60000]) instead of `GROWW_WALK_INTERVAL_MS` — a reconciliation
   sweep that emits ONLY what the hook missed (shared dedup cache), counted
   as `reconcile_emitted` (the hook-health signal). Reconcile rows carry no
   capture_ns.
4. **Kill-switch + fallback:** `GROWW_CALLBACK_CAPTURE=off` (or 0/false/no)
   → never attach, walker-only at WALK_INTERVAL_MS (byte-identical pre-PR
   behaviour). Attach or topic-map failure (future wheel) → one stderr line
   + walker-only fallback — the feature degrades, never breaks.
5. **Rust bridge (`crates/app/src/groww_bridge.rs`):** `GrowwTickLine` gains
   `#[serde(default)] capture_ns: Option<i64>` (UTC epoch nanos). Pure
   `row_received_at_with_capture(capture_ns, wake_receipt_ist_nanos)`
   converts UTC→IST nanos (`+ IST_UTC_OFFSET_NANOS`) and uses it as the
   per-ROW `received_at` when plausible (above the ~2020 floor AND not ahead
   of the wake clock beyond GROWW_FUTURE_TS_TOLERANCE_NANOS); otherwise the
   pre-PR per-wake stamp. `received_at` now means true capture-at-receipt
   time; `received_at − ts` = external floor + ≤1ms ours;
   `capture_ns`-vs-wake delta = our pipeline hop, both SQL-measurable.
   Storage is UNCHANGED (`GrowwLiveTickRow.received_at_ist_nanos` is already
   `Option<i64>` → ILP micros).
6. **Threading model:** single-writer-per-counter (hook cells: NATS thread;
   CAPTURE_EMITTED_TOTAL: writer thread; RECONCILE_EMITTED_TOTAL: walker
   thread). `_OUT_LOCK` serializes every NDJSON record write + the batch
   fsync across the two writing threads (interleaved partial lines / rotation
   races impossible); `_STATUS_LOCK` serializes the status-file temp+rename
   (pid-based temp name is shared within the process).

## Edge Cases

- **Queue full (writer stalled / burst beyond 65,536):** hook drops to
  `_HOOK_DROPPED_FULL` (never blocks); writer folds the delta into
  `DROP_REASONS["capture_queue_full"]` via `note_drop_bulk`; the reconcile
  sweep still emits the latest snapshot values, so the FEED never goes dark.
- **Same-topic overwrite race (study §Q1 c1 risk):** does not apply to c2 —
  the hook captures the exact per-message payload, zero intra-window loss.
- **Duplicate delivery (hook + reconcile see the same print):** the shared
  `_LAST_EMITTED` cache with IDENTICAL keys swallows the second emission in
  either order (tested both directions). A rare cross-thread cache race can
  at worst double-emit one row — QuestDB DEDUP `(ts, security_id, segment,
  capture_seq, feed)` + the bridge's validation absorb it.
- **Old-format lines / mixed file (deploy boundary, reconcile rows):**
  `capture_ns` is `#[serde(default)]` — absent → per-wake fallback, exactly
  the pre-PR behaviour.
- **Clock skew / garbage capture_ns:** plausibility-gated (pre-2020 floor,
  future-vs-wake tolerance) → wake fallback; both broken → NULL (never a
  1970 stamp).
- **Index token names (e.g. "NIFTY"):** topic map resolves security_id from
  sid_map only — never coerces a name; numeric-token fallback is
  stocks-only (walker parity, tested).
- **Index META segment:** the SDK stamps index topic meta segment as CASH
  regardless of the subscribe entry — the topic map follows the META (same
  keys as the walker's tree paths), tested with a faithful fake.
- **Unknown subject (SDK-internal topic):** `capture_subject_miss` drop
  counter, never a raise.
- **Reconnect cycle:** fresh feed → fresh hook attach; queue + writer thread
  + counters are process-lifetime; topic map rebuilt (identical contents).
- **IST-midnight rotation:** `_RotatingOut` rotation now runs under
  `_OUT_LOCK` via `_write_record`, so the two writing threads cannot race
  the rotate.

## Failure Modes

- **Hook attach fails (SDK internals renamed):** `install_callback_capture`
  / `build_capture_topic_map` return False/None on ANY exception → one
  stderr line → `_CAPTURE_MODE=False` → walker runs at full WALK_INTERVAL_MS
  cadence — the current shipped path, unchanged (c1-fallback contract).
- **Writer thread dies (per-item broad except makes this practically
  unreachable):** the reconcile sweep still emits everything at its slow
  cadence; the frozen-watermark force-close + Rust FEED-STALL-01
  process-kill remain the outer backstops — never a silent-forever failure.
- **Decode error on one payload:** `capture_decode_error` drop counter,
  writer continues (never dies on one bad message).
- **fsync stall (slow disk):** serializes only the writer thread's batch
  commit; the hook keeps enqueueing into the 65,536-slot buffer; overflow is
  counted, never blocking.
- **Starvation regression risk:** the hook is provably enqueue-only (one
  clock read + put_nowait + two int bumps + delegate) — unit test proves
  10,000 full-queue calls complete non-blocking with delegation intact.

## Test Plan

- Python: `cd scripts/groww-sidecar && python3 -m unittest test_dedup -v` —
  60 tests green (16 new: CaptureResolverTests ×3, CaptureHookTests ×6,
  CaptureTopicMapTests ×2, CaptureEmitTests ×5) proving (a) hook enqueue is
  non-blocking + bounded, (b) queue-full drops are counted not blocking,
  (c) walker reconciliation dedups against hook emissions (both orders),
  (d) kill-switch + attach-failure fall back cleanly, (e) capture_ns lands
  on the NDJSON row and is omitted on legacy/reconcile rows.
- Python selftest: `_selftest_callback_capture` added to
  `python3 groww_sidecar.py --selftest` (runs in the deployed env).
- Rust: `cargo test -p tickvault-app --lib` (757 green, includes the 4 new
  capture_ns parse/received_at tests) + `cargo test -p tickvault-storage
  --lib` (665 green, storage untouched) + `cargo fmt --check` +
  `cargo clippy -p tickvault-app --no-deps` (no new warnings; the two
  pre-existing main.rs warnings are untouched).
- Hooks: banned-pattern-scanner, plan-gate, per-item-guarantee-check,
  pre-push fast gates — green before push.

## Rollback

- **Runtime (no deploy):** set `GROWW_CALLBACK_CAPTURE=off` on the sidecar
  env → walker-only at WALK_INTERVAL_MS, byte-identical pre-PR capture
  behaviour on the next process start.
- **Code:** single revert of this PR's squash commit. The NDJSON schema
  change is additive (`capture_ns` optional) — a reverted bridge simply
  ignores the extra field on already-written lines (serde ignores unknown
  fields), and a reverted sidecar stops writing it. No data migration, no
  table change, no config-file change.

## Observability

- **stderr (→ CloudWatch `/tickvault/prod/app`):** one-time
  "per-callback capture ARMED" / "DISABLED" / "hook NOT attached" lines;
  bounded `CAPTURE-STATS hook_captured=… hook_enqueued=…
  hook_dropped_full=… reconcile_emitted=… qsize=…` heartbeat ≤1/300s;
  bounded DROP samples for the new reasons (capture_queue_full,
  capture_subject_miss, capture_decode_error, capture_emit_error).
- **Status file (→ Rust feed-health, additive fields):** `hook_captured`,
  `hook_dropped_full`, `reconcile_emitted` alongside the existing
  emitted/dropped/deduped counters.
- **QuestDB (the measured proof):** `received_at` on `ticks feed='groww'`
  rows now carries per-message capture time — post-ship,
  `min/avg(datediff(...received_at, ts))` directly measures the external
  Groww floor (the walker-wait smear is gone), and `reconcile_emitted`
  staying ≈0 proves the hook path captures everything. No new metrics /
  alerts / audit tables — the existing feed-health + FEED-STALL-01 +
  watermark-lag chains cover the failure modes; drop accounting reuses the
  existing note_drop plumbing (no new ErrorCode; no ratchet forces one).
