# Bounded WAL Frame Re-Injection — Error Codes (WS-REINJECT-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file.
> **Companion code:** `crates/app/src/wal_reinject.rs` (`reinject_wal_frames` /
> `ReinjectOutcome`), the two STAGE-C.2b call sites in `crates/app/src/main.rs`
> (fast boot inside `async fn main()` + the slow-boot mirror inside
> `start_dhan_lane`), constants `WAL_REINJECT_CHUNK_SIZE` (8,192) +
> `WAL_REINJECT_SEND_TIMEOUT_SECS` (30) +
> `WAL_REINJECT_PROGRESS_LOG_CHUNKS` (16) in `crates/common/src/constants.rs`,
> `crates/common/src/error_code.rs::ErrorCode::WsReinject01Aborted`.
> **Companion rules:** `ws-frame-spill-error-codes.md` (the WAL durable floor
> this path replays), `wave-2-error-codes.md` (WS-GAP-09 — the pool-halt
> restart that triggered the 2026-07-03 storm), `live-feed-purity.md`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `WsReinject01*` variant verbatim —
> `WsReinject01Aborted` and `WS-REINJECT-01` appear below.

---

## §0. Why this code exists (the 2026-07-03 10:35 IST re-injection storm)

At boot, STAGE-C.2b re-injects WAL-replayed LiveFeed frames into the pool's
frame mpsc channel (`FRAME_CHANNEL_CAPACITY = 131,072`) so the tick processor
drains them ahead of fresh live frames (QuestDB DEDUP keys make the replay
idempotent). Before this fix, both call sites drained the ENTIRE replay Vec
through a synchronous `try_send` loop — everything past the channel capacity
was silently dropped (observed `dropped=1,127,801` at 10:35 IST on
2026-07-03, a mid-market WS-GAP-09 pool-halt restart).

Worse, the drop made the re-injection NOT-clean, so `confirm_replayed()`
never archived the staged WAL segments out of `replaying/` — they were
re-globbed and re-replayed AND GREW on every restart: a **self-feeding
storm**. The drop was logged `error!` with a raw counter but NO typed
`code =` field (operator-charter Rule 5 violation).

**The fix (C3, 2026-07-03):** both call sites now delegate to
`wal_reinject::reinject_wal_frames`, which:

1. Uses backpressured `sender.send(frame).await` — waits for the consumer,
   NEVER drops while the channel is open. This is a COLD, once-per-boot
   recovery path (NOT the per-tick hot path at `connection.rs`), so awaiting
   is correct.
2. Yields (`tokio::task::yield_now()`) every `WAL_REINJECT_CHUNK_SIZE`
   (8,192) frames so the live WS read loop and tick processor keep getting
   scheduled — a 1M+ replay cannot monopolize the runtime.
3. Bounds each send with `WAL_REINJECT_SEND_TIMEOUT_SECS` (30s). Only a
   truly dead (channel closed) or wedged (zero progress for 30s) consumer
   aborts the run — and then the remaining frames are counted, typed-paged
   (WS-REINJECT-01), and left staged in the WAL for next boot.
4. A fully delivered replay returns `clean = true`, which finally lets
   `confirm_replayed()` archive the WAL segments — **breaking the
   re-replay-grows-forever loop**.

**Ordering invariant (C3 adversarial-review CRITICAL, 2026-07-03):** at BOTH
call sites the frame-channel consumer (`run_tick_processor`) MUST be spawned
BEFORE the `reinject_wal_frames(...).await`, and the reinject await MUST
complete BEFORE the WS connections spawn. Consumer-first is load-bearing:
without a live consumer, any replay larger than `FRAME_CHANNEL_CAPACITY`
(131,072) fills the channel with nobody draining, the next send stalls into
the 30s timeout, the run aborts NOT-clean, and the WAL never archives — the
storm loop persists (the backpressure fix alone only helped the ≤capacity
case that was never broken). Reinject-before-connections preserves FIFO:
replayed frames enter the single sequential sender loop ahead of any fresh
live frame. Ratcheted (build-failing source-scan) by
`wal_reinject::tests::ratchet_tick_processor_spawns_before_reinject_await`.

**Boot-latency honest envelope:** the re-injection drains inline before
`notify_systemd_ready`, so boot wall-clock scales LINEARLY with WAL backlog
size (~the consumer's drain rate); a pathologically large WAL delays
readiness — bounded per-send by the 30s stall timeout, unbounded in total BY
DESIGN (the zero-drop trade-off). systemd tolerates this
(`TimeoutStartSec=infinity` per PR #1275, `deploy/systemd/tickvault.service`),
and operators tailing logs see a `WAL re-injection progress` `info!` line
every `WAL_REINJECT_PROGRESS_LOG_CHUNKS` (16) chunks ≈ every ~131K frames.

**The dropped frames were never durably lost:** the WAL floor
(`ws_frame_spill.rs`) keeps segments in `replaying/` until a CLEAN replay
confirms them, so every "dropped" frame of the incident remained on disk and
was re-replayed. The storm was a growth/CPU/duplicate-work problem plus a
false candle-derivation gap for the affected window — not durable tick loss.

---

## §1. WS-REINJECT-01 — boot WAL re-injection aborted

**Severity:** High. **Auto-triage safe:** Yes (the abort self-heals: the
staged WAL segments re-replay on the next boot; a dead consumer usually means
the process is restarting anyway — but the operator must see it).

**Trigger:** `reinject_wal_frames` hit one of two abort conditions
(`ErrorCode::WsReinject01Aborted`, `reason` label on the payload):

- `reason="channel_closed"` — `send().await` returned `SendError`: the tick
  processor's `Receiver` was dropped (consumer task died). Cross-check
  WS-GAP-07.
- `reason="send_timeout"` — a single send made zero progress for
  `WAL_REINJECT_SEND_TIMEOUT_SECS` (30s): the consumer is alive but wedged
  (blocked/stalled tick processor).

On abort the injector STOPS immediately: `injected` frames were delivered,
`aborted_remaining` were not. The re-injection is marked NOT-clean, so the
boot skips `confirm_replayed()` and the staged segments stay in `replaying/`
for re-replay next boot — fail-closed, no silent loss.

**Metrics** (renamed 2026-07-03 for `tv_ws_frame_wal_*` family consistency —
the short-lived `tv_ws_wal_reinject_*` names never shipped to prod):
- `tv_ws_frame_wal_reinject_aborted_total{reason}` — one increment per abort.
- `tv_ws_frame_wal_reinjected_dropped_total{ws_type="live_feed"}` — continues
  to count undelivered frames (semantic continuity with the pre-fix counter;
  now bounded to genuine consumer-dead/wedged aborts).
- `tv_ws_frame_wal_reinject_chunks_total` — one increment per delivered
  8,192-frame chunk (progress signal for a large replay; every 16th chunk
  also emits the `WAL re-injection progress` `info!` line).
- `tv_ws_frame_wal_reinjected_total{ws_type="live_feed"}` — delivered frames
  (pre-existing, still incremented at the call sites).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `WS-REINJECT-01`; the payload
   carries `reason`, `injected`, `aborted_remaining`.
2. `reason="channel_closed"` → the tick processor died during boot; look for
   a panic backtrace / WS-GAP-07 in `data/logs/errors.jsonl.*` immediately
   before. Restart the app — boot re-creates channel + consumer and the WAL
   re-replays everything.
3. `reason="send_timeout"` → the consumer is wedged; cross-check QuestDB
   health (BOOT-01/BOOT-02) and host CPU/memory (PROC-01, RESOURCE-02). The
   frames are safe on disk; fix the wedge, restart.
4. Verify the loop is actually broken: after a healthy boot, expect the
   `STAGE-C.2b … re-injection complete` `info!` and
   `tv_wal_replay_confirmed_segments_total` rising — `replaying/` should
   empty out instead of growing.

**Honest envelope:** the injector guarantees bounded zero loss inside its
envelope — while the channel is open it NEVER drops (backpressure), and on a
dead/wedged consumer the undelivered remainder stays durably staged in the
WAL `replaying/` directory and re-replays next boot. It does NOT claim the
consumer can never die, and it deliberately does NOT archive a partially
delivered replay (that would be silent loss). Duplicate delivery across
boots is absorbed by the QuestDB DEDUP keys (STORAGE-GAP-01).

**Source:**
- `crates/app/src/wal_reinject.rs::reinject_wal_frames` (the abort arm)
- `crates/app/src/main.rs` — STAGE-C.2b fast-boot + slow-boot call sites
- `crates/common/src/error_code.rs::ErrorCode::WsReinject01Aborted`
- Ratchet: `wal_reinject::tests::ratchet_main_rs_uses_bounded_reinject_helper`
  (main.rs must call `reinject_wal_frames(` at both sites and must not
  contain a raw `sender.try_send(frame)` loop)
- Ratchet: `wal_reinject::tests::ratchet_tick_processor_spawns_before_reinject_await`
  (per boot path, the `run_tick_processor(` spawn must precede the
  `reinject_wal_frames(` await in main.rs source order)

**Pre-existing envelope gap (flagged, NOT introduced or changed by this
fix):** `confirm_replayed()` archives on frames-IN-CHANNEL, not
frames-PERSISTED — a crash after the archive but before the consumer drains
+ persists the in-channel frames can lose those frames from the replayable
WAL floor (they would exist only in whatever the persistence chain absorbed
before the crash). This window is identical before and after the C3 change;
tracked as a follow-up (confirm-on-persist would need a
consumer-side acknowledgement watermark).

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/app/src/wal_reinject.rs`
- `crates/app/src/main.rs` (the STAGE-C.2b re-injection blocks)
- `crates/common/src/error_code.rs` (any `WsReinject01*` variant)
- `crates/common/src/constants.rs` (`WAL_REINJECT_CHUNK_SIZE` /
  `WAL_REINJECT_SEND_TIMEOUT_SECS`)
- Any file containing `WS-REINJECT-01`, `WsReinject01`, `reinject_wal_frames`,
  `tv_ws_frame_wal_reinject_aborted_total`, or
  `tv_ws_frame_wal_reinject_chunks_total`
