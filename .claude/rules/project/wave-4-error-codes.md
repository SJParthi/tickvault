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

> **⚠ RETIRED 2026-07-06 (audit finding).** The movers runtime feeding this
> code was deleted 2026-05-19 (AWS-lifecycle PRs #2-#4 — movers pipeline +
> depth-20/200 feeds removed) and the `Depth20Dyn01TopSetEmpty` ErrorCode
> variant was deleted with it. Content below retained for historical audit.

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
   — empty / very low count means the upstream unified
   `MoversWriter` (movers_1s + 25 mat views) is unhealthy. Check
   `tv_movers_writer_dropped_total` and the `movers_pipeline` task
   logs. Phase 4b (2026-05-05) retired the legacy MOVERS-02 code +
   the `OptionMoversWriter` it referenced.
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

> **⚠ RETIRED 2026-07-06 (audit finding).** The movers runtime feeding this
> code was deleted 2026-05-19 (AWS-lifecycle PRs #2-#4) and the
> `Depth20Dyn02SwapChannelBroken` ErrorCode variant was deleted with it.
> Content below retained for historical audit.

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

## PROC-01 — OOM kill detected (Wave-4-E1 / BP-07)

**LIVE (2026-07-01).** Severity::Critical. Auto-triage: NO.
`ErrorCode::Proc01OomKillDetected` (`code_str() == "PROC-01"`).

**Trigger:** the supervised OOM monitor
(`crates/storage/src/oom_monitor.rs::spawn_supervised_oom_monitor`, wired at
boot in `crates/app/src/main.rs` next to the disk-health watcher) reads the
cgroup-v2 `memory.events` file at [`DEFAULT_CGROUP_V2_MEMORY_EVENTS_PATH`]
(`/sys/fs/cgroup/memory.events`) every 60s and compares the `oom_kill` counter
against a boot-time baseline (captured on the FIRST successful read, so a
pre-existing lifetime OOM count never fires a spurious page). A positive delta
means one or more processes in this cgroup — tickvault itself OR a sidecar —
were killed by the kernel OOM killer. The monitor emits
`error!(code = "PROC-01", …)` — pages via the `tv-<env>-errcode-proc-01`
log-filter alarm → SNS → Telegram since 2026-07-06 (the `error!` alone never
routed to Telegram post-CloudWatch-migration) — and increments
`tv_oom_kills_total` by the delta, then advances the baseline so the same
kills are not re-reported.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the `PROC-01` payload: `new_kills`,
   `total_kills`, `baseline`.
2. `mcp__tickvault-logs__docker_status` — is the host OOM-killed? Check any
   container/service restart count; the killed process may be a sidecar, not
   tickvault.
3. Cross-check host memory pressure — the r8g.large 16 GiB budget
   (`aws-budget.md` / `daily-universe-scope-expansion-2026-05-27.md` §7) has
   ~7.8 GB headroom, so a real OOM points at a leak or an unexpected working-set
   spike; correlate with `tv_subsystem_memory_estimated_bytes{component=…}` +
   the host `mem_used_high` alarm.
4. A repeating PROC-01 (OOM-loop) during market hours needs operator action —
   the box will keep dying; investigate the offending subsystem before it
   starves the feeds.

**Observability:** `tv_oom_kills_total` (new kills since baseline),
`tv_oom_monitor_probe_failed_total` (probe I/O/parse failures — a non-cgroup-v2
host reports these instead of kills, honestly signalling "no OOM source"),
`tv_oom_monitor_respawn_total{reason}` (supervisor respawn — flapping monitor).

**Honest envelope:** the monitor reports kills the kernel attributes to THIS
cgroup's `oom_kill` counter. On a non-cgroup-v2 host (cgroup-v1 / macOS dev box)
the probe fails softly — no page, no panic, and `tv_oom_monitor_probe_failed_total`
rises so the lack of a signal is itself visible. It never blocks boot or the
hot path.

**Source:** `crates/storage/src/oom_monitor.rs`
(`parse_oom_kill_count` / `classify_oom_delta` pure primitives +
`spawn_supervised_oom_monitor`), `crates/common/src/error_code.rs::Proc01OomKillDetected`.
Boot wiring: `crates/app/src/main.rs`. Wiring ratchet:
`crates/core/src/auth/secret_manager.rs::tests::test_oom_monitor_is_wired_into_main`.

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

## AUTH-GAP-04 — TOTP secret rotated externally (AUTH-P11, live 2026-07-01)

**Status (2026-07-01):** PROMOTED FROM RESERVED — defined as
`ErrorCode::AuthGap04TotpRotatedExternally` with `code_str() == "AUTH-GAP-04"`.
Severity::Critical. NOT auto-triage-safe.

**Trigger:** the boot-time token-generation retry loop
(`crates/core/src/auth/token_manager.rs`) rejected the TOTP code and
exhausted `TOTP_MAX_RETRIES` (each retry waits a fresh 30s window). A
genuinely wrong secret — the classic cause is the operator regenerating
the 2FA/TOTP seed via the dhan.co web UI without updating the SSM param —
never produces a valid code, so the loop terminates. That terminal branch
now fires a distinctly-typed `error!(code = "AUTH-GAP-04", …)` — pages via
the `tv-<env>-errcode-auth-gap-04` log-filter alarm → SNS → Telegram since
2026-07-06 (the `error!` alone never routed to Telegram
post-CloudWatch-migration); the site separately fires the generic
`AuthenticationFailed` NotificationEvent (not typed as AUTH-GAP-04) — pointing
the operator at the exact SSM parameter to reconcile.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the `AUTH-GAP-04` payload
   (`totp_retries`, the SSM param hint).
2. Verify `/tickvault/<env>/dhan/totp-secret` in SSM matches the seed
   currently registered on the dhan.co web UI. If it was rotated, update
   the SSM value + restart; if it was NOT changed, the seed drifted (clock
   skew on the TOTP window is ruled out by BOOT-03).
3. Auth is dead until the secret is reconciled — never auto-actioned.

**Source:** `crates/common/src/error_code.rs::AuthGap04TotpRotatedExternally`,
`crates/core/src/auth/token_manager.rs` (the TOTP-exhaustion terminal branch).

## AUTH-GAP-05 — sustained mid-session token-invalid: forced re-mint triggered (live 2026-07-06)

> **⚠ 2026-07-14 UPDATE (operator Dhan noise lock —
> `dhan-rest-only-noise-lock-2026-07-14.md`):** BOTH Telegram pages this
> section describes are REMOVED — the `TokenForcedRemintTriggered` (High)
> and `MidSessionProfileInvalidated` (Critical) NotificationEvent variants
> are DELETED. The SILENT self-heal machinery is RETAINED AND EXTENDED:
> the 900s `/v2/profile` probe, `decide_remint`, the forced re-mint, and
> every coded `error!` + `tv_token_forced_remint_total` counter stay;
> (GAP-04) the retry-once latch now RE-ARMS every 2nd failing 900s cycle
> (~30-min retry cadence) while an episode persists, still honoring the
> ~125s mint cooldown + the RESILIENCE-03 lock refusals; (GAP-02) the
> REST-only stack additionally runs a 900s `force_renewal_if_stale(4h)`
> sweep (`DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS`) as the
> renewal-loop-halt backstop. A TERMINAL re-mint failure now pages the
> family-(c) Critical directly (`AuthenticationFailed` emitted from the
> watchdog's terminal arm — `force_renewal`→`acquire_token` pages nothing
> on a non-RESILIENCE-03 permanent failure, verified 2026-07-14). Content
> below retained as historical audit; page references are stale.

**Status (2026-07-06):** LIVE — defined as
`ErrorCode::AuthGap05ForcedRemintTriggered` with `code_str() == "AUTH-GAP-05"`.
Severity::High. Auto-triage safe: YES (the forced re-mint IS the
self-remediation; the operator inspects, never manually re-mints first).

**Trigger:** the 900s market-hours-gated mid-session profile watchdog
(`crates/core/src/auth/mid_session_watchdog.rs`) observed
`CONSECUTIVE_INVALID_REMINT_THRESHOLD` (= 2) consecutive REAL `/v2/profile`
auth failures (~30 min of a Dhan-rejected token; transient network blips
neither escalate nor clear the counter). **Classification update (F12,
2026-07-08):** a SEND-LEG failure (the request never got a response) whose
error text matches NO known transient needle now classifies
`RestSurfaceDegraded` — it pages via the pre-existing rising-edge CRITICAL
but NEVER walks the counter toward the destructive mint (previously it fell
through to `RealAuthFail`, so two novel reqwest/proxy wordings ~30 min
apart could mint against a healthy token). Only status-proven 401/403,
dataPlan/segment invalidation, token expiry, and unknown NON-send-leg
shapes count as REAL. The pure `decide_remint` core then
fires exactly ONE forced re-mint per failing episode through the EXISTING
`TokenManager::force_renewal()` machinery (RenewToken-then-fallback-
generateAccessToken), honoring:

- **Lock-before-mint (RESILIENCE-03), fail-closed:** when
  `TokenManager::dual_instance_lock_held()` reads `Some(false)` the mint is
  REFUSED with ZERO external side effects — a peer owns the Dhan session and
  its active token is never destroyed. The refusal is checked BEFORE the
  cooldown so a cooldown hold can never mask the safety refusal. The
  `error!` on this arm is tagged `RESILIENCE-01`.
- **~125s Dhan mint cooldown** (`DHAN_TOKEN_GENERATION_COOLDOWN_SECS`) via
  the pure `cooldown_elapsed` input — structurally moot at the 900s cadence,
  hard-stopped anyway. **HONEST SCOPE (AG5-R2-1, 2026-07-06):** this
  cooldown gates ONLY the watchdog's own mints — `acquire_token` has NO
  mint-cooldown gate (the 125s cooldown in `token_manager.rs` lives solely
  in the boot-time `initialize` TOTP retry loop), so an uncoordinated
  concurrent caller (the ~23h renewal loop's failure retries, the
  AUTH-GAP-03 ws-wake force renewal, and — SEC-R4-1, 2026-07-06 — the
  lane-owned 4h token sweep's `force_renewal_if_stale` backstop in
  `start_dhan_lane`) can still issue a second
  `generateAccessToken` inside Dhan's ~125s window during the same
  dead-token episode. Bounded + self-correcting (Dhan rejects/rate-limits
  the second mint; the loops keep retrying). Moving the cooldown stamp into
  `TokenManager` so ALL `renew_with_fallback` callers share one gate is a
  flagged follow-up. The sweep itself is LANE-OWNED since SEC-R4-1
  (registered in `DhanLaneRunHandles`, aborted on teardown/Drop) — a
  detached copy previously survived every runtime Dhan disable, kept the
  dead lane's JWT alive via RenewToken, and fired a false RESILIENCE-03
  "peer owns the session" Critical page every 4h after a lane
  stop/restart.
- **Retry-once latch** (`remint_attempted_this_episode`) — at most one mint
  (and one HIGH `TokenForcedRemintTriggered` Telegram) per failing episode;
  a clean check re-arms.

**Escalation / NO mid-session HALT:** a failed or non-restoring re-mint is
covered by the pre-existing CRITICAL `MidSessionProfileInvalidated` page +
the 23h renewal-loop circuit breaker. The BOOT-path DH-901
rotate→retry-once→HALT law is untouched — a mid-session HALT would drop the
live WS feed.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `AUTH-GAP-05`; the payload
   carries `consecutive` (trigger) or `permanent` (failure classification).
2. `tv_token_forced_remint_total{outcome}` —
   `triggered`/`ok` = self-heal working; `failed` = the re-mint itself
   errored (check Dhan status + SSM TOTP secret, cross-check AUTH-GAP-04);
   `refused_lock_lost` = the PRE-mint gate refused — a peer holds the
   dual-instance lock (see RESILIENCE-03 in
   `dual-instance-lock-2026-07-04.md` — decide which host owns the
   session); `refused_lock_lost_inflight` (SEC-R2-2, 2026-07-06) = the
   in-flight RESILIENCE-03 tripwire INSIDE `force_renewal` refused (the
   lock was lost between the watchdog's gate and the mint) — same
   peer-ownership triage, distinct series so an in-flight refusal never
   conflates with the pre-gate one; classified prefix-anchored on our own
   refusal literal, never on server-controlled body text; `cooldown` = a
   sub-125s re-trigger was held.
3. Gauges `tv_token_valid` (0/1 AND-composed, live 15s poller in
   `crates/core/src/auth/token_health_gauge.rs`) and
   `tv_token_remaining_seconds` (now LIVE, no longer frozen at mint time)
   show whether the re-mint restored validity within one poll/cycle.

**Source:** `crates/common/src/error_code.rs::AuthGap05ForcedRemintTriggered`,
`crates/core/src/auth/mid_session_watchdog.rs` (`decide_remint` + the
Trigger/RefuseLockLost arms), `crates/core/src/auth/token_health_gauge.rs`
(the honest gauges), `crates/core/src/auth/token_manager.rs::dual_instance_lock_held`.

## AUTH-GAP-06 — fast-boot cached-token validation (live 2026-07-08)

> **⚠ MODULE DELETED 2026-07-14 (operator Dhan noise lock —
> `dhan-rest-only-noise-lock-2026-07-14.md`).** The module
> `crates/core/src/auth/fast_boot_validation.rs`, its sole call site (the
> Dhan-gated FAST crash-recovery arm in main.rs — dead with
> `dhan_enabled = false` and deleted wholesale by the Phase C-2 lane PR),
> and its wiring guard test are DELETED. The
> `ErrorCode::AuthGap06FastBootCachedTokenValidation` variant is RETAINED
> until the C4 variant sweep (this file keeps satisfying the cross-ref
> test). A stale cached token on a REST-only boot is covered by
> `dhan_rest_stack` Phase 2's full `TokenManager::initialize` (which owns
> cache/mint logic itself). Content below retained as historical audit.

**Status (2026-07-08):** LIVE — defined as
`ErrorCode::AuthGap06FastBootCachedTokenValidation` with
`code_str() == "AUTH-GAP-06"`. Severity::High. Auto-triage safe: YES (the
re-mint / degraded-proceed is the self-remediation; the operator inspects,
never manually re-mints first).

**The incident this closes (2026-07-07 — THIRD morning outage of this
class, 08:32–09:06 IST):** the FAST crash-recovery boot arm
(`crates/app/src/main.rs`) restarts during market hours from a CACHED Dhan
JWT and previously spawned the WebSocket pool with ZERO validation of that
token. Dhan enforces one active token at a time (`authentication.md` rule
5), so a token minted elsewhere overnight silently killed the cached one —
the pool spawned dead and stayed dead until the AUTH-GAP-05 mid-session
watchdog's ~30-minute self-heal window or manual intervention.

**Trigger:** the fast arm now validates the cached token with **ONE
`GET /v2/profile`** BEFORE the WS-GAP-08 cooldown wait and any WebSocket
spawn (`crates/core/src/auth/fast_boot_validation.rs::validate_cached_token_at_fast_boot`,
routed through the pure, unit-tested `classify_probe_result`):

- **2xx** → proceed (one `info!`, `outcome="proceed"`). No page.
- **Prefix-anchored HTTP 401/403** (the broker rejected OUR login — the
  incident shape) → `error!(code = "AUTH-GAP-06")` + forced re-mint
  through the EXISTING TokenManager machinery (`initialize_deferred`
  bounded by `TOKEN_INIT_TIMEOUT_SECS`, then `force_renewal` —
  RenewToken→generateAccessToken fallback with the RESILIENCE-03 in-flight
  tripwire preserved). The fresh token lands in the shared handle the WS
  pool reads; the deferred background arm REUSES this manager (no
  duplicate SSM fetch).
- **Anything else** (send-leg/network, REST-surface 400/429/5xx, parse,
  unknown) → ONE bounded retry (`FAST_BOOT_PROFILE_RETRY_DELAY_SECS`),
  then PROCEED with the cached token + `error!(code = "AUTH-GAP-06")` —
  fail-open; a REST-surface outage with a healthy WS (the 2026-06-10
  class) must neither block boot nor trigger a token-destroying mint
  (the F12 no-mint-on-ambiguity lesson; classification reuses the
  AUTH-GAP-05 prefix-anchored wrapper primitives — never a substring scan
  of server-controlled body text).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `AUTH-GAP-06`; the payload
   names the arm (rejected → re-mint / degraded-proceed / remint failed).
2. `tv_fast_boot_token_validation_total{outcome}` —
   `remint_triggered`/`remint_ok` = the self-heal worked (the WS spawned
   with a fresh token); `remint_failed`/`remint_unavailable` = the mint
   leg errored — cross-check AUTH-GAP-04 (TOTP secret), RESILIENCE-03
   (in-flight lock refusal), and Dhan status; `degraded` = the REST
   surface was unreachable at boot — cross-check REST-CANARY-01; the
   AUTH-GAP-05 watchdog remains the in-session backstop either way.
3. A `remint_triggered` on EVERY fast boot means something keeps killing
   the cached token overnight (peer host mint, manual web login, the
   Groww-style daily reset) — investigate the token lifecycle, not this
   validation.

**Honest envelope:** the fast arm still holds NO dual-instance lock — it
passes `None` for the lock flag exactly as before
(`dual-instance-lock-2026-07-04.md` §3 documented residual, UNCHANGED by
this feature; no lock acquisition was added, pending the operator's
halt-semantics decision for that arm). The validation is one bounded
probe (+ one bounded retry) per boot on the cold path; it never blocks
boot (every leg timeout-bounded, every failure falls through loudly to
the pre-existing fast-arm semantics) and never touches the tick hot path.

**Source:** `crates/common/src/error_code.rs::AuthGap06FastBootCachedTokenValidation`,
`crates/core/src/auth/fast_boot_validation.rs` (`classify_probe_result` /
`CachedTokenVerdict` / `validate_cached_token_at_fast_boot`), the main.rs
fast-arm call site. Ratchets:
`crates/app/tests/fast_boot_token_validation_wiring_guard.rs` — exactly
one validation call in the main.rs production region, ordered before the
cooldown wait + `create_websocket_pool`, plus a stub-guard that the helper
actually probes `/v2/profile` and reaches `force_renewal`.

## DH-911 — Dhan API silent black-hole (Wave-4-E2)

**Reserved.** Severity::High. Triage: subscribe accepted (no error
response), but no packets arrive within 60s of subscription. Common
cause: far-OTM contract per `depth-subscription.md` rule 2 server-side
filtering.

## TIME-01 — trade attempted on declared holiday (Wave-4-E3)

**Reserved.** Severity::Critical. Defensive guard. Should never fire
in normal operation; if it does, the trading calendar is wrong or
operator manually disabled the holiday gate.

## RESOURCE-01/02/03 — fd / RSS / spill-free early-warning (BP-08, live 2026-07-01)

**Status (2026-07-01):** PROMOTED FROM RESERVED — defined as
`ErrorCode::Resource01FdCountHigh` / `Resource02ResidentMemoryHigh` /
`Resource03SpillFreeLow` (`RESOURCE-01/02/03`). All Severity::High,
auto-triage-safe (they page + are inspected, never auto-fixed).

**Source:** the supervised `crates/app/src/resource_monitor.rs` task
(mirrors the `disk_health_watcher.rs` supervised-poll + respawn template;
`tv_resource_monitor_respawn_total` on a flapping monitor). Linux-only
(`/proc`, cgroup); a non-Linux host or an unreadable probe degrades to a
`ProbeFailed` skip with no false-OK.

### RESOURCE-01 — file-descriptor count above threshold

**Trigger:** `/proc/self/fd` entry count crossed the early-warning
threshold (default 80% of `LimitNOFILE`, read from `/proc/self/limits`).
A leaked WS / QuestDB socket can exhaust the fd table with zero signal
until `connect()` starts failing; this monitor pages BEFORE that.
`tv_open_fds` gauge. **Triage:** `ls /proc/<pid>/fd | wc -l`; inspect for
a socket leak (repeated same-peer entries) → cross-check WS reconnect
churn (WS-GAP-05).

### RESOURCE-02 — resident memory bytes above threshold

**Trigger:** process `VmRSS` (`/proc/self/status`) crossed the
early-warning threshold (default 80% of the cgroup memory limit,
`memory.max`). Distinct from the host-aggregate `mem_used_high` CloudWatch
alarm — this is the tickvault process itself approaching its cgroup
ceiling before the OOM killer (PROC-01) fires. `tv_process_rss_bytes`
gauge. cgroup limit = `max` (unlimited) → skipped (no denominator).
**Triage:** right-size the workload / investigate the leak; if it keeps
climbing, PROC-01 (OOM kill) is imminent.

### RESOURCE-03 — spill-directory free space below threshold

**Trigger:** spill-dir free space dropped below the early-warning
percent-of-total threshold (default 20% free). A process-level percent
view distinct from the host `disk_used_high` aggregate, so a fast-filling
spill dir is caught before the zero-loss chain is at risk. Reuses the
`disk_health_watcher::probe_disk_free_bytes` probe. `tv_spill_free_pct`
gauge. **Triage:** `df -h data/spill/`; restore the QuestDB drain
(cross-check DISK-WATCHER-01 / BOOT-01/02) or free disk.

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
