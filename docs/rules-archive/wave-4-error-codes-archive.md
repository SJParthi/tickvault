# Archived sections — wave-4-error-codes.md

> Sections excised verbatim from `.claude/rules/project/wave-4-error-codes.md`
> on 2026-07-20 (context-size incident). Live file keeps a pointer per section.


<!-- ==== DEPTH-DYN-01 + DEPTH-DYN-02 (RETIRED 2026-07-06 — movers runtime deleted 2026-05-19; ErrorCode variants deleted; historical audit) ==== -->

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



<!-- ==== AUTH-GAP-06 body (module deleted 2026-07-14; variant retained until the C4 sweep; historical audit) ==== -->

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

