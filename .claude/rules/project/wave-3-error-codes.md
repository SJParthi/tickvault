# Wave 3 Error Codes

> **Authority:** This file is the runbook target for the new ErrorCode
> variants added in the Wave 3 hardening implementation (Item 11 narrow —
> Telegram bucket-coalescer + dispatcher hardening).
>
> Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `crates/common/src/error_code.rs::ErrorCode` is
> mentioned in at least one rule file under `.claude/rules/`.

## TELEGRAM-01 — a Telegram event was dropped

**Trigger:** the Telegram dispatcher dropped an event without delivery.
Sources of drops, in order of likelihood:

1. **Coalescer overflow** — within a single 60s window the bucket sample
   ArrayVec hit its 10-entry cap. The summary message still fires on the
   next drain with an accurate `count`, so the operator is NOT blind, but
   individual sample payloads beyond the 10th are NOT preserved
   verbatim. Counter label: `reason="coalesced_sample_capped"`.
2. **Send-after-retry failure** — `send_telegram_chunk_with_retry`
   exhausted its retry budget. The downstream `error!` log still fires;
   Loki routes it to Telegram via Alertmanager (independent path), so
   the operator MAY still see it, but the dispatcher itself dropped.
   Counter label: `reason="send_failed"`.
3. **Disabled mode** — service initialized in NoOp mode (SSM credentials
   unavailable at boot). Counter label: `reason="noop_mode"`.

**Severity:** Medium. Drops are visible quality degradation but do not
breach the zero-tick-loss or order-management envelopes. The
`tv-telegram-drops` Prometheus alert fires when
`rate(tv_telegram_dropped_total[1m]) > 0.1` for ≥ 60s.

**Triage:**
1. Inspect `tv_telegram_dropped_total` in the CloudWatch metrics console
   (scraped from the app's `/metrics` exporter) — inspect the `reason`
   label distribution.
2. If `reason="send_failed"` dominates: check Telegram bot API health
   (`curl -fsS https://api.telegram.org`), verify the bot token is still
   valid in AWS SSM, check egress firewall.
3. If `reason="coalesced_sample_capped"` dominates: a single event topic
   is firing > 10 times per 60s window. Investigate the underlying noisy
   subsystem; the count in the summary message tells you the magnitude.
4. If `reason="noop_mode"`: SSM unavailable at boot. `make doctor`
   should already be RED; follow that runbook.

**Source:** `crates/core/src/notification/coalescer.rs::record_drop`,
`crates/core/src/notification/service.rs::dispatch_immediate`.

## TELEGRAM-02 — coalescer state inconsistency (informational)

**Trigger:** the coalescer detected an inconsistent bucket state during
a drain — typically a `count > 0` with an empty `samples` Vec, which can
only happen if a sample push raced with the drain in a way the Mutex
didn't fully serialize. Self-recovers on the next window; the affected
window's summary still fires (the count is authoritative), only the
sample list may be empty for that one window.

**Severity:** Low. Informational — no operator action required. The
`error!` log carries `code = ErrorCode::Telegram02CoalescerStateInconsistency.code_str()`
so the tag-guard meta-test can pin it; routing to Telegram is via Loki
(no per-event escalation).

**Triage:**
1. Inspect `data/logs/errors.jsonl.*` for the JSON event; the `bucket`
   field tells you which `(topic, severity)` pair was affected.
2. If this fires more than once in a 24h window, file an issue — the
   coalescer's Mutex-based serialization should make this provably
   impossible; recurrence indicates a logic bug.

**Source:** `crates/core/src/notification/coalescer.rs::drain`.

## TELEGRAM-03 — episode live-edit machinery degraded (2026-07-07 UX overhaul)

**Trigger:** the one-incident-one-bubble episode machinery
(`Telegram03EpisodeDegraded`, code_str `TELEGRAM-03`) hit a degraded path.
Five reasons, carried on the `reason` field of the `error!` line:

1. `store_write_failed` — the advisory episode snapshot file
   (`data/notify/episodes.json`) could not be written (disk full,
   unwritable directory). In-memory episode state keeps working; only the
   cross-restart bubble linkage degrades (a restart mid-outage opens ONE
   fresh duplicate bubble instead of resuming the old one).
2. `rehydrate_corrupt` — boot-time rehydrate found corrupt JSON. The
   registry starts empty, boot proceeds (fail-open, never gates boot or
   any notify()).
3. `edit_fallback_storm` — the edit-message fallback ladder is firing
   repeatedly (see `tv_telegram_edit_fallback_total{reason}`): Telegram
   keeps rejecting edits, so fresh bubbles are being sent instead
   (duplicate-over-drop — noisier, never silent).
4. `edit_transient_deferred` (2026-07-07 hostile-review fix) — a bubble
   edit exhausted the transient retry ladder BELOW the fallback threshold
   on a sub-High episode. Nothing was delivered for this event yet — the
   next event or the drain ticker re-drives the ladder; the emission +
   `tv_telegram_edit_fallback_total{reason="transient_deferred"}` keep it
   loud (never a silent terminal path). High/Critical episodes never take
   this arm — they fall back to a FRESH send on the FIRST exhausted
   transient (a structurally-final event, e.g. the once-per-outage
   order-update page, may never re-drive the ladder).
5. `stale_close_edit_failed` (2026-07-07 hostile-review fix) — the
   neutral close edit for a stale-expired Down bubble (the restart edge:
   a rehydrated bubble whose recovery event never arrives is expired
   after 30 event-less minutes) failed. Cosmetic only — the registry
   already dropped the episode, so any new problem opens a fresh alert;
   the old bubble may keep showing DOWN.

**Severity:** Low. Delivery is NEVER at risk from this code — every
transport failure still terminates at the existing TELEGRAM-01
error!+counter loudness. TELEGRAM-03 signals only that the one-bubble UX
is degraded.

**Companion counters (episode machinery, static labels only):**
`tv_telegram_episode_events_total{action="open"|"escalate"|"edit"|"edit_throttled"|"close"|"expired"|"reopen"|"legacy_passthrough"}`
and `tv_telegram_edit_fallback_total{reason="not_found"|"transient_exhausted"|"transient_deferred"|"stale_close_edit_failed"}`.
`escalate` = a Low-peak episode crossed into High/Critical and re-paged
FRESH (push + SMS — the pre-open Low storm can never swallow an in-market
HIGH outage); `expired` = a stale event-less Down bubble was neutrally
closed (restart edge); `legacy_passthrough` = a recovery for an untracked
episode was delivered via the legacy immediate lane instead of dropped.
An edit-failure-triggered fallback replaces the bubble message id and
increments the fallback counter; if the fallback send itself fails, the
EXISTING `tv_telegram_dropped_total{reason="send_failed"}` + TELEGRAM-01
`error!` fire exactly as before (the never-drop ladder's terminal rung).

**Triage:**
1. `reason="store_write_failed"` → `df -h data/` + `ls -la data/notify/`
   — disk full / permissions. In-memory episodes still work; fix the disk
   at leisure.
2. `reason="rehydrate_corrupt"` → delete `data/notify/episodes.json`; the
   file is advisory and self-recreates. One duplicate bubble per live
   outage is the worst case.
3. `reason="edit_fallback_storm"` → check
   `tv_telegram_edit_fallback_total` label split: `not_found` means the
   bubble message was deleted or aged out (>48h) — benign; sustained
   `transient_exhausted` means Telegram's API is rate-limiting or flaky —
   cross-check TELEGRAM-01 and the `tv-telegram-drops` alert.
4. Config rollback (no code change): `[notification] episode_mode = false`
   restores byte-identical legacy per-event dispatch.

**Auto-triage safe:** YES (Severity::Low; the degrade already self-limited
— never an operator-action pager).

**Source:** `crates/core/src/notification/episode.rs` (pure core:
`next_episode_action` FSM, `EpisodeRegistry`, `episode_snapshot`
codec), `crates/core/src/notification/service.rs` (transport shell:
`dispatch_episode_event`, `edit_telegram_message_with_retry`,
`classify_edit_body`, snapshot writer task),
`crates/common/src/error_code.rs::Telegram03EpisodeDegraded`.
