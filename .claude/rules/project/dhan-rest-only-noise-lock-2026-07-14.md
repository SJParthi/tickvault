# Dhan REST-Only Noise Lock — Operator Lock 2026-07-14

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §D/§F >
> `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A.1 (the
> 2026-07-14 subsection recording the same quote) >
> `no-rest-except-live-feed-2026-06-27.md` §8 (the retained spot-1m +
> option-chain grant) > this file > defaults.
> **Scope:** PERMANENT. Every PR, every branch, every future Claude/Cowork
> session.
> **Operator-locked:** 2026-07-14 (verbatim quote below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase — expletives included)

**Quote (2026-07-14, relayed verbatim via the coordinator session):**

> "for Dhan except spot 1m and option chain nothing else should work… these
> fucking issues of mid profile and all other fucking issues of Dhan should be
> entirely removed… always make the telegram messages/notifications cleaner,
> always mention precisely which broker."

---

## §1. The rule (one line)

**The ONLY Dhan Telegram alerts that can ever fire are: (1) the DHAN spot-1m
pull failing/recovered, (2) the DHAN option-chain pull failing/recovered,
(3) the DHAN token-unobtainable Critical, and (4) the CloudWatch 4h
token-remaining early-warning — every other Dhan-era page, probe, watchdog
Telegram, and dead alarm is deleted or silenced; the token machinery
self-heals SILENTLY.**

---

## §2. The contract table (the final 4-item Dhan alert set)

| # | Allowed Dhan alert | Variant(s) / route | Fires when |
|---|---|---|---|
| 1 | Spot-1m pull failing / recovered | `Spot1mFetchDegraded` (High) / `Spot1mFetchRecovered` (Info) / `Spot1mSidNotServed` (High) / `Spot1mSidServedRecovered` (Info) | the per-minute spot leg's persist-gated 3-minute escalation edge (`rest-1m-pipeline-error-codes.md`) |
| 2 | Option-chain pull failing / recovered | `ChainFetchDegraded` (High) / `ChainFetchRecovered` (Info) / `ChainEntitlementAbsent`/`Confirmed` / `ChainExpirylistFailed` (High) | the chain leg's own edges (`rest-1m-pipeline-error-codes.md`) |
| 3 | Token could not be obtained | `AuthenticationFailed` / `TokenRenewalFailed` (both Critical; reworded 2026-07-14 to plain English naming DHAN + the consequence: "the Dhan spot-1m and option-chain pulls will stop until this is fixed") | mint/renewal is TERMINALLY dead — the mid-session watchdog pages **ONCE PER FAILING EPISODE** (H1a latch, 2026-07-14 fix round — never the pre-fix ~30-min repeat) on EITHER (a) a forced re-mint failing terminally OR (b) the H1b attempt cap: `REMINT_MAX_ATTEMPTS_PER_EPISODE` (= 3) re-mints all "succeeded" yet the profile stayed REAL-invalid (dead-dataPlan/segment class — the body names the N re-logins + that the spot-1m/chain pulls are blocked). The latch resets on a clean profile cycle. (Its terminal arm emits `AuthenticationFailed` directly, since `force_renewal` -> `acquire_token` pages nothing on a non-RESILIENCE-03 permanent failure; the Telegram body is redacted + truncated via the house sanitizer — M2.) |
| 4 | Token expires soon (4h early warning) | CloudWatch alarm `tv-<env>-token-remaining-low` on `tv_token_remaining_seconds` → SNS → Telegram Lambda | the renewal loop stopped renewing (the watchdog-of-the-renewal-loop). The Lambda's wording is ANOTHER session's scope. |

**Deleted or silenced 2026-07-14 (everything else Dhan):**

| Component | Disposition |
|---|---|
| Mid-session profile watchdog Telegram pages (`MidSessionProfileInvalidated` Critical + `TokenForcedRemintTriggered` High) | **Variants DELETED.** The 900s `/v2/profile` probe + the AUTH-GAP-05 forced re-mint machinery are KEPT and run SILENTLY (coded `error!` + counters only); a terminal re-mint failure routes to the family-(3) Critical. |
| AUTH-GAP-05 latch re-arm (GAP-04, 2026-07-14 backstop) | **ADDED, silent:** while a failing episode persists, `decide_remint` re-arms the retry-once latch every 2nd failing 900s cycle (~30 min retry cadence), still honoring the ~125s mint cooldown + the RESILIENCE-03 lock refusals — **BOUNDED (H1b, fix round) at `REMINT_MAX_ATTEMPTS_PER_EPISODE` (= 3) mints per episode**; the cap fires the once-per-episode family-(3) Critical when the profile is still invalid, closing the silent dead-dataPlan loop (~48 silent mints/day pre-fix). A persisting LOCK-LOST episode re-logs the RESILIENCE-01 refusal at the same ~30-min cadence (log-only, no mint, no Telegram from that arm). No routine Telegram from this path. |
| REST-stack stale-token sweep (GAP-02, 2026-07-14 backstop) | **ADDED, silent:** `dhan_rest_stack` Phase 3 runs `force_renewal_if_stale(14400)` every 900s (`DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS`) — the renewal-loop-halt backstop the lane's 4h sweep used to be. Not market-hours-gated. Terminal failure pages via family-(3). SUPERVISED (fix round: the house respawn pattern — a silent sweep death would re-open the audited gap; unwind-build self-heal only, release panics abort). Honest wording note (fix round): the ~23h renewal loop is NOT an independent retry — it HALTS PERMANENTLY after its circuit-breaker cycles; this sweep + the AUTH-GAP-05 watchdog are the retries. |
| Shared mint-cooldown gate (H3, 2026-07-14 fix round) | **ADDED, silent:** `TokenManager::renew_with_fallback` — the ONE shared re-mint entry (watchdog + GAP-02 sweep + renewal loop + `force_renewal*`) — SKIPS the `generateAccessToken` fallback with a coded warn + typed refusal (`mint-cooldown` prefix; never a page, never burns the episode latch) while a previous mint ATTEMPT is younger than the ~125s Dhan cooldown. Closes the AG5-R2-1 flagged residual the 900s sweep had tightened 16x. The boot-time `initialize` retry loop is deliberately UNGATED (calls `acquire_token` directly; owns its own >=130s floor — no boot deadlock; source-scan pinned). |
| Token-health gauge poller supervision + pre-#1522 residual (GAP-06 + M6, fix round) | The re-homed poller is SUPERVISED like the sweep. **ACCEPTED residual (M6):** on a hypothetical `dhan_enabled=true` boot BEFORE #1522 merges, the LANE path no longer spawns the poller (its main.rs spawn sites are deleted) and the stack does not run — so `tv_token_valid` would go unpublished for that boot shape. Accepted because prod is dhan-OFF (config + the Phase-A 409 refusal) and #1522 (which deletes the lane's fast arm) merges FIRST; this PR rebases after. |
| REST canary (`rest_canary_boot.rs`, REST-CANARY-01 probes 09:05/12:00/15:25 IST) | **Module + both spawn sites + the `rest-canary-01` CloudWatch filter/alarm DELETED.** The legs self-detect REST death in ~3-4 min via their own escalation edges — strictly better than 3 fixed slots. `ErrorCode::RestCanary01ProbeFailed` variant retained until C4. |
| No-tick watchdog (`no_tick_watchdog.rs`, `NoLiveTicksDuringMarketHours` Critical) | **Module + variant + both spawn sites DELETED.** Its heartbeat was fed ONLY by the retired Dhan tick pipeline; Groww stall detection is FEED-STALL-01 + the market-hours-liveness alarm. |
| Fast-boot cached-token validation (`fast_boot_validation.rs`, AUTH-GAP-06) | **Module + sole call site DELETED** (the Dhan-gated fast arm is dead with `dhan_enabled=false` and dies in #1522). `ErrorCode::AuthGap06…` variant retained until C4. |
| Token-health gauge poller (`token_health_gauge.rs`, `tv_token_valid` + live `tv_token_remaining_seconds`) | **RE-HOMED (GAP-06, 2026-07-14 — supersedes the same-day delete ruling):** the module is KEPT; the lane/fast-arm spawn sites in main.rs are DELETED; `dhan_rest_stack` Phase 3 spawns it, so the gauges stay alive on dhan-off boots even after a renewal-loop circuit-breaker halt (which kills the 30s in-loop gauge writer) — keeping alarm #4 sighted. |
| Order-update WS spawn (`dhan_rest_stack` Phase 5a) + its 2 alarms (`tv-<env>-order-update-ws-inactive`, `tv-<env>-order-update-reconnect-storm`) | **Spawn + alarms DELETED** per `websocket-connection-scope-lock.md` §A.1. The core module `order_update_connection.rs` is RETAINED DORMANT (unit tests stay) for the live-trading re-wire — re-spawn or module deletion needs a fresh dated quote in the scope-lock file first. |
| `observability-architecture.md` paging list | REST-CANARY-01 removed from the Filtered+alarmed set (dated note; the paging drift guard pins tf↔doc↔emit). |

### §2a. Order-execution family (cluster C, PR #1554 — a SEPARATE landed family, NOT a Dhan REST alert)

The §2 4-item set is the **Dhan REST-only surface** (spot-1m / option-chain /
token). Distinct from it, the **order-execution family** — the cluster-C
order-side observability that landed on `main` in **PR #1554** — dispatches its
OWN typed Telegram events from `crates/app/src/order_observability.rs`
(the order-side consumer's `OmsAlertBridge` / `RiskAlertBridge` sinks):
`NotificationEvent::OrderRejected`, `NotificationEvent::CircuitBreakerOpened`,
and `NotificationEvent::RiskHalt`. These fire on the OMS order path (order
rejects, circuit-breaker transitions, risk halts) in the paper/dry-run layer —
NOT on the Dhan REST data-pull surface — so they are **outside** the §2 count
and are NOT governed by the §3 "new Dhan-scoped REST Telegram page" REJECT.

**This subsection is a rebase-reconciliation note (2026-07-14):** it DOCUMENTS
the pre-existing, landed #1554 dispatch sites so the exit-order lockout guard
(`dhan_exit_order_lockout_guard::exit_layer_emits_no_telegram_dispatch`) — which
requires this file to carry an `order execution` family row once any order-path
`NotificationEvent` dispatch site exists — reconciles cleanly with `main`. It
introduces NO new emit. **The 🔷 DHAN exit-order layer itself stays
Telegram-free** (engine exit region + `exit_rules.rs` + `exit_execution.rs` are
sink-free; EXIT-ORDER-01 / EXIT-VERIFY-01 remain log-sink-only) — the guard's
part (a) still enforces that verbatim. Any FUTURE change that routes the exit
layer's own signals to Telegram remains a REJECT under §3 until an operator
dated quote lands here.

---

## §3. What a PR that violates this lock looks like (REJECT)

- Adds ANY new Dhan-scoped Telegram page outside the §2 4-item set without a
  fresh dated operator quote HERE first.
- Re-introduces the mid-session profile / forced-re-mint Telegram pages, the
  REST canary, the no-tick watchdog, the fast-boot validation call, or the
  order-update spawn (each needs a fresh dated quote; the order-update
  re-wire additionally needs the scope-lock §A.1 edit).
- Removes the SILENT self-heal machinery this lock deliberately KEEPS: the
  900s profile probe, the AUTH-GAP-05 forced re-mint + its GAP-04 latch
  re-arm, the GAP-02 REST-stack token sweep, the re-homed token-health gauge
  poller, or the `tv-<env>-token-remaining-low` alarm.
- Downgrades / removes the family-(3) Critical on a terminally-dead token
  (silent terminal failure = Rule-11 false-OK).
- Makes a Dhan-scoped Telegram body stop naming the broker (the operator's
  "always mention precisely which broker" — the 🔷 DHAN badge and/or the word
  Dhan in the body).

Any such PR MUST be rejected in review even if the operator approves verbally
— the operator must update this rule file FIRST with a dated quote.

---

## §4. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: a dead
> Dhan token is detected within minutes by the legs' own persist-gated
> escalation edges (SPOT1M-01 / CHAIN-02 → the family-(1)/(2) High pages) and
> self-heals SILENTLY via three retained mechanisms (the 900s profile probe's
> AUTH-GAP-05 forced re-mint with the GAP-04 ~30-min latch re-arm, the GAP-02
> 900s `force_renewal_if_stale(4h)` stack sweep, and the ~23h renewal loop);
> a TERMINALLY-unobtainable token pages ONE family-(3) Critical PER FAILING
> EPISODE naming Dhan + the consequence (H1a latch; re-armed only by a clean
> profile cycle), and a token whose re-mints "succeed" while the profile
> stays invalid (dead dataPlan/segment) pages the SAME once-per-episode
> Critical after `REMINT_MAX_ATTEMPTS_PER_EPISODE` (= 3) re-logins (H1b cap)
> instead of re-minting silently forever. NOT claimed: (a) same-day heal of a Dhan-side-KILLED but
> locally-fresh token AFTER market close — the profile probe is
> market-hours-gated and the 4h sweep only re-mints on <4h local headroom, so
> the post-close 15:33:30 spot sweep can still fail on such a token until the
> next boot's init re-mints (bounded to one post-close window; the in-session
> surface is covered); (b) detection latency below the legs' 3-minute edges —
> the deleted REST canary's 3 fixed probe slots were strictly slower, not
> faster, than the always-on edges; (c) any order-update capture — the socket
> is deliberately closed until live trading (dry_run=true, events were
> counted-then-discarded); (d) `tv_token_valid`/`tv_token_remaining_seconds`
> publication on a dhan-ON lane boot BEFORE #1522 merges — the lane's poller
> spawn sites are deleted and the stack does not run on that boot shape
> (M6 ACCEPTED residual: prod is dhan-off and #1522 merges first)."

---

## §5. Auto-driver / Insta-reel explanation

> Sir, the Dhan supplier now does exactly TWO jobs for the juice shop: once a
> minute he phones in the official price card (spot 1m) and the option-coupon
> sheet (option chain). So the phone should ring about Dhan for exactly FOUR
> reasons: the price card stopped coming, the coupon sheet stopped coming, the
> shop KEY could not be cut at all, or the key expires in 4 hours. Everything
> else — the old "checking his pulse three times a day", the "no fruit prices
> heard" siren from the removed live board, the confirmation line that kept
> ringing about a phone nobody uses — is ripped out. If the key jams, the boy
> quietly re-cuts it himself every half hour; you only hear about it if the
> key genuinely cannot be cut. And every message now says WHICH supplier it is
> about — Dhan or Groww — right at the top.

---

## §6. Trigger (auto-loaded paths)

Always loaded. Reinforced on any session editing:
- `crates/app/src/dhan_rest_stack.rs`
- `crates/core/src/auth/mid_session_watchdog.rs`
- `crates/core/src/auth/token_health_gauge.rs`
- `crates/core/src/notification/events.rs` (any Dhan-scoped variant)
- `deploy/aws/terraform/error-code-alarms.tf` / `app-alarms.tf`
- Any file containing `MidSessionProfileInvalidated`,
  `TokenForcedRemintTriggered`, `NoLiveTicksDuringMarketHours`,
  `rest_canary`, `fast_boot_validation`, `run_order_update_connection`, or
  `DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS`
