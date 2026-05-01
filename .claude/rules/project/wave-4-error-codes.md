# Wave 4 Error Codes (RESERVED — runbook stubs)

> **Authority:** This file is the runbook target for the new ErrorCode
> variants reserved for Wave 4 hardening (Items 1–10 + 14). Each entry
> below is a STUB. The sub-PR that ships the variant MUST flesh out
> Trigger / Triage / Auto-triage-safe / Source / Ratchet-tests before
> merging. Cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires that
> every variant in `ErrorCode` is mentioned in at least one rule file
> under `.claude/rules/`, so this stub file unblocks sub-PR landings.

## HIST-04 — DH-904 retry budget exhausted (Wave-4-B commit 1)

**Reserved.** Severity::High. Auto-triage: NO.

Triage stub: after 10 consecutive DH-904 retries on a single
`(timeframe, security_id, trading_date)` tuple, mark the slot failed,
emit Telegram, continue with the next tuple. Operator inspects
`tv_historical_dh904_retries_total{tf,security_id}` to identify
chronic offenders.

**Source (planned):** `crates/core/src/historical/candle_fetcher.rs`

## SELFTEST-03 — pre-open self-test failed (Wave-4-C commit 1)

**Reserved.** Severity::High (Degraded) / Critical (Critical).

Triage stub: at 09:00:30 IST the scheduler verifies (a) IDX_I feed
has tick within 30s, (b) NSE_EQ pre-open buffer non-empty per F&O
stock, (c) depth-20 connections in `Connected` state. Failures
follow the SELFTEST-02 (market-open) triage pattern in
`wave-3-c-error-codes.md`.

**Source (planned):** `crates/core/src/instrument/preopen_self_test.rs`

## DEPTH-DYN-01 — top-150 selector returned empty set (Wave-4-D, Phase 7 SHIPPED 2026-04-28)

**Status (2026-04-28):** PROMOTED FROM RESERVED — defined as
`ErrorCode::Depth20Dyn01TopSetEmpty` with `code_str() == "DEPTH-DYN-01"`.
Severity::High. NOT auto-triage-safe.

**Trigger:** every 60s the dynamic depth-20 selector queries
`option_movers` for the top 150 contracts (filtered `change_pct > 0`,
sorted `volume DESC`). If returned set has fewer than 50 contracts
(per-slot capacity), `check_set_emptiness` flags as either
`zero_results` or `below_slot_capacity`. Runner emits
`NotificationEvent::Depth20DynamicTopSetEmpty { returned_count, reason }`
with edge-triggered semantics (rising-edge fire only).

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select count(*) from option_movers where ts > now() - 5m"`
   — empty / very low count means upstream `OptionMoversWriter` is
   unhealthy; follow MOVERS-02 runbook.
2. Outside market hours this is expected. The boot-wired runner uses
   `is_within_market_hours_ist()` to suppress emission outside
   `[09:00, 15:30] IST`.
3. Inside market hours with healthy rows: universe-wide bear day where
   < 50 contracts are positive movers. Operator confirms; do NOT relax
   the `change_pct > 0` filter without Parthiban approval (Option B
   is locked).

**Source:** `crates/core/src/instrument/depth_20_dynamic_subscriber.rs`,
`crates/common/src/error_code.rs::Depth20Dyn01TopSetEmpty`,
`.claude/triage/error-rules.yaml::depth-dyn-01-top-set-empty-escalate`.

## DEPTH-DYN-02 — Swap20 command channel broken (Wave-4-D, Phase 7 SHIPPED 2026-04-28)

**Status (2026-04-28):** PROMOTED FROM RESERVED — defined as
`ErrorCode::Depth20Dyn02SwapChannelBroken` with `code_str() == "DEPTH-DYN-02"`.
Severity::Critical. NOT auto-triage-safe.

**Trigger:** selector emits `DepthCommand::Swap20` over one of the 3
dynamic-slot Senders (slots 3/4/5); send returns `SendError` because
receiver task exited. Pool supervisor (WS-GAP-05) should respawn
within 5s.

**Triage:**
1. Check `tv_ws_pool_respawn_total` — if incremented within last 5s,
   supervisor is working; next selector cycle should succeed.
2. If counter NOT incrementing: supervisor itself broken. Check
   `data/logs/errors.jsonl.*` for panic backtraces immediately
   preceding the DEPTH-DYN-02 emission.
3. If supervisor + respawn both fail repeatedly: restart the app.

**Source:** `crates/common/src/error_code.rs::Depth20Dyn02SwapChannelBroken`,
`crates/core/src/notification/events.rs::Depth20DynamicSwapChannelBroken`,
`.claude/triage/error-rules.yaml::depth-dyn-02-swap-channel-broken-escalate`.

## PROC-01 — OOM kill detected (Wave-4-E1)

**Reserved.** Severity::Critical.

Triage stub: scrape of `/sys/fs/cgroup/.../memory.events` shows an
`oom_kill` increment vs the boot-time baseline. The killed process
may not be tickvault itself (could be a sidecar) — check
`docker ps -a` for restart count.

**Source (planned):** `crates/app/src/oom_monitor.rs`

## PROC-02 — container restart loop (Wave-4-E1)

**Reserved.** Severity::Critical. Triage: any Docker container with
`RestartCount > 5` in the last hour. Block boot if tickvault itself
is the offender.

## NET-01 — IP changed mid-session (Wave-4-E1)

**Reserved.** Severity::Critical (mid-market) / High (off-hours).

Triage stub: `crates/core/src/network/ip_monitor.rs` polls the AWS
metadata service every 60s. A change vs the boot-time baseline
triggers Dhan order rejection (static IP enforcement effective
2026-04-01). Operator runs `aws ec2 describe-addresses` to verify
EIP still associated.

## NET-02 — DNS resolution failure cascade (Wave-4-E1)

**Reserved.** Severity::High. Triage: 3 consecutive DNS failures
within 60s targeting any of {api.dhan.co, auth.dhan.co,
api-feed.dhan.co, full-depth-api.dhan.co, depth-api-feed.dhan.co}.

## STORAGE-GAP-05 — disk-full pre-flight failed (Wave-4-E2)

**Reserved.** Severity::Critical. Already partially shipped in
PR #406 item 9 (spill pre-flight). Wave-4-E2 extends to historical
fetch + audit table writes.

## AUTH-GAP-04 — TOTP secret rotated externally (Wave-4-E2)

**Reserved.** Severity::Critical. Triage: token generation fails
with `INVALID_TOTP` on first attempt despite TOTP secret being
unchanged in our config. The secret was rotated externally (e.g.
operator regenerated via dhan.co web UI without updating SSM).

## DH-911 — Dhan API silent black-hole (Wave-4-E2)

**Reserved.** Severity::High. Triage: subscribe accepted (no error
response), but no packets arrive within 60s of subscription. Common
cause: far-OTM contract per `depth-subscription.md` rule 2 server-side
filtering.

## TIME-01 — trade attempted on declared holiday (Wave-4-E3)

**Reserved.** Severity::Critical. Defensive guard. Should never fire
in normal operation; if it does, the trading calendar is wrong or
operator manually disabled the holiday gate.

## RESOURCE-01 — file-descriptor count above threshold (Wave-4-E3)

**Reserved.** Severity::High. Threshold: > 80% of `ulimit -n`.

## RESOURCE-02 — resident memory bytes above threshold (Wave-4-E3)

**Reserved.** Severity::High. Threshold: > 80% of cgroup limit.

## RESOURCE-03 — spill file size above threshold (Wave-4-E3)

**Reserved.** Severity::High. Threshold: > 50% of `data/spill/`
free space.

## OPER-01 — Dhan client-id changed in config (Wave-4-E3)

**Reserved.** Severity::Critical. Defensive guard. Boot-time check
that `config/base.toml::dhan.client_id` matches the SSM-stored
canonical value.

## RESILIENCE-01 — dual-instance detected (Wave-4-E3)

**Reserved.** Severity::Critical. Two tickvault instances running
against the same Dhan client-id will fight over the static-IP
enforcement and fragment the WebSocket connection budget.

Triage stub: at boot, write `(host_id, boot_ts)` to QuestDB
`live_instance_lock` table with DEDUP UPSERT KEYS(client_id). If the
existing row's `boot_ts` is < 60s old, refuse to start.

## RESILIENCE-02 — mid-rebalance crash recovered (Wave-4-E3)

**Reserved.** Severity::High. Replay-on-boot from
`subscription_audit` table catches an unfinished `Swap20`/`Swap200`
sequence and re-issues it.

## CASCADE-01 — triple-failure recovery test (Wave-4-E3)

**Reserved.** Severity::Critical. Synthetic test: WebSocket drop +
QuestDB pause + token expiry within 60s window. Exercises every
recovery primitive simultaneously.

## IDEMP-01 — order placement idempotency breach (Wave-4-E3)

**Reserved.** Severity::Critical. Two distinct orderIds returned for
the same UUID v4 idempotency key.

## IDEMP-02 — instrument-master persist idempotency breach (Wave-4-E3)

**Reserved.** Severity::High. Same `(security_id, segment, expiry)`
written twice with different lot_size.

## IDEMP-03 — Phase 2 dispatch idempotency breach (Wave-4-E3)

**Reserved.** Severity::Critical. Phase 2 fired twice on the same
`trading_date_ist`.

## IDEMP-04 — depth rebalance idempotency breach (Wave-4-E3)

**Reserved.** Severity::High. Same `(underlying, levels, ts)`
swap-event published twice.

## IDEMP-05 — historical fetch state idempotency breach (Wave-4-E3)

**Reserved.** Severity::High. Same `(security_id, segment, timeframe)`
last_success_date moved backwards.
