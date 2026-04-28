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
1. `mcp__tickvault-logs__prometheus_query "tv_telegram_dropped_total"` —
   inspect the `reason` label distribution.
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
