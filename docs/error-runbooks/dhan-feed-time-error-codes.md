# Dhan feed time — DHAN-FEED-TIME-01, DHAN-LTT-01, DHAN-LTT-02

> **Authority:** `websocket-connection-scope-lock.md` §"2026-11-20 — DHAN TRADE
> TIME SENT AS UTC", `dhan-rest-only-noise-lock-2026-07-14.md` §2.24,
> `daily-universe-scope-expansion-2026-05-27.md` Quote 63; superseded in part
> by scope lock §"2026-11-21 — DHAN LIVE LTT IS UTC EPOCH FROM RECEIPT
> 2026-11-20 00:00 IST" and noise lock §2.24-a.
> **Code:** `crates/storage/src/ltt_correction.rs` (table, record, detector,
> controller), `crates/core/src/parser/dispatcher.rs` (`apply_ltt_correction`),
> `crates/app/src/dhan_feed_stack/ltt_feed.rs` (drain seams, replay hold),
> `crates/app/src/dhan_feed_stack/feed_time_page.rs` (the page).

## What happened (2026-11-20)

Every Last Trade Time on the Dhan main feed arrived as plain UTC epoch
seconds, 19,800 s (5 h 30 min) early under the documented IST rule. No candle
was sealed from 08:32 to 14:52, and after 14:30 the shifted times were folded
as 09:15–10:48 candles. Raw WAL frames were complete. Dhan was asked on
2026-11-21 (`docs/dhan-support/2026-11-20-ltt-sent-as-utc.md`).

## ⚠ CHANGED 2026-11-21 — the correction is permanent and needs no record

Dhan support (ticket #6684672, 21 Nov 2026): the Live LTT format changed from
IST-based to UTC-based epoch before the open on 20 Nov, as part of an API
restructure, and UTC is permanent. Owner cards "Yes, always UTC (Recommended)"
and "Before Monday if checks pass (Recommended)". So:

- Every Ticker, Quote and Full trade time of a frame whose WAL receipt is at or
  after 2026-11-20 00:00 IST gets exactly +19,800 s, from the very first
  packet, the same live and in replay (`ltt_correction::corrected_ltt`, a pure
  function of raw time and receipt). Receipts before it are never changed.
- Guards kept: a plausible receipt, `0` and out-of-band values stay raw, and the
  result is never after receipt + 5 s. A refusal leaves the raw time and is
  counted on `tv_dhan_ltt_correction_refused_total{reason="receipt"|"band"|"cap"}`
  (local `/metrics` only, registered at 0 when the lane builds the guard). A
  frame with no WAL receipt stamp is never corrected and is counted on
  `tv_dhan_ltt_unstamped_total`. Both count DISPATCHES: a packet dispatched
  again by a WAL catch-up, the after-close depth pass or the offline replay is
  counted again.
- No detector arming, no record file, no daily notice, no replay hold. Leftover
  `data/state/ltt-correction/*.rec` files are ignored and kept.
- The detector now only watches for Dhan reverting to IST epoch and pages
  DHAN-FEED-TIME-01 `source="dhan_time_format_reverted"` (below).
- `[dhan_feed] ltt_utc_autocorrect` now governs only that guard: `off` = no
  detector, `shadow` = coded `warn!` only, `act` (default) = page and hold the
  day. It never
  switches the correction off; rolling the correction back is a code change
  behind a dated owner quote.

The section below describes the 2026-11-20 design and is kept as history.

## How the correction works (2026-11-20 design, superseded)

- The live drain votes on each fresh index tick and NSE stock spot print:
  "normal" (raw time near the receipt in IST) or "UTC" (raw + 19,800 s near
  it). 100 votes, 3 distinct instruments, at least 99% agreement arm it; the
  same evidence the other way closes it. At most 8 changes a day.
- A change is two-step: an INTENT line in `data/state/ltt-correction/<date>.rec`
  is fsynced first, then the drain applies it from one nanosecond past every
  receipt it already dispatched, then writes the COMMIT line. Replay applies
  only committed changes, by WAL receipt, so live and replay write the same rows.
- Every corrected time is exactly +19,800 s, never after receipt + 5 s, never on
  another IST day (except inside the previous day's session), never for `0` or
  an out-of-band value, never for a frame without a WAL receipt stamp. A
  segment the detector did not vote on (options) is corrected only for prints
  within 120 s of receipt.
- Switch: `[dhan_feed] ltt_utc_autocorrect = "act" | "shadow" | "off"`
  (default `act`; 2026-11-20 text: `off` was the full rollback. ⚠ Since
  2026-11-21 no value switches the correction; see above.)

## DHAN-FEED-TIME-01 — trade times still wrong (PAGES)

**Severity:** High. **Auto-triage:** never (operator verdict).

Causes `prices_outside_session` and `no_candles` fire at most once per cause
per IST day, inside 09:15–15:30 IST (or the published Muhurat window), from
the live drain only. The 2026-11-21 reason `dhan_time_format_reverted` has its
own window and latch (its row below; noise lock §2.24-a):

| cause | meaning |
|---|---|
| `prices_outside_session` | In two 30 s scans in a row, at least 20 live index ticks and at least half carried a time (after the correction) outside the session or more than 5 minutes from receipt. |
| `no_candles` | 20 scans in a row (10 minutes) with main-feed frames arriving and no candle sealed. |
| `dhan_time_format_reverted` (2026-11-21, noise lock §2.24-a) | `source` of this cause. The reversion guard read 100 fresh index or NSE stock prints (3 distinct instruments, at least 99% agreeing) whose raw trade time is IST epoch again, with a receipt on or after 2026-11-20 inside 09:00–15:40 IST (or the Muhurat window). At most once per segment per IST day per process (the latch is in RAM, so a restart can page again), live drain only. The day is auto-held too (`act` only): the 30 s feed-time scan hands it to the same writer thread within 30 s. |

For every reason the day is written to `data/state/held-days/<date>.hold`
(daily-universe Quote 63; for `dhan_time_format_reverted` within 30 s of the
page, `act` mode only), so the archiver, the Brutex export and the cross-check
catch-up skip it until a verified repair. If the hold write fails, a
`DHAN-LTT-02` `feed_time_hold_write_failed` line says so: write the file by
hand.

**Triage:**
0. (2026-11-21) `dhan_time_format_reverted`: Dhan is sending IST epoch again.
   Fresh prints are left raw by the cap (correct IST) and counted as `cap`;
   every print more than about 5.5 h behind its receipt, any day (yesterday's
   trade on an illiquid contract's first packet included), is shifted +5.5 h
   wrongly. The day is auto-held; check `data/state/held-days/<date>.hold`
   exists (write it by hand if a `feed_time_hold_write_failed` line says it
   could not). Tell Dhan, and decide a code change with the owner.
1. ⚠ CHANGED 2026-11-21: the correction no longer arms; it applies from the
   cutover by itself. Check `tv_dhan_ltt_correction_refused_total` (a high
   `cap` count means prints are already IST epoch or the box clock is more
   than 5 s behind) and `tv_dhan_ltt_unstamped_total`. *(2026-11-20 text:
   check whether the correction armed today: look for `DHAN-LTT-01`
   `source="ltt_correction_armed"` and the once-a-day Telegram notice. If it
   did not arm and times are 5 h 30 min early, check the switch is `act` and
   the record folder is writable (`DHAN-LTT-02`).)*
2. If the shift is something other than exactly 5 h 30 min, the correction
   cannot fix it (by design). Leave the day held; raw frames are kept.
3. `no_candles` with correct times: check the seal writer and QuestDB health
   (`make doctor`), and the AGGREGATOR codes.
4. Release the day only through the repair (`data/state/day-repair/<date>.done`,
   see `held_days.rs`), never by deleting the `.hold` file without a repair.

## DHAN-LTT-01 — correction armed or closed (log only)

> ⚠ DORMANT since 2026-11-21: nothing arms or closes the correction any more,
> so no `DHAN-LTT-01` line is written. Kept until the follow-up PR deletes the
> dormant code.

**Severity:** Low. A `warn!`: `source` is `ltt_correction_armed`,
`ltt_correction_closed`, `ltt_shadow_would_arm` or `ltt_shadow_would_close`.
Never pages (§2.24). The once-a-day notice is the operator signal.

## DHAN-LTT-02 — record or replay hold (log only)

> ⚠ CHANGED 2026-11-21: the record writer is not started and the boot step
> reads no record, so the `ltt_record_*`, `ltt_detector_frozen` and
> `ltt_replay_held` sources cannot fire. The `feed_time_*` rows (the page's
> own latch and hold files) still apply.

**Severity:** Medium. Never pages. `source`:

| source | meaning | action |
|---|---|---|
| `ltt_record_write_failed` | A record line could not be saved. An intent: the change is voided, never applied. A commit: the day reads "started, never confirmed" at the next start. | Check disk space and `data/state/ltt-correction/`. |
| `ltt_record_queue_refused`, `ltt_record_ack_lost`, `ltt_record_writer_spawn_failed` | The record thread did not take or acknowledge a line. | Same; the correction does not arm without a durable record. |
| `ltt_detector_frozen` | 8 changes in one day; the detector stops for the day. | The feed is flapping; check with Dhan. |
| `ltt_record_day_held` (boot) | A day's record has a change that was started and never confirmed, or cannot be read. That day's frames are held from replay. | Manual repair below. |
| `ltt_replay_held` | A WAL replay reached such a day and stopped (`stop_reason="ltt_day_held"`). Frames stay in the log. | Manual repair below. |
| `feed_time_latch_write_failed`, `feed_time_hold_write_failed`, `feed_time_persist_refused` | The page could not save its daily latch or the day's hold marker. | Write `data/state/held-days/<date>.hold` by hand before the evening archive. |

### Manual repair of a held record day

A started-but-unconfirmed change means the live run crashed between the two
steps; it may or may not have corrected that day's prices.

1. Stop the app (outside 09:00–15:45 IST).
2. Decide from the stored candles of that day whether the correction was live
   (times inside the session) or not (times 5 h 30 min early).
3. No repair tool exists yet (flagged follow-up). The fix is one record line
   for the dangling entry: a `V` (void) line for "never applied", or an `A`
   (armed) line with the effective receipt, each CRC-framed exactly as
   `ltt_correction::encode_line` writes it. Until a reviewed tool writes it, the
   day stays held and nothing is deleted.

## Honest limits

> ⚠ CHANGED 2026-11-21: the first two limits below are gone (the correction
> needs no votes and holds no day). Now: if Dhan reverts, every print more
> than about 5.5 h behind its receipt, any day, is shifted wrongly (fresh
> prints stay raw by the cap); the reversion page auto-holds the day within
> 30 s (`act`), so those rows stay out of the archive and the export until
> repaired; a restart can page the reversion a second time the same day (the
> latch is in memory; the second hold is a no-op); the offline live-fill replay tool reads no record and needs none.

- (2026-11-20) The detector needs live votes, so the first seconds after a mid-day flip are
  folded uncorrected (those minutes are what cause A pages on).
- A day held by an unconfirmed change keeps its WAL segments on disk until
  repaired.
- The offline live-fill replay tool does not read the record.
