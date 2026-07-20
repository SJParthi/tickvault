---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/storage/src/ws_frame_spill.rs"
---

# WAL Frame-Spill Writer — Error Codes (WS-SPILL-01 / WS-SPILL-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file.
> **Companion code:** `crates/storage/src/ws_frame_spill.rs`,
> `crates/common/src/error_code.rs::ErrorCode::{WsSpill01WriterRespawn, WsSpill02FrameDropped}`.
> **Companion rules:** `live-feed-purity.md` (the `ticks` table has ONE live source),
> `observability-architecture.md` (zero-touch chain), `wave-2-error-codes.md`
> (WS-GAP-05 pool supervisor + DISK-WATCHER-01 — the two patterns this mirrors).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to
> mention every `WsSpill*` variant.

---

## §0. Why these codes exist (the durable-floor guarantee)

Every Dhan WebSocket frame is captured to a disk-durable Write-Ahead-Log **before** any
in-process broadcast (`ws_frame_spill.rs` append precedes the `frame_sender.try_send` fan-out).
That WAL is the lossless, append-ordered record replayed on boot — it is the single guarantor that
a frame survives a crash, an OOM, a QuestDB outage, or a dead tick consumer.

The hot-path `append()` hands each frame to a bounded crossbeam channel; a dedicated background
**writer thread** drains the channel and fsyncs records to WAL segment files. A reliability audit
(2026-06-09) found the one residual silent-loss vector: if that writer thread **died** (panicked, or
returned on a transient disk I/O error), then (a) every later `append()` hit the channel's
`Disconnected` arm and dropped the frame **silently** — no log, no counter — and (b) nothing
respawned the thread, so the durable floor was permanently gone with near-zero signal.

These two codes close that gap. The writer thread is now **supervised + resilient** (it survives
per-batch I/O errors and respawns on panic, mirroring WS-GAP-05 / DISK-WATCHER-01), and the
last-resort drop is now **loud**.

---

## §1. WS-SPILL-01 — WAL spill writer thread respawned

**Severity:** High. **Auto-triage safe:** Yes (the respawn already self-healed; preserves the
durable WAL floor).

**Trigger:** the writer thread either panicked, or `writer_loop` returned, and the supervisor
re-entered it with the **same** crossbeam receiver — so the channel never becomes `Disconnected`
and `append()` keeps returning `Spilled`. Each respawn increments
`tv_ws_frame_spill_writer_respawn_total` and logs `error!(code = "WS-SPILL-01", …)`. A per-batch
disk write/flush/segment-rotation error does NOT kill the thread — it logs, increments
`tv_ws_frame_spill_write_errors_total`, reopens a fresh segment, and keeps draining.

**Why High and not Low (unlike DISK-WATCHER-01):** the disk-health *watcher* dying only loses
*monitoring*; the WAL *writer* dying threatens the durable *capture* of every Dhan frame. A
flapping writer means the underlying disk is failing — page the operator.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the `WS-SPILL-01` payload (panic backtrace, or the I/O
   error string + spill `path`).
2. `df -h data/` + `ls -la data/instrument-cache/ data/spill/` — disk full / unwritable / mount
   issue is the dominant cause of writer death.
3. Watch `tv_ws_frame_spill_write_errors_total` and `tv_ws_frame_spill_writer_respawn_total` rates —
   a sustained non-zero rate means the disk is actively failing; restore disk health.
4. The thread is self-healing; no app restart is required unless the disk problem is permanent.

**Source:** `crates/storage/src/ws_frame_spill.rs` (the `catch_unwind` respawn wrapper + the
resilient `writer_loop`), `crates/common/src/error_code.rs::WsSpill01WriterRespawn`.

---

## §2. WS-SPILL-02 — durable frame dropped (writer dead at append instant)

**Severity:** Critical. **Auto-triage safe:** No (a durable frame was actually lost).

**Trigger:** the hot-path `append()` `try_send` returned `Disconnected` — i.e. the writer thread was
dead at that exact instant (the brief window before WS-SPILL-01 respawns it). The frame is dropped
from the WAL path; this arm now increments `drop_critical`, `tv_ws_frame_spill_drop_critical{reason="writer_dead"}`,
`tv_ticks_lost_total{source="spill_writer_dead"}`, and logs `error!(code = "WS-SPILL-02", …)`.
Before this fix the arm returned silently.

**Honest envelope:** with the WS-SPILL-01 respawn in place this is practically unreachable — the
respawn re-attaches the same channel before the next `append`, so `Disconnected` requires the writer
to be dead *and* not-yet-respawned in the same micro-window. It remains the last-resort signal so the
zero-tick-loss invariant is *asserted*, never *assumed*.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — every `WS-SPILL-02` is paired with a `WS-SPILL-01` respawn;
   follow the §1 triage to fix the underlying disk failure.
2. Any non-zero `tv_ticks_lost_total{source="spill_writer_dead"}` is a real durable-loss event for the
   named `ws_type` — the affected minutes are rebuildable from the lossless `ticks` table /
   15:31 IST 1m cross-verify only if those frames also reached the persist-side consumer; treat as a
   genuine incident.

**Source:** `crates/storage/src/ws_frame_spill.rs::WsFrameSpill::append` (the `TrySendError::Disconnected`
arm), `crates/common/src/error_code.rs::WsSpill02FrameDropped`.

---

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `WsSpill*` variant)
- `crates/storage/src/ws_frame_spill.rs`
- Any file containing `WS-SPILL-`, `WsSpill0`, `tv_ws_frame_spill_writer_respawn_total`, or
  `tv_ws_frame_spill_write_errors_total`
