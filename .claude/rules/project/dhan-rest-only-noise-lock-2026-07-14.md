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

**The ONLY Dhan-scoped Telegram alerts that can ever fire are: (1) the DHAN
spot-1m pull failing/recovered, (2) the DHAN option-chain pull
failing/recovered, (3) the DHAN token-unobtainable Critical, (4) the
CloudWatch 4h token-remaining early-warning, and (5 — added 2026-07-16 per
the §2.2 dated row) the cadence expiry cross-broker DISAGREEMENT page (a
cross-broker data-integrity page naming BOTH brokers, not a Dhan-failure
family) — every other Dhan-era page, probe, watchdog
Telegram, and dead alarm is deleted or silenced; the token machinery
self-heals SILENTLY.**

---

## §2. The contract table (the final 4-item Dhan alert set + the §2.2 2026-07-16 cross-broker addition)

| # | Allowed Dhan alert | Variant(s) / route | Fires when |
|---|---|---|---|
| 1 | Spot-1m pull failing / recovered | `Spot1mFetchDegraded` (High) / `Spot1mFetchRecovered` (Info) / `Spot1mSidNotServed` (High) / `Spot1mSidServedRecovered` (Info) | the per-minute spot leg's persist-gated 3-minute escalation edge (`rest-1m-pipeline-error-codes.md`) |
| 2 | Option-chain pull failing / recovered | `ChainFetchDegraded` (High) / `ChainFetchRecovered` (Info) / `ChainEntitlementAbsent`/`Confirmed` / `ChainExpirylistFailed` (High) / **`Chain1mUnderlyingNotServed` (High) / `Chain1mUnderlyingServedRecovered` (Info) — added 2026-07-14 per the §2.1 dated directive (the Dhan mirror of the Groww #1537 per-underlying detector)** | the chain leg's own edges (`rest-1m-pipeline-error-codes.md`) |
| 3 | Token could not be obtained | `AuthenticationFailed` / `TokenRenewalFailed` (both Critical; reworded 2026-07-14 to plain English naming DHAN + the consequence: "the Dhan spot-1m and option-chain pulls will stop until this is fixed") | mint/renewal is TERMINALLY dead — the mid-session watchdog pages **ONCE PER FAILING EPISODE** (H1a latch, 2026-07-14 fix round — never the pre-fix ~30-min repeat) on EITHER (a) a forced re-mint failing terminally OR (b) the H1b attempt cap: `REMINT_MAX_ATTEMPTS_PER_EPISODE` (= 3) re-mints all "succeeded" yet the profile stayed REAL-invalid (dead-dataPlan/segment class — the body names the N re-logins + that the spot-1m/chain pulls are blocked). The latch resets on a clean profile cycle. (Its terminal arm emits `AuthenticationFailed` directly, since `force_renewal` -> `acquire_token` pages nothing on a non-RESILIENCE-03 permanent failure; the Telegram body is redacted + truncated via the house sanitizer — M2.) |
| 4 | Token expires soon (4h early warning) | CloudWatch alarm `tv-<env>-token-remaining-low` on `tv_token_remaining_seconds` → SNS → Telegram Lambda | the renewal loop stopped renewing (the watchdog-of-the-renewal-loop). The Lambda's wording is ANOTHER session's scope. |

**§2.1 — 2026-07-14 (same day, second directive): the family-(2) row gains the per-underlying
not-served pair.** Coordinator-relayed operator directive (verbatim intent, labeled as such —
the §38.0-Context-3 convention): *"make the Dhan option-chain capture complete and precise,
cross-cover the Groww gaps, and be loud on any empty or partial chain — never a silent gap."*
The motivating incident is the 2026-07-14 Groww NIFTY expiry-day cutoff (14:54 IST, 2xx/zero
strikes, `ok=2/empty=1` all afternoon, ZERO pages — PR #1537); the Dhan chain leg carries the
IDENTICAL blind spot (`chain_minute_fully_failed` requires `ok == 0`). Per this directive the
family-(2) row is extended with `Chain1mUnderlyingNotServed` (High, one page per underlying per
episode, edge-latched, ~10-minute detection latency) + `Chain1mUnderlyingServedRecovered` (Info,
falling edge). This is a variant EXTENSION of family (2), not a 5th family: it still means
"the Dhan option-chain pull is failing" — scoped to one index. Everything else in §2 stands;
the deleted/silenced table is untouched.

**§2.2 — 2026-07-16: the cadence expiry cross-broker DISAGREEMENT page joins the allowed
set.** Authority: the OPERATOR's 2026-07-16 cadence/expiry-disagreement directive, relayed
via the coordinator session (verbatim intent, labeled as such per the house §38.0-Context-3
convention — this dated row is the rule-file edit §3 demands before any new Dhan-scoped
page): a cross-broker expiry split must page the operator as a real Telegram alert, never a
log-only line. The coordinator's implementation ruling relaying it (verbatim): *"R6 —
expiry cross-broker disagreement must be a REAL typed
NotificationEvent Telegram page (not log-only) + a dated 2026-07-16 row in
dhan-rest-only-noise-lock-2026-07-14.md §2."* The cadence scheduler's pre-market expiry
resolution (`cadence-error-codes.md` §0) can find Dhan's and Groww's contract lists
DISAGREEING on today's policy expiry for one underlying — Dhan's exchange-sourced date WINS
and keys BOTH lanes. The new page is `NotificationEvent::CadenceExpiryDisagreement` (High):
edge-latched ONCE per underlying per day (the day-locked store's `newly_disagreeing` latch),
body names BOTH brokers + both dates + that the Dhan date now keys both lanes (plain English
per the 10 commandments). This is a CROSS-BROKER data-integrity page, not a Dhan-failure
family: it fires only when both brokers resolved and their answers split — never per wave,
never on a single-broker outage (those stay the log-only `expiry_unresolved` stage + the
pre-market deadline page). Emit site: the `newly_disagreeing` arm in
`crates/core/src/cadence/runner.rs`; sink threaded from boot via `cadence_boot.rs`.

> **RETIRED 2026-08-21 -- this page can no longer fire, and the row stays as the
> dated record of why.** The 2026-08-21 directive removed the second broker
> entirely (authorization in `websocket-connection-scope-lock.md`, "2026-08-21
> (THIRD quote of the day)"). A cross-broker disagreement needs two brokers to
> resolve and disagree; with one, `newly_disagreeing` is unreachable. The
> `CadenceExpiryDisagreement` variant, its emit arm and the `notifier` the boot
> threaded to reach it are all deleted -- leaving them would have meant a
> declared Telegram family that nothing can send, which is exactly the
> permanently quiet surface this table's deleted rows exist to prevent. The
> four-item Dhan family is UNCHANGED by this retirement: this was a
> cross-broker data-integrity page, never a Dhan-failure family, so nothing in
> the count moves. Re-introducing a cross-vendor parity page needs a second
> live vendor AND a fresh dated quote here first.

| Component | Disposition |
|---|---|
| Mid-session profile watchdog Telegram pages (`MidSessionProfileInvalidated` Critical + `TokenForcedRemintTriggered` High) | **Variants DELETED.** The 900s `/v2/profile` probe + the AUTH-GAP-05 forced re-mint machinery are KEPT and run SILENTLY (coded `error!` + counters only); a terminal re-mint failure routes to the family-(3) Critical. |
| AUTH-GAP-05 latch re-arm (GAP-04, 2026-07-14 backstop) | **ADDED, silent:** while a failing episode persists, `decide_remint` re-arms the retry-once latch every 2nd failing 900s cycle (~30 min retry cadence), still honoring the ~125s mint cooldown + the RESILIENCE-03 lock refusals — **BOUNDED (H1b, fix round) at `REMINT_MAX_ATTEMPTS_PER_EPISODE` (= 3) mints per episode**; the cap fires the once-per-episode family-(3) Critical when the profile is still invalid, closing the silent dead-dataPlan loop (~48 silent mints/day pre-fix). A persisting LOCK-LOST episode re-logs the RESILIENCE-01 refusal at the same ~30-min cadence (log-only, no mint, no Telegram from that arm). No routine Telegram from this path. |
| REST-stack stale-token sweep (GAP-02, 2026-07-14 backstop) | **ADDED, silent:** `dhan_rest_stack` Phase 3 runs `force_renewal_if_stale(14400)` every 900s (`DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS`) — the renewal-loop-halt backstop the lane's 4h sweep used to be. Not market-hours-gated. Terminal failure pages via family-(3). SUPERVISED (fix round: the house respawn pattern — a silent sweep death would re-open the audited gap; unwind-build self-heal only, release panics abort). Honest wording note (fix round): the ~23h renewal loop is NOT an independent retry — it HALTS PERMANENTLY after its circuit-breaker cycles; this sweep + the AUTH-GAP-05 watchdog are the retries. |
| Shared mint-cooldown gate (H3, 2026-07-14 fix round) | **ADDED, silent:** `TokenManager::renew_with_fallback` — the ONE shared re-mint entry (watchdog + GAP-02 sweep + renewal loop + `force_renewal*`) — SKIPS the `generateAccessToken` fallback with a coded warn + typed refusal (`mint-cooldown` prefix; never a page, never burns the episode latch) while a previous mint ATTEMPT is younger than the ~125s Dhan cooldown. Closes the AG5-R2-1 flagged residual the 900s sweep had tightened 16x. The boot-time `initialize` retry loop is deliberately UNGATED (calls `acquire_token` directly; owns its own >=130s floor — no boot deadlock; source-scan pinned). |
| Token-health gauge poller supervision + pre-#1522 residual (GAP-06 + M6, fix round) | The re-homed poller is SUPERVISED like the sweep. **ACCEPTED residual (M6):** on a hypothetical `dhan_enabled=true` boot BEFORE #1522 merges, the LANE path no longer spawns the poller (its main.rs spawn sites are deleted) and the stack does not run — so `tv_token_valid` would go unpublished for that boot shape. Accepted because prod is dhan-OFF (config + the Phase-A 409 refusal) and #1522 (which deletes the lane's fast arm) merges FIRST; this PR rebases after. |
| REST canary (`rest_canary_boot.rs`, REST-CANARY-01 probes 09:05/12:00/15:25 IST) | **Module + both spawn sites + the `rest-canary-01` CloudWatch filter/alarm DELETED.** The legs self-detect REST death in ~3-4 min via their own escalation edges — strictly better than 3 fixed slots. `ErrorCode::RestCanary01ProbeFailed` variant retained until C4 — **DELETED in the C4 sweep (2026-07-15)**. |
| No-tick watchdog (`no_tick_watchdog.rs`, `NoLiveTicksDuringMarketHours` Critical) | **Module + variant + both spawn sites DELETED.** Its heartbeat was fed ONLY by the retired Dhan tick pipeline; Groww stall detection is FEED-STALL-01 + the market-hours-liveness alarm. |
| Fast-boot cached-token validation (`fast_boot_validation.rs`, AUTH-GAP-06) | **Module + sole call site DELETED** (the Dhan-gated fast arm is dead with `dhan_enabled=false` and dies in #1522). `ErrorCode::AuthGap06…` variant retained until C4 — **DELETED in the C4 sweep (2026-07-15)**. |
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
> escalation edges (SPOT1M-01 / CHAIN-02 → the family-(1)/(2) High pages), and
> a SINGLE-underlying vendor cutoff (the 2026-07-14 class) pages within ~10
> counted minutes via the family-(2) `Chain1mUnderlyingNotServed` edge (a
> mid-day task respawn restarts the streak — worst case ~doubles that
> latency), and it
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
(2026-07-16, operator directive events 38df2073 + 157f7cd0) The paper-mode, receive-only order-update re-spawn is AUTHORIZED (config `[dhan_order_push] enabled = false`, default OFF). The channel is Telegram-SILENT (`notifier: None`) — the 4-item Dhan Telegram family is UNCHANGED and the 2 deleted CloudWatch alarms STAY deleted. Observability is coded logs + counters (`tv_dhan_order_updates_total`, `tv_dhan_order_push_respawn_total`) + ws_event_audit + order_audit rows only. Live order fire remains locked.

### 2026-07-21 — [dhan_order_push] PAPER ACTIVATION prep (DRAFT PR; operator-go pending)

Prep lane (coordinator-routed, 2026-07-21): `config/base.toml [dhan_order_push]
enabled` flips to `true` so the authorized (2026-07-16, events 38df2073 + 157f7cd0)
receive-only paper order-update re-spawn from `dhan_rest_stack` begins capturing.
The channel stays Telegram-SILENT (`notifier: None`) — the 4-item Dhan alert family is
UNCHANGED and the 2 deleted CloudWatch alarms STAY deleted; observability remains coded
logs + `tv_dhan_order_updates_total` / `tv_dhan_order_push_respawn_total` +
ws_event_audit + order_audit mode='paper' rows. Serde default stays OFF; live order
fire remains locked. The operator's go for the merge lands here verbatim:
<OPERATOR-GO-HERE>

---

## §2.3 — 2026-08-14: the REVIVED LIVE-LANE alert family joins the allowed set

**The verbatim operator authorization (2026-08-14, typed directly in-session — preserve
EXACTLY, typos included):**

> "Go ahead with the entire fixes dude okay? Not per Sid cloudwatch right per websocket
> connections or entire webscoket connections right dude ami right dude"

Given in DIRECT response to a message whose priority table listed, as item 3 of eight,
*"Stop the console lying + wire alarms"* — for a lane the same message had just shown to
have **zero CloudWatch alarms and zero Telegram events**. This dated row is the rule-file
edit §3 demands before any new Dhan-scoped page, and it is recorded BEFORE the terraform
lands.

**Why this file, when its §1 governs the REST-only surface.** When this lock was written
on 2026-07-14 the Dhan live WebSocket had been retired for a day, so "Dhan-scoped alert"
and "Dhan REST alert" were the same set. The 2026-08-09 revival re-created a Dhan surface
this lock never contemplated. Rather than let a whole live lane page the operator through
a gap in an unrelated file's wording, the family is declared HERE, under the same
discipline as the other four.

**The new family (5) — LIVE-LANE INTEGRITY.** Four alarms, all on metrics that **already
exist and are already shipped to CloudWatch today with nothing consuming them**:

| Metric | Alarm fires when | Why it is not noise |
|---|---|---|
| `tv_dhan_feed_stack_up` | `< 1` | The lane is down. Today this can only be discovered by opening a console that reports a hardcoded constant. |
| `tv_ticks_dropped_total` | non-zero over a window | The repo's own EMF notes call this "the single largest tick-loss window". It is billed and shipped today with **no alarm** — paying to publish a number nobody watches. |
| `tv_dhan_ws_park_total` | non-zero | A socket has parked PERMANENTLY on a fatal disconnect and will never dial again. Silent today. |
| `tv_dhan_feed_drain_respawn_total` | exceeds its restart cap | The drain died repeatedly; the lane is carrying nothing. |

**Deliberately NOT alarmed, and the reason recorded so it is not "fixed" later:**
per-instrument latency and per-instrument silence. Latency alarms on a per-instrument
basis would page constantly, because Dhan's LTT is last-TRADE time and a thin option is
legitimately minutes stale — that is the instrument being quiet, not the feed being
broken. Silence is already owned by `scan_silence` → `RISK-GAP-03`. Adding a second
signal for the same condition would produce two contradicting pages for one event.

**Cardinality is bounded by design (the operator's own correction, second sentence of the
quote above):** latency is dimensioned **per WebSocket connection — 16 fixed slots** —
never per instrument. Per-instrument figures live in RAM and are served over the existing
API. This is not a preference: 4,565 per-instrument CloudWatch metrics ≈ $1,369/mo
against a budget whose AUTOMATIC action is `STOP_EC2_INSTANCES`, i.e. the observability
feature would stop the trading box. 16 dimensions ≈ $4.80/mo.

**Cost:** +4 alarms + the per-connection latency dimensions ≈ **$6–7/mo**. This is
inside the operator's ₹7,500/mo ceiling ONLY alongside the Elastic-IP release he already
approved on 2026-07-19 (−₹361/mo); without that lever the high-side envelope sits ~₹118
under the ceiling, which is not real margin. Stated plainly rather than absorbed
silently.

**The §2 four-item REST family is UNCHANGED.** This family covers the LIVE lane only.
Every "deleted or silenced" row in §2's second table stays deleted and silenced — the
mid-session profile pages, the REST canary, the no-tick watchdog, and the two
order-update alarms are NOT revived by this row.

**What a PR that violates §2.3 looks like (REJECT):** adds a per-INSTRUMENT CloudWatch
dimension for latency or any other live-lane signal; adds a live-lane page outside the
four metrics above without its own dated row here; re-introduces any of the §2 deleted
alarms under cover of this family; or claims sub-second latency accuracy anywhere in an
operator-facing surface (the ±1 s floor is structural — Dhan's LTT is whole seconds).

### §2.3a — 2026-08-15 amendment: the DURABLE FLOOR joins family (5), and one row of §2.3 was wrong

**The verbatim operator authorization (2026-08-15, typed directly in-session):**

> "fix evryhtign dude okay?"

Given in DIRECT response to a table whose top open row read **"No loss counter pages
you — High — alarms need the market-hours gate's named list"**. That row is the scope
this quote authorizes, and this dated edit lands BEFORE the terraform, per §3.

**First, a correction to my own §2.3 (Rule 11 — a stale row manufactures false work).**
§2.3 lists four metrics, and the fourth is **`tv_dhan_feed_drain_respawn_total` "exceeds
its restart cap"**. That metric **has ZERO emit sites** — verified by source scan
2026-08-15 — and is not in the EMF allowlist, because **the drain is not respawned at
all**. If it dies the lane is over, and `tv_dhan_feed_stack_up` falls to 0, which
family-(5) alarm #1 already pages on. Building the fourth alarm as written would have
created a filter that can never match: a permanently-green dead monitor, which is the
`ws-reinject-01` / `tick-conserve-01` precedent this repo has retired twice before.
**That row is WITHDRAWN.** The correct signal for a dead drain is alarm #1.

**Second, the metric §2.3 should have named instead — `tv_dhan_ws_wal_dropped_total`.**
It is the most serious counter in the lane and it is unalarmed today:

| | `tv_ticks_dropped_total` (alarmed 2026-08-14) | `tv_dhan_ws_wal_dropped_total` (this row) |
|---|---|---|
| Where the loss happens | between the WAL and QuestDB | **before the WAL** |
| Is the frame on disk? | yes | **no** |
| Recoverable in principle | yes — the bytes exist | **no — they were never written** |

So the alarm that shipped watches the *recoverable* half of the loss chain while the
*unrecoverable* half watches nothing. `WalRingSink` counts a drop when the durable
floor — the capture-at-receipt guarantee this entire architecture is built on — did not
hold, and there is no other signal for it: the frame is simply gone, with no payload to
count downstream and no error anywhere later.

**Third, RISK-GAP-03 (`instruments never ticked / gone silent`) joins as log-filter.**
The 2026-08-12 note that wired `scan_silence` recorded plainly that its `error!` is
**log-sink-only**, and that alarming the gauges "needs the market-hours window gate". It
does not: the app already gates the emit to the CONTINUOUS session and edge-latches it
to one per episode, so a **coded-error log filter** carries the same signal with no gate
Lambda change, no threshold baseline, and a sparse near-free derived metric. A
silently-failed subscribe has **no other evidence in the entire system** — no payload,
no parse failure, no log line of its own — so absence measured against a seeded key is
the only thing that can ever report it.

**Family (5) is therefore SIX signals, not four:** lane down · socket parked · ticks
dropped · **durable floor breached (new)** · **connected-but-silent (new)** · [the
withdrawn drain-respawn row]. Cost: **+1 metric alarm ≈ $0.10/mo** and **+1 errcode
log-filter alarm ≈ $0.10/mo** (sparse, dimensionless, billed only in hours the code
fires) — **~$0.20/mo total** against the $100 kill-ceiling whose 90% line is $90.

**Still NOT claimed:** `tv_dhan_ws_subscribe_failed_total`, `tv_dhan_ws_ring_full_total`,
`tv_dhan_ws_ring_bytes_full_total`, `tv_tick_persist_errors_total`,
`tv_tick_rows_refused_total` and `tv_dhan_feed_seals_dropped_total` remain **visible but
unpageable** — charted on the operator dashboard since 2026-08-15, alarmed by nothing.
That is a deliberate stopping point rather than an oversight: each would add ~$0.10/mo,
several are downstream symptoms that the two alarms above would already have fired for,
and a family of eleven pagers for one subsystem trains an operator to ignore all of
them. Listed here so the gap is a decision on the record, not something to be discovered
from a quiet dashboard.

### §2.3b — 2026-08-21: the lane can carry ZERO ticks and page nobody; and the contract path has no counters at all

**The verbatim operator authorization (2026-08-21, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "fix evrythgin entiley dude okay i cannot miss even a single ticks and neevr eevr due to ebs iops volume mibs or because of any fucking issues like codes configruatiosn aws instance kernel memory pressure or confirguation or it can be any issues or any extreme worst case permutations an dcombiantions of entire worst cases also this should never ever fuckign happen bro okay? so whatevr is needed or reocmmeneded go ahead with the entire fixes and solutions dude oaky?"

Given in DIRECT response to a plan whose Phase 2 named exactly these two items and
stated plainly that both were **blocked on a dated row in this file** — this file's §3
makes any new Dhan-scoped page a REJECT without one. This row is that authorization,
recorded HERE first, before the terraform, per the rule-file-first law.

**Why the first one matters more than any alarm already in family (5).** Every one of
the ten live-lane alarms answers *"did something break"*. Not one answers *"is the feed
producing anything"* — and those are different questions. `tv_dhan_feed_ingest_ticks_total`
has been EMF-shipped since 2026-08-14 and charted on the operator dashboard since, and
**no alarm has ever read it**. A lane that dials, connects, subscribes and delivers
nothing reports fully green: the lane-up gauge reads 1, the connection gauge reads
healthy, and every loss counter reads zero — because nothing was lost, nothing arrived.
That is exactly what the 2026-08-12 session looked like (`compared: 0`,
`missing_live: 373`, 12 dial failures), and it was found by reading a cross-verify log
line rather than by being told. This is the paid-for-and-unwatched shape that §2.3
alarms 3 and 9 were created to end, on the one signal that separates "nothing broke"
from "nothing ran".

**Why the second one is not optional either.** `dhan_contract_universe.rs` carries
**zero** `metrics::` calls. "No options resolved today", "the ATM window shrank to
three", "the artifact was unreadable" are `error!` lines and struct fields that nothing
consumes. A session missing ~22,000 authorized contracts leaves no number anywhere for
an alarm or a triage path to read. The 2026-08-20 incident is the shape:
`atm_window_reason = "no_ladders"` was recorded, printed, and ignored.

**Family (5) therefore gains two members** — `dhan-no-ticks-flowing` and
`dhan-contract-universe-failed`. Both are LIVE-LANE alarms; the §2 four-item REST family
is UNCHANGED, and every "deleted or silenced" row in §2's second table stays deleted and
silenced.

**Binding constraints on the implementation, each of which is a REJECT if broken:**

1. **The tick-flow alarm MUST be market-hours gated.** It treats missing data as
   breaching — necessarily, because a dead app publishes no datapoint and a lane that
   never receives a frame never registers the series at all, so `notBreaching` would
   read both as health. But `breaching` without a gate pages every evening at 17:30 and
   all weekend, which this file's own §2.3a calls the fastest way to train an operator
   to ignore an alarm. It joins the `market_hours_liveness_gate` Lambda's `ALARM_NAMES`
   list in the SAME change, taking that gate from 3 alarms to 4. Landing the flip
   without the gate membership re-creates the nightly false page.
2. **The contract alarm is `notBreaching` and ungated**, deliberately: the box is
   stopped overnight, so no-data is the normal off-hours state, and this alarm reports a
   DEFECT rather than silence. The dark-lane case belongs to the alarm above.
3. **Every `reason` label on the contract counter must be a real defect.** The EMF
   processor folds labels to `{host}` by summing, so a name carrying successes too would
   fire on a healthy day. The success side stays on the existing info line.
4. **The contract counter emits ONCE per session at the terminal verdict, never per
   retry.** `no_ladders` before 09:16 is NORMAL — no tick has landed yet — so a
   per-attempt emit would page every healthy trading morning.
5. **Any new EMF name must land in BOTH selector copies** (`cloudwatch-agent.json` and
   `user-data.sh.tftpl`), which `cw_agent_selector_lockstep_guard.rs` pins byte-for-byte,
   and must respect the `user_data_size_guard.rs` budget.

**Cost.** `tv_dhan_feed_ingest_ticks_total` is already shipped, so its alarm is **+1
alarm ≈ $0.10/mo and no new EMF name**. The contract counter is **+1 EMF name ≈ $0.30/mo
and +1 alarm ≈ $0.10/mo** — one series, since the `reason` label folds. **~$0.50/mo
total** against the $130 kill-ceiling whose 90% line is $117.

**⚠ What this row does NOT do (Rule 11, no false-OK).** An alarm on tick flow does not
make ticks flow. It converts a silent all-day outage into a page within ~10 minutes of
the session opening — that is the entire claim, and it is worth making precisely because
the 2026-08-12 outage ran a full session undetected. It does not address the reasons a
lane might carry nothing, and it cannot see loss that happens UPSTREAM at Dhan's side:
their published architecture skips a slow consumer forward to "the latest available
state" with no sequence number, so intermediate ticks discarded there are invisible to
every counter we own. The 15:31 REST cross-verification remains the only ground truth
for that class, and a non-zero `compared` from it remains the only evidence this
repository can offer that the feed works at all.

#### §2.3b-i — 2026-08-21 (SAME DAY): the tick-flow alarm reads a GAUGE, because the counter's meaning was never verified

**No new authorization is claimed or needed.** §2.3b above authorizes exactly one
alarm — "is the feed producing anything" — and this note records that its SIGNAL was
corrected within hours of landing, before the terraform ever applied. The alarm name,
the market-hours gate membership, and the count of alarms in family (5) are all
unchanged. This is an implementation correction, recorded here rather than made
quietly, because the reason is worth more than the change.

**What was wrong.** The alarm read the counter `tv_dhan_feed_ingest_ticks_total` with
`Sum < 1` over two 300-second windows. Its own header conceded the residual that
undoes it: this repository has never verified, from the sandbox, whether the
CloudWatch agent's prometheus pipeline publishes each scrape's DELTA or the running
CUMULATIVE total. `auth-failed-alarm.tf` records the same uncertainty and lands on the
over-paging side of it; this alarm landed on the other side.

The two readings are not equally survivable here:

| | delta reading | cumulative reading |
|---|---|---|
| `Sum` over 300 s | ticks in the window | ≈ 5 × the running session total |
| `Sum < 1` | true only when nothing arrived — **correct** | false forever after the first tick of the morning — **blind** |
| Operator sees | a page when the feed dies | green, all day, whatever the feed does |

So the one alarm written to prove ticks are flowing was, on a coin flip, the alarm
most likely to prove nothing. That is the false-OK class this file exists to stop, and
it was sitting inside the change that closed a false-OK.

**The fix is not a better guess.** `tv_dhan_feed_last_tick_age_secs` is a GAUGE, and a
gauge is published verbatim by both pipelines — there is no delta to compute for a
value that is free to go down. The alarm now reads `Maximum >= 300` over two windows
and means the same thing under either reading, so the unverifiable pipeline detail
stops being load-bearing.

**Three things it improves beyond removing the ambiguity**, each of which was a real
weakness of the counter version:

1. **It measures PERSISTENCE, not decode.** The gauge is stamped in `flush_and_record`
   only when a flush actually wrote rows, so a QuestDB outage decays it while the
   socket is busy — correct, because during one the feed is not delivering. The
   counter incremented on a decoded tick, which can be true while nothing reaches
   disk.
2. **The series is DENSE from the drain's first second**, published on the 30-second
   silence timer regardless of whether a frame ever arrives. The counter's handles are
   built lazily inside the frame arm, so a lane that received nothing never registered
   the series at all — the exact case the alarm was for, and the reason it needed
   `breaching` to catch it.
3. **Before the first tick it reports the drain's own uptime**, not zero. A lane that
   dialled, connected, subscribed and received nothing therefore pages instead of
   reporting perfect health — the 2026-08-12 shape.

**Cost delta, stated rather than absorbed.** §2.3b costed this as "+1 alarm ≈ $0.10/mo
and no new EMF name". It is now +1 alarm and +1 EMF name ≈ **$0.40/mo**, because the
gauge needs a selector entry in both copies. Against the $130 kill-ceiling whose 90%
action line is $117, and whose current margin is $4.28, that is real money and it is
named here rather than discovered on an invoice.

**Ratchet:** `crates/app/tests/no_ticks_alarm_gauge_guard.rs` (7 tests) pins that the
alarm reads the gauge and **not** the counter, that it uses `Maximum` with a
`GreaterThanOrEqualToThreshold` comparison (an age gauge breaches when it GROWS — the
counter-era `LessThanThreshold` would fire whenever the feed is healthy), that the
alarm name survives so the gate still arms it, that the metric is in both EMF selector
copies, and that production — not only a test — publishes it.

**⚠ Still not claimed.** A correct alarm does not make ticks flow, and this one still
cannot see loss that happens upstream at Dhan's side. The 15:31 cross-verification's
`compared` count remains the only evidence this repository can offer that the feed
works at all. What changed is narrower and worth stating exactly: the alarm that
reports a dead lane can no longer be silently disabled by a pipeline detail nobody
here has been able to check.

**The residual this does NOT fix, named so it is not mistaken for fixed:** every OTHER
alarm in this file's family (5) still reads a `_total` counter and still inherits the
same unverified assumption. Their failure direction is milder — a loss counter under
the cumulative reading pages correctly on the FIRST loss and then latches, so a second
episode in the same session is silent — but milder is not fixed, and it is a separate
piece of work rather than something this note closed.

### §2.3c — 2026-08-21: the SPILL tier joins family (5); and one gap this file was told about did not exist

**The verbatim operator authorization (2026-08-21, typed directly in-session — preserve
EXACTLY, typos included):**

> "Go ahead wirh your recommendation. Dude okay"

Given in DIRECT response to a message that ended: *"Making these losses actually page you
is a decision only you can authorize — say the word and I'll draft the dated row and the
alarm together, with its cost line."* This dated row is that draft, recorded BEFORE the
terraform, per §3.

#### First, a correction to what that message claimed (Rule 11 — an over-stated gap is
#### still a false claim, and it manufactures work)

The message asked for authorization partly on the grounds that
`tv_ws_frame_wal_reinjected_dropped_total` was "not EMF-selected and has no alarm". **Both
halves are FALSE.** Verified by source scan the same day: the metric IS in the EMF selector
(`cloudwatch-agent.json`), and `live-lane-alarms.tf:354` carries
`tv-<env>-wal-frames-not-recovered` on it at `threshold = 1`,
`GreaterThanOrEqualToThreshold`, one evaluation period — it pages on the FIRST unrecovered
frame.

So the WAL half of the work was already done, and the six refusal paths wired on 2026-08-21
feed a counter that already ships and already pages. That is a better outcome than was
claimed, and it is recorded here rather than quietly enjoyed: an executor that over-states a
gap sends the next session hunting for something that is not there, which is the same waste
a stale O(1)-table row causes in the other direction.

#### What IS genuinely dark, and what this row authorizes

The SPILL tier — not the WAL tier — has no CloudWatch presence at all:

| Metric | EMF-selected? | Alarmed? |
|---|---|---|
| `tv_ticks_spilled_total` | **no** | no |
| `tv_tick_spill_replay_failed_total` | **no** (new 2026-08-21) | no |
| `tv_tick_spill_replayed_bytes_total` | **no** (new 2026-08-21) | no |

That matters because the two tiers fail differently. The WAL tier's counter means "frames
we could not re-fold" — a discrete, already-pageable loss. The spill tier's counters mean
"an ILP flush failed and we rescued the buffer to disk" and "the automatic drain could not
put it back". A spill that is never drained becomes a real tick loss at the 512 MiB cap, and
today nothing outside the box's own log would say so.

**Family (5) therefore gains two members:**

| Alarm | Fires when | Why it is not noise |
|---|---|---|
| `tv-<env>-tick-spill-replay-failing` | `tv_tick_spill_replay_failed_total >= 1` in a period | The drain is the recovery arm. If it cannot put rescued ticks back, the rescue is a countdown to the cap, not a save. |
| `tv-<env>-ticks-spilling` | `tv_ticks_spilled_total >= 1` in a period | An ILP flush failed. Distinct from `tv_ticks_dropped_total`, which both counters now increment: the drop alarm says "a flush failed"; this says "and it went to disk", which is the difference between loss and deferred recovery. |

**Both are market-hours gated**, joining the `market_hours_liveness_gate` Lambda's
`ALARM_NAMES` list in the same change. Without the gate they page every evening and all
weekend, which this file's own §2.3a calls the fastest way to train an operator to ignore an
alarm.

**Cost:** +3 EMF metric names ≈ $0.90/mo, +2 alarms ≈ $0.20/mo ⇒ **~$1.10/mo** against the
$130 kill-ceiling whose 90% action line is $117. `tv_tick_spill_replayed_bytes_total` is
shipped without an alarm deliberately — it is the SUCCESS signal, and a chart of successful
recoveries is worth having beside the two failure alarms without adding a third pager.

**⚠ What this does NOT do (Rule 11).** An alarm on a spill does not stop the flush timeouts
that cause spills; their cause is QuestDB write latency under live load and is untouched. It
converts a loss visible only in the box's own log into one that reaches the operator — that
is the entire claim.

**What a PR that violates §2.3c looks like (REJECT):** adds either alarm without the
market-hours gate membership in the same change; adds an alarm on the SUCCESS counter
(`replayed_bytes`), which would page on recovery working; adds a per-INSTRUMENT dimension to
any of the three (the §2.3 cardinality rule stands — 4,565 per-instrument metrics ≈
$1,369/mo against a budget whose automatic action stops the trading box); or re-states the
corrected claim above as though the WAL counter were unalarmed.

### §2.3d — 2026-08-22 FINDING (no authorization claimed): the universe can collapse 24,600 → 4 with every signal green

**This section authorizes NOTHING.** It records a verified defect found by an
adversarial permutation sweep, and the reason it was NOT fixed in the same pass.
Adding the alarm it argues for still requires a fresh dated operator quote, per
§3. It is written here because §2.3a/§2.3b are where this file records what is
measured-but-unpageable, and this is the sharper case: measured, and not even
shipped.

**The defect.** `select_live_universe` (`dhan_live_universe.rs:252`) falls back to
the 4-instrument index universe on EITHER of two triggers: the resolved set
exceeding the capacity envelope, or a master that produced "no usable widening"
(unreadable/absent/empty artifact). The fallback is correct and deliberate —
fail-soft beats a dark lane. What is wrong is that nobody is told.

| # | Verified fact | Evidence |
|---|---|---|
| 1 | Fallback lands on 4 instruments | `dhan_live_universe.rs:252` |
| 2 | It logs `error!` + counter + gauge | `:483`, `record_master_sourcing_fallback` |
| 3 | `tv_dhan_live_universe_instruments` is in **0** deploy files | not in either EMF selector copy, not on the dashboard, not alarmed |
| 4 | `WS-GAP-03` is **not** among the 14 metric-filter-alarmed codes | `error-code-alarms.tf` |
| 5 | The §2.3b tick-flow alarm CANNOT catch it | it reads flow, and 4 indices still tick — the gauge stays healthy |

Net: a **99.98% loss of market data** (24,600 → 4) presents to the operator as a
normal session. Every dashboard line is green, no page fires, and the sole trace
is one ERROR line in a log sink nothing watches. That is the false-OK class this
file exists to prevent, with a larger blast radius than most of what IS alarmed.

**Why it is not merely theoretical.** The margin is thin and shrinking from the
outside: `websocket-connection-scope-lock.md` (2026-08-21) puts the authorized set
at ~24,600 against a 25,000 cap — 400 to 1,427 spare — while the SAME section
rules index option chains **UNCAPPED**, and records vendor-controlled chain depth
observed at 542 and at 2,037 for three indices. That section also states plainly
that its "fits" verdict rests on a 2026-04-25 measurement. A volatile expiry week
that adds strikes is an ordinary event, not an exotic one.

**Why it was not fixed in the finding pass (the honest blocker).** The fix is to
ship the gauge in both EMF selector copies. That was attempted and REVERTED:
`user_data_size_guard` failed at **15,905 bytes against a 15,872 budget — 33 over**
(AWS hard limit 16,384, 512 reserved). The metric name is 33 bytes. The guard's own
message forbids the cheap workaround verbatim — "DO NOT fix this by shaving
comments off unrelated blocks" — and prescribes moving content to a file `cp`'d in
after the Step 5 repo clone. That is a change to the prod boot path whose failure
mode (clone fails ⇒ agent gets no config ⇒ NO metrics at all) is worse than the gap
it closes, and it cannot be tested from a dev container. Doing it blind was
refused deliberately.

**The two candidate fixes, for whoever takes this up.** (a) Free the 33 bytes by
the architectural route the guard prescribes, then ship the gauge and alarm on it
falling below a floor during market hours. (b) Cheaper and rule-cheaper: add
`WS-GAP-03` to the existing metric-filter set in `error-code-alarms.tf` — it reuses
the log-filter machinery already in place, costs ~$0.10/mo, and needs no user-data
byte. Either way the alarm itself needs the §3 dated quote first.

#### §2.3d-i — 2026-08-22 AUTHORIZED AND SHIPPED (the dated quote §3 requires)

**The verbatim operator authorization (2026-08-22, typed directly in-session):**

> "Fix and resolve wvrytni fdude okay"

Given in DIRECT response to a message that named this fix, priced it, and stated
plainly that it needed his go: *"the cheaper fix — adding `WS-GAP-03` to the
existing metric-filter set — is ~$0.10/mo, needs no user-data byte… Say the word
and it's a small, contained change."* This row is the §3 dated quote, recorded
BEFORE the terraform, per the rule-file-first law.

**⚠ The fix that shipped is NOT the fix that quote authorized, and the difference
matters.** The recommendation said "add WS-GAP-03 to the metric-filter set". Acting
on it revealed that recommendation was WRONG: `WS-GAP-03` has **~50 emit sites** —
every dial failure, reconnect and pool-supervisor event in the WebSocket layer
carries it. A bare `$.code = "WS-GAP-03"` filter would page on ordinary connection
churn, which is the RISK-GAP-03 noise trap this file records (25 pages in one
session) with fifty times the surface. Recorded rather than quietly corrected,
because the executor proposed it and the operator approved it on that description.

**What shipped instead:** a THREE-condition pattern —
`{ $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "fell_back_to_indices" }`.
The `source` label appears on exactly one ERROR emit, the universe-collapse arm in
`dhan_live_universe.rs`; the sibling emits on that path are `info!` and are already
excluded by the level condition. So the alarm fires on the collapse and on nothing
else. `ok_recovery = false`, matching the discrete-event precedent: the universe is
chosen once per boot, so an auto-OK an hour later means the datapoint aged out, not
that the next session widened correctly.

**Family (5) therefore gains a SEVENTH signal.** Cost: one log-filter alarm on an
existing metric-filter lane, ~$0.10/mo, no new EMF name and no user-data byte —
which is why this route was taken over shipping the gauge (that path is still
blocked at 33 bytes over the user-data budget, per §2.3d).

**What this does NOT fix, and is not claimed:** the collapse is now PAGED, not
PREVENTED. The 400–1,427-instrument margin, the UNCAPPED index chains, and the
four-month-old measurement behind the "it fits" verdict are all unchanged. An
operator woken by this alarm still has to decide whether to raise the cap or
narrow the set. The gauge remains unshipped, so there is still no way to watch the
number CREEP toward the cap — only to be told after it crossed.

#### §2.3d-ii — 2026-08-22 MEASURED: the user-data template has ZERO bytes free

§2.3d records the headroom gauge as blocked "33 bytes over the budget", which
reads like a shortfall specific to that one metric. Measured today, it is not:

| | bytes |
|---|---|
| `user-data.sh.tftpl` renders to | **15,872** |
| Guard budget (16,384 AWS limit − 512 reserved) | **15,872** |
| **Free margin** | **0** |

Method: appending a known 63-byte block made the guard report "renders to 15,935
… (63 over)", so the unmodified render is exactly 15,872 — the budget, to the
byte. (A 1-byte append passes, because a trailing character with no newline is
trimmed before measurement; that near-miss is why this was measured rather than
inferred.)

**The consequence is not about one gauge.** EVERY future addition to user-data
is blocked: one more metric in the EMF selector, one environment variable, one
line of boot script. The next person to try will read "33 bytes over" in §2.3d,
assume their smaller change fits, and discover it does not.

**Two routes remain, and the cheap-looking one was rejected on evidence.** The
EMF selector is a regex alternation in which 13 metrics share `tv_dhan_feed_`
and 10 share `tv_dhan_ws_`, so mechanical prefix factoring would free well over
100 bytes. It was NOT taken: that regex is the live shipping path for 76 metrics
which 65 alarms depend on, its failure mode is silent (metrics simply stop
arriving), and it cannot be tested from a dev container. Restructuring it to
ship one gauge is the wrong trade even with a strong equivalence proof. The
guard's own prescription — move content to a file `cp`'d in after the Step 5
repo clone — remains the correct fix, and remains a prod boot-path change that
needs someone who can watch a real instance boot.

### §2.3e — 2026-08-22: the pre-open readiness deadline joins family (5)

**The verbatim operator demand (2026-08-22, typed directly in-session, repeated
four times across the day — preserve EXACTLY, typos included):**

> "9.13 am evryhtign dude okay?"

> "see i said 9.13 am eveyrhtign shdou lbe entiltey conencted an subscribe of entire around 25k instruemnts right dude okay?"

> "see i ened at 9.13 am it shdu lebe ntirely subscribed conencted rigth dude"

> "solvign and fixing due okay?"

This is the dated quote the rule-file-first law requires, recorded BEFORE the
terraform.

**What it answers.** `PREOPEN_READY_DEADLINE_IST_SECS = 09:12:00` landed in
`6f328c99` and forces the contract dial rather than waiting on the 60% pricing
quorum (whose only previous escape was `out_of_time` — **10:00 IST**). The
deadline is enforced in code. Whether it is MET on a given morning was, until
this row, invisible to every operator surface.

**The gap this closes, and it is one I created.** The commit publishes
`tv_dhan_preopen_ready_secs`, and I described the requirement as "now
measurable". It is not, on its own: the EMF selector is an EXPLICIT LIST, the
gauge is not in it, and the user-data template renders at **exactly** its
15,872-byte budget with **zero** free (measured 2026-08-22, §2.3d-ii) — so the
name cannot be added without the boot-path restructure that section defers. The
gauge reaches the local exporter and stops there.

**The route that costs no user-data byte.** A CloudWatch metric filter can
extract a NUMERIC JSON field as the metric value — `value = "$.field"` rather
than the `value = "1"` every existing filter in `error-code-alarms.tf` uses. The
readiness ERROR line already carries `ready_at_ist_secs`, so the same log
stream yields both the page and the trendable number, through the metric-filter
lane that is already in place.

**Family (5) therefore gains an EIGHTH signal**: a session that finishes its
attach after 09:12. `notBreaching` on missing data and deliberately so — the
box is stopped overnight and does not attach on weekends, so no-data is the
normal off-hours state; this alarm reports a LATE attach, never a silent one.
The dark-lane case is already owned by `dhan-no-ticks-flowing` (§2.3b-i).

**Cost:** one metric filter + one alarm on an existing log stream ≈ **$0.10/mo**,
no new EMF name, no user-data byte. Against the $130 kill-ceiling whose 90%
action line is $117.

**⚠ What this does NOT do (Rule 11).** An alarm on lateness does not make the
attach early. It converts "did we make 09:12?" from unanswerable into a page,
which is the entire claim. The deadline itself can still be missed for reasons
outside this lane — pre-open prices arriving late is the exchange's business —
and depth has still never faced a market open (§2.3b's residual stands).

**What a PR that violates this section looks like (REJECT):** alarms the gauge
instead of the log field (the gauge does not reach CloudWatch); makes the
filter `breaching` on missing data (pages every night and weekend); adds the
EMF name without the byte-budget restructure §2.3d-ii describes; or reports the
deadline as met on the strength of a dial rather than a completed attach.

### §2.3f — 2026-08-25: the cross-verify verdict gets its page, and the watchmen get watched

**The verbatim operator authorization (2026-08-25, typed directly in-session — preserve
EXACTLY, typos included):**

> "Fix wbrytjonf dude oaku"

Given in DIRECT response to a message whose "Still open, not done" list named exactly
these items and stated that the first needed his go: *"**No new alarm exists.** Fix 3
made one *writable*; creating it needs your dated quote per §3 of the noise lock"* and
*"**The start-watchdog Lambda has no `Errors` alarm** — the only Lambda in the tree
without one, and it's the component that starts the box and pages about it. Small and
self-contained if you want it."* This section is the dated record the rule-file-first
law requires, written BEFORE the terraform.

#### ⚠ First, a correction to the message that earned this quote

The claim that the start-watchdog is **the only** Lambda without an `Errors` alarm is
**FALSE**, and it understated the gap by six times. Counted in source the same day:
13 `aws_lambda_function` resources, **7** with an `Errors` alarm, **6 without**:

| Unalarmed Lambda | What it does when it works | What its silence costs |
|---|---|---|
| `start_watchdog` | starts the box at 08:30 IST, retries, verifies the 17:30 stop | a failed start pages nobody; the trading day is simply missing |
| `hard_stop_guard` | force-stops the box outside its window / over budget | a failed stop bills silently; a spuriously-failing one is invisible |
| `boot_heartbeat_gate` | arms and disarms the boot alarm around the window | a stuck gate leaves the boot alarm armed all night or disarmed all morning |
| `deploy_watchdog` | detects a box booted on a stale binary | the 2026-07-09 stale-binary class returns unannounced |
| `tv_daily_budget_digest` | the daily spend digest | the digest just stops arriving |
| `questdb_console_proxy` | serves the console (its FRONT half **is** alarmed) | half a surface watched, half not |

The pattern is this file's own recurring one: **a set nobody enumerated.** The seven
that are alarmed were each added by the PR that created them; the six that are not were
each created by a PR that did not think to. Nothing was ever decided about them.

#### What this section authorizes

**(a) The cross-verification verdict becomes a page.** Family (5) gains a ninth and
tenth signal: `tv-<env>-errcode-ws-gap-03-xverify-vacuous` and
`tv-<env>-errcode-ws-gap-03-xverify-failed`. The 15:41 live-vs-official comparison is
the only ground truth the revived Dhan feed has, and both of its failure verdicts —
compared ZERO minutes, or could not run at all — reach nothing today.

They are **log-filter** alarms, not metric alarms, and that is deliberate:
`tv_dhan_feed_xverify_runs_total` is 31 bytes and needs 32 with its separating pipe
against 31 free in the user-data budget (§2.3d-ii) — so the EMF route misses **by one
byte**, while the log-filter lane costs no user-data byte at all.

Each pattern carries **three** conditions, not the usual two:

```
{ $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "xverify_vacuous" }
{ $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "xverify_failed"  }
```

**Two entries rather than one `||` pattern, deliberately.** The first draft matched
both verdicts in a single `($.source = "a" || $.source = "b")` filter, and
`terraform plan` accepted it — which means nothing: the provider treats `pattern` as
an opaque string, so filter-pattern SYNTAX is parsed only by the real
PutMetricFilter call at APPLY time. A malformed pattern would pass every PR check
and break the post-merge apply lane. Two single-condition entries use only the shape
already proven live by `ws-gap-03-universe-collapse`, cost one extra dime, and name
the two verdicts separately — which is better triage anyway, since they have
different causes and different next steps.

`WS-GAP-03` has ~50 emit sites in `dhan_feed_stack.rs` — every dial failure, reconnect
and pool event — so a bare code filter would page on ordinary connection churn. That is
the RISK-GAP-03 noise trap with fifty times the surface, and it is the same mistake
§2.3d-i records being approved and then caught. The `source` field, added by PR #1808
specifically so this alarm could exist, appears on exactly these two emits.

`ok_recovery = false`: the comparison runs **once per session**, so an auto-OK an hour
later means the datapoint aged out, never that the next run compared anything.

**(b) All six unalarmed Lambdas get an `Errors` alarm**, on the house shape — `Sum >= 1`
over 300s, `notBreaching`, and `ok_actions = []` (their auto-OK is an aged-out datapoint,
never a fix — the round-14 precedent).

**(c) A ratchet, which is the durable half.** Six alarms fix today's list; they do not
stop the seventh Lambda from arriving unwatched next month, which is exactly how these
six accumulated. `every_lambda_has_an_errors_alarm_or_a_declared_exemption` reads every
`aws_lambda_function` in the terraform directory and requires each to have an `Errors`
alarm targeting it, or to be declared exempt with a reason. A new Lambda now fails the
build until someone decides.

#### Honest cost

8 new alarms at ~$0.10/mo = **~$0.80/mo**. Measured against the live account the same
day: August MTD actual **$48.87**, AWS forecast **$61.51**, ceiling **$130** with the
90% `STOP_EC2_INSTANCES` action line at **$117**. No new EMF name, no user-data byte.

#### ⚠ What this does NOT do (Rule 11)

- **It does not make the feed work.** The cross-verify alarm reports that the comparison
  produced no verdict. A non-zero `compared` remains the only evidence this repository
  can offer that ticks arrive, and no alarm produces one.
- **It does not fix the AZ pin.** The message that earned this quote also listed
  automatic AZ failover, and that one is NOT taken here: remediation means flipping
  termination protection, snapshotting a 200 GB root, TERMINATING the production
  instance, re-applying, and restoring. That is a destructive, hard-to-reverse operation
  on the live trading box, and "fix everything" is not the explicit go-ahead a terminate
  needs. Detection is already correct and already pages (`classify_start_failure` names
  the remedy). Automating the remediation needs its own dated quote that says so in as
  many words.
- **An `Errors` alarm catches a Lambda that THROWS.** A Lambda that returns success
  having done nothing useful is invisible to it, because a dropped schedule produces
  no error at all. **CORRECTED 2026-08-25 (same day, by an adversarial re-read):** an
  earlier draft of this row claimed `start_watchdog` has "its own not-invoked alarm".
  It does NOT. A tree-wide scan for `metric_name = "Invocations"` returns exactly TWO
  alarms — on `dhan-token-minter` and `market-hours-liveness-gate` — and
  `start-watchdog-lambda.tf` declares one alarm only. So the component that STARTS the
  trading box every morning is blind to the 2026-07-02 repo-wide scheduler-drop class,
  and this row asserted the opposite while pointing at it as reassurance. Adding that
  alarm is a real follow-up; claiming it already existed was exactly the false-OK this
  file exists to stop.

#### What a PR that violates §2.3f looks like (REJECT)

- Filters either cross-verify alarm on `WS-GAP-03` alone, or drops the `source` condition
  (pages on every reconnect — the trap this section exists to avoid).
- Sets `ok_recovery = true` on it (a once-per-session emitter cannot recover by aging).
- Adds an `aws_lambda_function` without an `Errors` alarm or a declared exemption.
- Gives any of the eight `ok_actions` (a green "recovered" page for an aged-out
  datapoint is the Rule-11 false-OK the round-14 note records).
- Ships the EMF name for `tv_dhan_feed_xverify_runs_total` without the byte-budget
  restructure §2.3d-ii describes — it is over by one byte, and shaving an unrelated
  comment to make room is what that guard's own message forbids.
- Automates an instance TERMINATE for AZ failover under cover of this quote.

### §2.3g — 2026-08-25: the disk went to 20 KB free and 15 tables suspended, and neither gauge had an alarm

**The verbatim operator demand (2026-08-25, typed directly in-session — preserve
EXACTLY, typos included):**

> "see menawhoel alogn with these ensure to achieve O(1) irrepsetcive of any woerts case sistauitons or errros or scenarios it can ve any stuaitons dudew which is db memory ram app forntend backend ir tut ca be antthing dude see which is forntend bakcend db app memory ram db aws isnatcnes ebs iops imbs disk rpessure ram wal disk spill ring ufefr dlq or etc etc"

> "See this process shod lbe entitled fully comprehsoisnvely automated no manual intervention or human inputs or human monitoring shod lobe expected"

The quote names **disk pressure, WAL and disk spill** by hand and rules out
human monitoring. This dated row is the rule-file edit §3 requires before any
new page, recorded BEFORE the terraform.

#### The evidence, read from the live account rather than inferred

`get-metric-statistics`, namespace `Tickvault/Prod`, 2026-08-25:

| IST | `tv_spill_dir_free_bytes` (Min) | `tv_questdb_wal_suspended_tables` (Max) |
|---|---:|---:|
| 08:30 | **38.8 GB** | 0 |
| 09:30 | **14.5 GB** | 0 |
| 10:30 | **20,480 bytes** | 3 |
| 11:30 | 20,480 bytes | **15** |
| 12:30 | 20,480 bytes | 11 |
| 13:30 | 58.6 GB | 0 |

24 GB vanished in the first hour, the volume sat at **twenty kilobytes free**
for three hours, and fifteen tables suspended themselves. A WAL-suspended
QuestDB table keeps ACKing ILP writes while silently not applying them, so
every writer reported success throughout.

**Neither metric has an alarm.** Verified live:
`describe-alarms --query 'MetricAlarms[?MetricName==...]'` returns EMPTY for
both. Both are EMF-selected and both reached CloudWatch on schedule — the data
was there, in the operator's own account, the whole time, and nothing was
watching it. The hour of warning between 38.8 GB and 14.5 GB went to nobody.

#### ⚠ A claim from the same audit that live data REFUTED

The audit reported that `tv-prod-disk-fill-rate-high` "cannot fire inside a
session" because its 6-hour period × 2 evaluations needs 12 hours of data on a
box that runs ~9. **That is FALSE**, and it is recorded here because acting on
it would have damaged a working alarm. `describe-alarms` returns:

> `Threshold Crossed: 1 out of the last 2 datapoints [1.4588218265938622 (25/08/26 12:22:00)] was not greater than the threshold (4.0)`

It evaluates, it produces datapoints, it can fire. Its periods are wall-clock
aligned, not "12 hours of samples" — a 9-hour session spans two aligned 6-hour
buckets and both carry samples.

**What the same reading DOES show is worse than the claim it refutes.** On the
day the volume hit 100% for three hours, this alarm measured **1.46 points per
day against a threshold of 4** — arithmetically correct and completely useless.
The 24-hour drift stays low *because* overnight archival drops partitions; the
failure is INTRADAY, and a daily-trend alarm cannot see an intraday fill by
construction. Its 6-hour window is the right window for the trend it measures
and must not be "fixed"; what was missing is an intraday signal, which is
exactly what the free-bytes gauge is.

#### What this authorizes — family (5) gains an ELEVENTH and TWELFTH signal

| Alarm | Metric | Fires when | Why this shape |
|---|---|---|---|
| `tv-<env>-spill-dir-free-low` | `tv_spill_dir_free_bytes` | `Minimum <= 20 GiB` | 20 GiB is ~1 hour of headroom at the MEASURED 24 GB/h open burn, and the archiver's own high-water trigger sits at 75% used — this fires while remediation is still possible, not after |
| `tv-<env>-questdb-wal-suspended` | `tv_questdb_wal_suspended_tables` | `Maximum >= 1` | The ONE detector for the one failure where every tick counter reports success and the rows are not there. Threshold 1, not 3: a single suspended table is already silent loss for that table |

Both `treat_missing_data = notBreaching` and ungated: the box is stopped
overnight, so no-data is the normal off-hours state, and each reports a DEFECT
rather than silence. The dark-lane case is already owned by
`dhan-no-ticks-flowing` (§2.3b-i). Neither takes `ok_actions` — a gauge falling
back is an aged-out datapoint or a recovery the operator performed, never proof
the space came back (the round-14 precedent).

**Cost:** 2 alarms ≈ **$0.20/mo**, no new EMF name and no user-data byte —
which matters, because the user-data template renders at exactly its
15,872-byte budget with **zero** free (§2.3d-ii), so an EMF-route alarm is
currently impossible.

#### ⚠ What this does NOT do (Rule 11)

An alarm on free bytes does not create free bytes. The volume filled because a
session's ingest exceeds what 300 GB holds with a 2-day archival floor, and
that arithmetic is untouched here. This converts a three-hour full disk that
reached the operator only when he asked why a table was empty into a page an
hour before it happens — that is the entire claim.

It also does not fix the probe that FEEDS the WAL gauge. Until 2026-08-25 a
QuestDB schema drift that changed the `suspended` cell's TYPE made
`parse_wal_tables_response` skip every row and return `Ok(vec![])`, which set
the gauge to a confident **0**. Alarming a gauge whose producer can fail open
would be alarming a lie; the probe now returns `AllRowsSkipped` instead, so the
alarm has something honest to read. `tv_wal_suspension_probe_failed_total` is
still NOT EMF-selected and so is still CloudWatch-invisible — blocked by the
same zero-byte budget, recorded rather than papered over.

#### What a PR that violates §2.3g looks like (REJECT)

- Changes `disk_fill_rate_high`'s 6-hour period on the strength of the refuted
  "cannot fire" claim.
- Gives either alarm `ok_actions` (a green "recovered" for an aged-out
  datapoint is the Rule-11 false recovery).
- Makes either `breaching` on missing data (pages every night and weekend).
- Alarms the WAL gauge while leaving the probe able to fail open.
- Adds a per-INSTRUMENT dimension to either (the §2.3 cardinality rule stands).

### §2.3h — 2026-08-25: the byte budget that blocked three counters was already freed, and one of them guards an alarm shipped hours earlier

**The verbatim operator authorization (2026-08-25, typed directly in-session):**

> "Yes go ahead fix and resolve everything"

Given in DIRECT response to a message that named exactly this work, its cost, and
the correction that unblocked it: *"the follow-up is now genuinely available, and
it's small: EMF-select the four counters — the three that make your per-minute
high/low recovery visible (firing 2,632 times a session today, currently with no
way to alarm if it stops) and `tv_wal_suspension_probe_failed_total`."* This
dated row is recorded BEFORE the terraform and the selector edit, per the
rule-file-first law.

#### ⚠ First, the correction that makes this possible — I said this was blocked, four times

§2.3d-ii records the user-data template rendering at **exactly 15,872 of 15,872
bytes, zero free**, and names that as the reason `tv_wal_suspension_probe_failed_total`
could not be EMF-selected. §2.3g repeats it. I repeated it again in three separate
session check-ins, each time citing the one before rather than re-measuring.

**It stopped being true on 2026-08-25**, when #1815 performed the exact
restructure the guard's own message prescribes — moving the EMF selector OUT of
the boot template and into `deploy/aws/cloudwatch-agent.json`, `cp`'d into place
after the Step 5 repo clone. Measured across commits:

| commit | rendered user-data |
|---|---:|
| `18aebcfb1~1` (before #1815) | **15,841** — 31 bytes free |
| `18aebcfb1` (#1815) | **13,869** |
| `cacea254d` (#1817) | 13,869 |

So ~2,000 bytes were freed, and better than that: `cw_agent_selector_lockstep_guard`
now **forbids** a second copy in the boot template, so adding an EMF name costs
**zero** user-data bytes rather than 33. The blocker is not merely relieved, it is
structurally gone.

**The reusable lesson is the one this file keeps re-learning:** a measurement
carries a date, and a measurement quoted from a quote is not a measurement. The
byte figure was correct when first taken and stale within three days, and it was
propagated by citation four times without anyone re-running the one command that
settles it.

#### What this authorizes

| Metric | Why | Alarm? |
|---|---|---|
| `tv_wal_suspension_probe_failed_total` | The §2.3g `questdb-wal-suspended` alarm reads a GAUGE whose producer can fail. Before #1816 that producer could fail OPEN — return a confident 0 while tables were suspended. #1816 made it fail loud, but the counter that says so reached nothing. Without this, an alarm shipped hours earlier can be silently blind. | **YES** — `>= 1` |
| `tv_candle_session_high_recovered_total` | The operator asked about per-minute high/low specifically. This is the count of minute-highs widened to the exchange's own running day high — prints we never received a tick for. 2,632 today. | no — see below |
| `tv_candle_session_low_recovered_total` | Same, for lows. 247 today. | no |

#### Deliberately NOT shipped, and why the omission is the judgment

**`tv_candle_day_high_adopted_total` and `day_low_adopted_total` are LEFT OUT.**
They fire on the first bucket of a session only — today they read **0 and 3**
against the session counters' 2,632 and 247. They are a subset signal of the same
mechanism at ~1% of the resolution, and each EMF name is ~$0.30/mo. Shipping five
names to carry three names' worth of signal is the "paid for and unwatched" shape
§2.3b was written to end.

**The two session counters are shipped as METRICS but deliberately NOT ALARMED.**
An alarm on "recovery stopped" would fire on a quiet market: a flat session
legitimately sets no new day highs, so zero recoveries is a NORMAL reading, not a
broken fold. That is precisely the noise trap this file records repeatedly — a
pager that cries on an ordinary day teaches the operator to ignore it. They are
charted so the mechanism is observable, and the thing that would actually indicate
breakage (the fold refusing input) is already covered by
`tv_indicator_tick_rejected_total`.

> ### ⚠ CORRECTED 2026-08-26 — "They are charted" was FALSE
>
> The two counters were shipped to CloudWatch and charted **nowhere**.
> `grep tv_candle_session_high_recovered_total deploy/aws/terraform/dashboard.tf`
> returned **0**; the names appeared only in the EMF selector. So they were paid
> for (~$0.60/mo), alarmed by nothing *by design*, and visible on no surface at
> all — the "measured but unwatched" class §2.3b was written to end, sitting
> inside the very section that reports closing it.
>
> This is worse than an ordinary gap, and the reason is worth stating: anyone
> auditing the claim **by reading** would have found it satisfied. A wrong
> sentence in this file does not merely fail to warn — it certifies. That is the
> same cost the `day_ohlc_tracker` row records on 2026-08-12 and the
> `WAL-SUSPEND-01` row records on 2026-08-25, arriving a third time.
>
> **FIXED the same day.** Both counters are charted in `dashboard.tf` Row 13,
> deliberately NOT under that row's "every line should sit flat at zero"
> heading — a quiet market legitimately sets no new day highs, so zero here is
> normal and a rising line is the fold working. The paragraph's reasoning about
> not alarming them stands unchanged and is re-verified; only the claim that
> they were charted was false.
>
> **The durable half is not the widget.** The same sweep found **ten** shipped
> metrics that were on no dashboard and in no alarm, and the guard meant to
> prevent exactly this — `dashboard_live_lane_visibility_guard.rs` — could not
> have caught any of them: it filtered to `tv_dhan_*`, so it held for one prefix
> and drifted silently for the other eight. Its scope is now **every** selected
> name, bite-proven against a metric that could not have failed the old version.
> Eight of the ten are charted; the two left off are recorded there with reasons,
> one of them sharpened (`tv_dhan_feed_depth_total` folds its success and failure
> arms into a single summed line, so a plain chart could not show a failure spike
> at all — charting it would close the entry while creating the false comfort the
> entry exists to prevent).


#### Honest cost

3 EMF names ≈ **$0.90/mo**, 1 alarm ≈ **$0.10/mo** ⇒ **~$1.00/mo**, zero
user-data bytes.

**Against the budget, stated rather than absorbed:** §2.3c put a maximal month at
**$118.28** against a 90% `STOP_EC2_INSTANCES` line of **$117.00** at the live
`limit_amount` of $130 — already **$1.28 over**, and this takes it to ~$2.28 over.
The live account is nowhere near it (MTD **$48.87**, forecast **$61.51**, measured
2026-08-25), so no stop is imminent — but the maximal-month arithmetic crosses the
automatic action line and that gap widens with every addition. The levers remain
the already-approved Quote 10 EIP release (−$3.60/mo, execution bundled with an
instance recreate) or an operator decision on `limit_amount`, which Quote 18
forbids raising above 125 and which cannot be set to 125 because 90% of that is
$112.50, below the bill. Neither is taken here.

#### What a PR that violates §2.3h looks like (REJECT)

- Alarms `tv_candle_session_high_recovered_total` or `session_low_recovered_total`
  (fires on a quiet market — the noise trap named above).
- Re-adds a second copy of the EMF selector to `user-data.sh.tftpl` (the guard
  forbids it, and it is what consumed the 2,000 bytes in the first place).
- Cites the "15,872 with zero free" figure without re-measuring it.
- Adds an EMF name whose producer does not exist (`emf_selector_producer_guard`).
- Gives the probe-failed alarm `ok_actions` — a counter aging out of its window is
  not a repair.

### §2.3i — 2026-08-26: the deaf socket gets its page

**The verbatim operator authorization (2026-08-26, answering a presented
choice):**

> "Yes, phone me"

Given in DIRECT response to a question that named the alarm, its threshold,
its price, and the reason it had NOT been added unilaterally: *"Adding the
phone call costs about 10 cents a month, and I did NOT add it on my own
because your worst-case month already lands just above the line where AWS
automatically switches the trading box OFF."* This dated row is the rule-file
edit §3 demands before any new Dhan-scoped page, recorded BEFORE the
terraform.

**The failure it covers, and why nothing else can.** A socket that keeps
answering pings but stops delivering data defeats every existing mechanism,
each for a structural reason rather than an oversight:

| Mechanism | Why it is blind to this |
|---|---|
| Idle watchdog (27 s) | governs SILENCE on the wire; a ponging socket is not silent |
| Reconnect family | stays flat — the defining property of a deaf socket is that **nothing about it is retrying**. This is why "alarm the reconnect counters", the recommendation on record, cannot work |
| `dhan-no-ticks-flowing` (§2.3b) | reads the LANE. With fifteen of sixteen sockets delivering, the lane's last tick is always ~1 s old |
| `tv_dhan_ws_alive_connections` | counts sockets DIALED, not DELIVERING |

**Family (5) therefore gains a THIRTEENTH signal:**
`tv-<env>-dhan-worst-socket-deaf` on `tv_dhan_ws_worst_conn_tick_age_secs`,
`Maximum >= 600` over one 300 s period.

**The threshold is not invented.** `dhan-no-ticks-flowing` pages at 300 s × 2
periods = ten minutes of lane silence. This is the same question scoped to one
socket, so it takes the same ten minutes — expressed as one 600 s crossing
rather than two 300 s ones, because this gauge is a per-socket age that only
climbs.

**`notBreaching`, and that differs from its two siblings deliberately.**
`dhan_live_lane_down` and `dhan_no_ticks_flowing` are `breaching` because a
DEAD APP publishes nothing and that absence must page. Those two already cover
that case; a third alarm firing on the same absence buys nothing and triples
the noise of one outage. This one answers a narrower question that only has
meaning while the lane is alive: *of the sockets that ARE running, is one of
them deaf?*

**⚠ It joins the market-hours gate in the SAME change, and that is not
optional.** Without it this pages **every single trading day at ~15:40**:
after the 15:30 close every socket legitimately stops delivering, the gauge
crosses 600 within ten minutes, and the alarm fires on a market that is simply
shut. The gate now arms **7** alarms. Landing the alarm without the gate
membership re-creates the exact noise trap §2.3a records.

**Cost:** 1 alarm ≈ **$0.10/mo**, no new EMF name — the gauge was selected the
same day (82 names, ~$0.30/mo, priced in the count ratchet).

**⚠ What this does NOT do (Rule 11).** It reports that a socket has gone
quiet; it cannot distinguish "the socket is deaf" from "every instrument on
that socket genuinely stopped trading". The alarm text says so and points at
the lane gauge and the never-ticked count for that comparison. It also does
not prevent a socket going deaf, and detection latency is the 30 s publish
cadence plus the 600 s threshold.

**What a PR that violates §2.3i looks like (REJECT):** lands the alarm without
the gate membership (the daily 15:40 false page); flips it to `breaching`
(duplicates the two absence alarms and pages three times for one outage);
adds a per-CONNECTION dimension (the §2.3 cardinality rule stands — sixteen
dimensions ≈ $4.80/mo to answer a yes/no question one series answers);
or lowers the threshold below the lane alarm's ten minutes without a measured
baseline.
