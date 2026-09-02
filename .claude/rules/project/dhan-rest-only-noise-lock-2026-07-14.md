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

#### §2.3d-ii — 2026-08-22 MEASURED: the user-data template has ZERO bytes free  ⚠ SUPERSEDED 2026-08-26 (see the note at the end of this section)

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


> ### ⚠ SUPERSEDED 2026-08-26 — the budget was freed the same week, and two
> ### later sections in THIS file already say so
>
> §2.3d-ii's measurement was correct on 2026-08-22 and was obsolete within
> three days. **#1815 performed exactly the restructure the paragraph above
> calls "the correct fix"** — it moved the EMF selector OUT of the boot
> template and into `deploy/aws/cloudwatch-agent.json`, `cp`'d into place
> after the Step 5 repo clone. §2.3h (2026-08-25) records that and its
> before/after byte counts.
>
> **Re-measured 2026-08-26**, by running the guard rather than quoting anyone:
>
> | | bytes |
> |---|---|
> | `user_data_size_guard` reports rendered | **13,869** |
> | Budget (16,384 AWS limit − 512 required margin) | 15,872 |
> | **Free** | **2,003** (2,515 to the raw AWS limit) |
> | `tv_dhan_feed` occurrences left in `user-data.sh.tftpl` | **0** |
>
> That 13,869 matches §2.3h's figure to the byte, from an independent run,
> which is what settles which of the two sections is stale.
>
> **So every "blocked by the byte budget" reason in this file is now dead**,
> and adding an EMF name costs **zero** user-data bytes — `cw_agent_selector_
> lockstep_guard` now *forbids* a second copy in the boot template. Sections
> reasoning from the old figure, all superseded on this point: §2.3d (the
> headroom gauge, "33 bytes over"), §2.3f (`tv_dhan_feed_xverify_runs_total`,
> "misses by one byte"), and §2.3g (`tv_wal_suspension_probe_failed_total`,
> "still CloudWatch-invisible — blocked by the same zero-byte budget").
>
> **§2.3g is the one that matters.** Its own alarm
> `tv-<env>-questdb-wal-suspended` reads a GAUGE, and that section states the
> principle plainly: *"Alarming a gauge whose producer can fail open would be
> alarming a lie."* The producer-failure counter is what closes that loop, and
> the file says it cannot ship for a reason that stopped being true on
> 2026-08-25.
>
> **THIS NOTE AUTHORIZES NOTHING.** It records a measurement and retires a
> dead reason. Shipping any of those three metrics is an operator decision —
> each is ~$0.30/mo against a ceiling this file documents as having roughly
> $4.28 of margin at the 90% `STOP_EC2_INSTANCES` line, and §3 governs the
> protocol. What changes here is only that "we can't, the budget is full" is
> no longer a true answer.
>
> Recorded because §2.3d-ii is written as a MEASUREMENT, which is exactly the
> kind of claim this file's own §2.3f correction warns carries a date: *"a
> claim about whether a MECHANISM fired is checkable in one query and must be
> checked before it is written."* The same applies to a byte count. It was
> checked, and it moved.

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

  > **⚠ RE-CORRECTED 2026-08-26 — the correction above went stale within HOURS of
  > being written, and it is now the thing manufacturing a false finding.**
  >
  > `start-watchdog-lambda.tf:345` declares
  > `aws_cloudwatch_metric_alarm.start_watchdog_not_invoked` —
  > `tv-<env>-start-watchdog-not-invoked`, `Invocations < 1` over 6 hours,
  > `treat_missing_data = breaching` — and its own `alarm_description` names the
  > 2026-07-02 scheduler-drop class verbatim. It landed in `cacea254d`
  > (2026-08-25 19:16:55Z, PR #1817), whose title is literally *"the kill-switch
  > nobody checked was still running — and a claim in four rule sections was
  > overstated"*. So the follow-up this bullet calls for was DONE the same day, by
  > the same change, and this bullet was never updated.
  >
  > The tree-wide scan now returns **NINE** occurrences across **SEVEN** files,
  > producing **EIGHT** distinct not-invoked alarms: boot-heartbeat-gate,
  > daily-budget-digest, deploy-watchdog, dhan-token-minter, hard-stop-guard,
  > market-hours-gate, market-open-readiness, start-watchdog.
  >
  > **The cost is the same one this bullet was written to warn about, one level
  > up.** A reader trusting it today concludes the component that starts the
  > trading box is blind, and goes to build an alarm that already exists —
  > exactly the wasted work the `day_ohlc_tracker` row (2026-08-12) and the
  > `WAL-SUSPEND-01` row (2026-08-25) each record. **A correction is a claim
  > like any other, and it goes stale like any other.** The durable lesson is
  > not "check harder" — it is that an alarm-existence claim is one `grep`
  > (`grep -rn 'metric_name *= *"Invocations"' deploy/aws/terraform/`), so it
  > must be re-run at the moment of writing rather than carried forward, in a
  > correction just as much as in the sentence it corrects.
  >
  > **And it was checkable without even grepping.** The same PR added
  > `cloudwatch_app_alarms_wiring.rs::
  > every_scheduled_lambda_has_a_did_it_run_alarm_or_a_declared_exemption`,
  > which FAILS THE BUILD if any scheduled Lambda lacks a not-invoked alarm and
  > has no declared exemption. It has been GREEN ever since. So this bullet did
  > not merely go stale against the terraform — it contradicted a passing test
  > in the same repository, on the same day, added by the same commit. When a
  > prose claim and a green ratchet disagree, the ratchet is the one that
  > cannot be wrong by inattention.

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

### §2.3j — 2026-08-28: the durable floor's recovery path becomes visible, and the one way it loses ticks silently becomes pageable

**The verbatim operator demand (2026-08-28, typed directly in-session — preserve
EXACTLY, typos included):**

> "I dont need any hallucination or illusion I just need the working guaranteed assurance solution"

> "Irrespective of any situations i need clearly note frontend backend db or it cna be anyhtign always i need ultra fast extreme speed ... by covering all worst cases situations scenarios errors exceptions bugs issues causes shoudl be always comprehensively fully extremely automated alogn with real time checks"

Given as a standing constraint across the session in which the write-ahead
log's record format gained a receipt field (`TVW3`). This dated row is the
rule-file edit §3 demands before any new Dhan-scoped page, recorded BEFORE
the terraform.

#### What became visible

Four counters on the WAL replay path joined the EMF selector (84 → 88 names):

| Metric | What a non-zero value means |
|---|---|
| `tv_wal_replay_recovered_total` | frames staged at boot were successfully given back — the durable floor doing its job |
| `tv_wal_replay_deferred_segments_total` | a segment was left for a later pass |
| `tv_dhan_wal_refolded_total` | recovered frames were re-folded into candles |
| **`tv_wal_replay_unknown_magic_total`** | **a segment carried a record format this binary cannot read** |

#### Why the fourth one gets an alarm and the others do not

The first three describe recovery WORKING. The fourth describes recovery
being **impossible**, and its failure mode is the exact class this file
exists to stop.

A binary that meets an unreadable segment magic has, until 2026-08-27,
returned `Ok(vec![])` — an empty replay, indistinguishable from a clean one.
The segment is then STAGED AND ARCHIVED as successfully replayed. Every
frame in it is gone, permanently, and nothing anywhere reports a loss:
there is no payload left to count, no parse to fail, and no downstream
consumer that knows those frames were ever captured.

**The concrete way this happens is a deploy rollback.** A `TVW3` segment
written this morning, read by a rolled-back binary that only knows `TVW1`
and `TVW2`, is silently destroyed. That is capture-at-receipt — the floor
the whole zero-tick-loss claim rests on — failing in the one direction no
other signal can see.

Family (5) therefore gains a **FOURTEENTH** signal:
`tv-<env>-wal-replay-unknown-magic` on `tv_wal_replay_unknown_magic_total`,
`Sum >= 1` over one 300 s period, `treat_missing_data = notBreaching`,
`ok_actions = []` (a counter aging out of its window is not a repair —
the frames do not come back).

**Ungated, deliberately.** WAL replay runs at BOOT, and the box boots at
08:30 — before the market-hours gate opens. Gating this alarm would make it
structurally incapable of firing on the one path it exists to watch. It is
also the correct shape for an out-of-hours deploy, which is exactly when a
rollback happens.

#### ⚠ Honest cost, and it crosses a line this file has been tracking

4 EMF names × $0.30 + 1 alarm × $0.10 = **~$1.30/mo**.

The COST NOTE of 2026-08-25 (THIRD) put a maximal month at **~$119.28**
against the budget's automatic `STOP_EC2_INSTANCES` line of **$117.00**
(90% of the $130 `limit_amount`). This takes it to **~$120.58** — about
**$3.58 above the line that switches the trading box off** in a maximal
month. The live account is nowhere near it (August MTD $48.87, forecast
$61.51), so nothing fires today, but the gap widens with every addition and
is now the largest it has been.

This is stated rather than absorbed because the alternative was to ship the
three diagnostic names unalarmed — the "paid for and unwatched" shape this
very list has twice refused — or to ship nothing and leave the durable
floor's recovery invisible while the operator's hardest stated requirement
is that not one tick is lost. Shipping four names and pricing them honestly
is the defensible middle; pretending the budget position did not move would
not be.

The levers are unchanged and neither is taken here: the already-approved
Quote 10 Elastic IP release (−$3.60/mo, bundled with an instance recreate,
which alone would put the maximal month back UNDER the line), or an
operator decision on `limit_amount` — which Quote 18 forbids raising above
125, and which cannot be set to 125 because 90% of that is $112.50, below
the bill.

#### What a PR that violates §2.3j looks like (REJECT)

- Alarms `tv_wal_replay_recovered_total` or `tv_dhan_wal_refolded_total` —
  both rise on NORMAL recovery, so an alarm on either pages on the system
  working.
- Market-hours-gates the unknown-magic alarm (WAL replay runs at boot,
  before the gate opens — gating it makes it unable to fire).
- Gives it `ok_actions` (the frames are permanently gone; a counter aging
  out is not a recovery).
- Adds a per-SEGMENT or per-FEED dimension (the §2.3 cardinality rule
  stands).
- Bumps the EMF name count without a dated cost note — the ratchet's own
  instruction, and the reason this section exists.

### §2.3k — 2026-08-28: the cross-verification can say "these two records disagree" and page nobody

**The verbatim operator authorization (2026-08-28, typed directly in-session — preserve
EXACTLY, typos included):**

> "dude snure to fix and resoleve evryhtyehing entilrey whaevr you have highlighted dude okay?"

Given in DIRECT response to a ranked list in which this item appeared as a CRITICAL row
reading *"A failed cross-verification pages nobody — the revived feed's only ground truth
can report mass divergence and nothing alerts. Only 'didn't run' and 'ran on nothing'
page."* That list is the scope this quote authorizes; it is the same general-go-ahead
shape `daily-universe-scope-expansion-2026-05-27.md` §28.2 and §28.3 already accept, where
a broad instruction selects the work the preceding message enumerated. Recorded HERE
before the terraform, per the rule-file-first law.

Earlier the same session the operator asked what the cross-verification writes and whether
it notifies — *"where will you save those results ... telegram notification"* — which is
the question this row answers: it wrote three tables and told nobody when the answer was
bad.

#### The gap

Three verdicts can end the 15:41 run. Until now only two were reachable by an alarm:

| Verdict | Meaning | Carried a `source` field | Paged |
|---|---|---|---|
| `xverify_failed` | the check could not run | yes | yes |
| `xverify_vacuous` | it ran and compared ZERO minutes | yes | yes |
| **diverged** | **it ran, it measured, and the two records disagree** | **no** | **no** |

The third is the only one that is an actual finding about the feed. It logged at `info!`
among forty other fields on the same line. So the single check that exists to say whether
the revived Dhan feed is trustworthy could answer **"no"** in a form nothing was listening
for — while the two "we couldn't tell you" outcomes both paged.

#### Family (5) gains a FIFTEENTH signal

`tv-<env>-errcode-ws-gap-03-xverify-diverged`, a **log-filter** alarm on the same
three-condition shape §2.3f settled on:

```
{ $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "xverify_diverged" }
```

Three conditions, not one: `WS-GAP-03` has ~50 emit sites — every dial failure and
reconnect carries it — so a bare-code filter pages on ordinary connection churn. That is
the trap §2.3d-i records being approved and then caught, and the `source` field is what
makes this filter possible at all.

`ok_recovery = false`: the comparison runs ONCE per session, so an auto-OK an hour later
means the datapoint aged out, never that the next run agreed.

#### ⚠ Why the threshold is HALF, and why that is not a number picked out of the air

A non-zero divergence count is **EXPECTED and is not a defect**.
`cross-verify-1m-error-codes.md` §1 records that a sampled live stream and the vendor's
full tape legitimately differ, and says to *"track the trend, not the absolute count"*.
Paging on any divergence would page every trading day, which this file repeatedly names as
the fastest way to teach an operator to ignore an alarm.

**No baseline exists for what a normal rate looks like.** So a 1% or 5% threshold would be
a number invented and then presented as a measurement — the exact class this file's own
corrections keep retiring. What CAN be asserted with no baseline is narrower and holds at
any baseline: if **more than half** the compared price fields disagree beyond the
configured tolerance, the two records are not describing the same market. No sampling-noise
argument survives that.

So the bar sits where it is *defensible today* rather than where it might be *optimal after
a month of data*. The `outcome="diverged"` counter added beside it is what will eventually
supply that data, and tightening the threshold later needs its own dated row.

#### ⚠ Honest cost, and it moves further past the automatic action line

One log-filter alarm ≈ **$0.10/mo**. No new EMF name, no user-data byte.

§2.3j put a maximal month at **~$120.58** against the budget's automatic
`STOP_EC2_INSTANCES` line of **$117.00** (90% of the $130 `limit_amount`). This takes it to
**~$120.68 — about $3.68 above the line that switches the trading box off** in a maximal
month. The live account is nowhere near it (August MTD $48.87, forecast $61.51, measured
2026-08-25), so nothing fires today, but the gap widens with every addition and is now the
largest it has been. The levers are unchanged and neither is taken here: the
already-approved Quote 10 Elastic IP release (−$3.60/mo, which alone would put the maximal
month back under the line), or an operator decision on `limit_amount`.

#### ⚠ A stale claim corrected in the same change (Rule 11)

The comment at the vacuous emit site asserted the metric route was *"blocked by ONE BYTE"*,
because the EMF selector lived in a user-data template rendering 15,841 of a 15,872-byte
budget. That was true when measured on **2026-08-25** and stopped being true the **same
day** — #1815 moved the selector into `deploy/aws/cloudwatch-agent.json`. Re-measured
2026-08-28 by running the guard: the template renders **13,823 bytes, 2,049 free**, and
`tv_dhan_feed` appears **zero** times in it. The byte blocker is gone; what remains is the
cost decision above. Recorded because a stale measurement quoted as a live blocker is how
this repository has manufactured false findings before, and a byte count carries a date
like any other claim.

#### What a PR that violates §2.3k looks like (REJECT)

- Filters the alarm on `WS-GAP-03` alone, or drops the `source` condition (pages on every
  reconnect — the trap this shape exists to avoid).
- Sets `ok_recovery = true` on a once-per-session emitter.
- Lowers the divergence threshold below half without a MEASURED baseline and its own dated
  row — an unbaselined tightening is the false page this section refused to ship.
- Pages on any non-zero `cells_diverged` (fires every trading day).
- Adds a per-INSTRUMENT dimension (the §2.3 cardinality rule stands).
- Claims the feed is verified on the strength of this alarm being quiet: a quiet alarm here
  means "not catastrophically diverged", never "correct". A non-zero `compared` remains the
  only evidence that the check ran at all.


## COST NOTE 2026-08-28 — the refusal family had never been visible, and it was hiding 2.4% tick loss (+~$0.30/mo)

**Authorization:** operator, 2026-08-28, verbatim: *"Always ensure saving into db
and auditing logging tracking capturing debugging finding searching monitoring
dashboard vislualisign extremeims analyzing easil yaccessibel eveyrhtign also by
covering entirely each and every nook and corner but always ensure to achieve
O(1) always"*. Recorded here before the selector change, per the rule-file-first
law.

**What was added:** `tv_aggregator_tick_refused_total` to the EMF selector.
**+1 metric name ≈ $0.30/mo, no new alarm, no user-data byte** (the selector
moved out of the boot template in #1815, so an EMF name is now free of the byte
budget that blocked three earlier additions).

**Why it earns the money, measured rather than argued.** The candle fold has
FIVE refusal reasons — six as of today — and **not one of them has ever reached
CloudWatch**. `aws cloudwatch list-metrics --metric-name
tv_aggregator_tick_refused_total` returned `{"Metrics": []}`.

The consequence, read from the live account for 2026-08-27:

| Reading | Value |
|---|---|
| Ticks decoded (`tv_dhan_feed_ingest_ticks_total`) | **83,446,729** |
| Ticks HARD-refused on timestamp, no row written | **2,008,916** |
| Share of the session | **2.41%** |
| Where this was visible | one 30-second log line, and nowhere else |

A loss of that size ran every session and could not be charted, alarmed, or
trended. That is the "paid for and unwatched" shape inverted: not paid for and
not watched.

**⚠ HONEST LIMIT.** The EMF processor folds label values into one summed series
per host, so this publishes the TOTAL refusal rate and NOT the per-reason split.
The split stays in the `AGGREGATOR-DROP-01` line, which already carries
`refused_price` / `refused_timestamp` / `refused_slot_exhausted` as separate
fields. A total is what was missing — the reason a 2.4% loss went unnoticed is
that nobody could see a number at all, not that they saw the wrong breakdown.

**NOT claimed:** that this alarms. It is charted, not paged. A threshold needs a
baseline nobody has yet — the honest baseline is being established by this very
change, and the loss it was hiding is FIXED in the same commit (the out-of-band
class becomes candle-only, so the row is written). An alarm on a number that is
about to change by 2.4% would be calibrated against the defect rather than
against health.

**Budget position.** The COST NOTE of 2026-08-25 (THIRD) put a maximal month at
~$119.28 against the 90% `STOP_EC2_INSTANCES` line of $117.00. This takes it to
~$119.58, about $2.58 over that automatic action line, against the operator's
$125 hard cap (Quote 18). The live account is far below it (MTD $48.87, forecast
$61.51, measured 2026-08-25), so nothing fires today — but the maximal-month
arithmetic crosses the line and widens with each addition. Unchanged levers,
neither taken here: the already-approved Quote 10 Elastic IP release
(−$3.60/mo), or an operator decision on `limit_amount`.

### §2.3k — 2026-08-28: the gates paged five "recovered" messages every trading morning, for alarms that were never broken

**The operator demand this answers (2026-08-27/28, typed directly in-session —
preserve EXACTLY, expletives and typos included):**

> "why and how the fuck this fuckign issues or rerros rot elgram issues telegram ntoifications occurign always"

Repeated the following day. This dated row records a change that REMOVES
pages; §3's REJECT list governs ADDING them, so no new authorization is
claimed — but an alerting-behaviour change gets recorded either way, because
the next reader needs to know why a "recovered" message they used to see every
morning stopped arriving.

#### The mechanism, verified in source

Both window gates do the same two things in the same wrong order.
`market_hours_gate.rs` (`OpenDecision::Enable`) and `alarm_gate.rs`
(`GateMode::Open`) each call:

1. `enable_alarm_actions()`
2. `set_alarm_state(OK)` — "so a stale ALARM from a prior window does not
   immediately re-fire on the first enabled evaluation"

AWS executes an alarm's actions on `SetAlarmState` **when the new state
differs from the previous one**. So the sequence every trading day is:

| When | What | Result |
|---|---|---|
| 17:30 | box stops; no data all night | — |
| overnight | `treat_missing_data = "breaching"` → alarm enters ALARM | no page (actions disabled — correct) |
| 09:20 | gate ENABLES actions | stale ALARM now has live actions |
| 09:20 | gate resets to OK — state DIFFERS | **`ok_actions` fires → Telegram** |

Five gated alarms are both `breaching` and carry `ok_actions`, verified per
alarm: `dhan_live_lane_down`, `dhan_no_ticks_flowing`, `depth_steering_stalled`
(all `live-lane-alarms.tf`), `market_hours_liveness_missing`
(`market-hours-liveness-alarm.tf`), `app_log_ingestion_silent`
(`log-retention.tf`). That is **five "✅ recovered" messages at the same minute
every trading morning** — roughly 110 a month — reporting recovery from a
condition that was the box being switched off overnight, exactly as scheduled.

#### The fix, and why it is the ordering rather than the `ok_actions`

The obvious fix is to drop `ok_actions` on those five. It is the WRONG fix: it
would also silence the genuine case, where one of these alarms fires
mid-session and then really does recover — which is a signal the operator
wants.

The gates now **reset to OK first, then enable actions**. The state change
happens while actions are still disabled, so it pages nobody, and the alarm
begins its enabled life already in OK — which is what the original comment
says it wanted. Every alarm keeps its `ok_actions` and every real in-session
recovery still pages.

This also closes a second, quieter hole in the same two statements: enabling
first leaves a window in which a stale ALARM has LIVE actions, so the old
order could fire a spurious ALARM page as well as the spurious OK. Resetting
first removes both.

#### What this does NOT do (Rule 11)

- It does not reduce the page count of a REAL incident. The five alarms above
  still fire, and still send their recovery.
- It does not touch the larger finding from the same sweep: **ALARM records are
  never deduplicated anywhere between CloudWatch and Telegram.** The Lambda's
  `should_suppress_ok` cache covers OK only, by an explicit never-drop law, so
  one flapping alarm still pages once per flap — `risk-gap-03` produced 25
  pages in a single session, recorded verbatim in `error-code-alarms.tf`.
  Changing that means editing a law written in capitals and belongs in its own
  change with its own row.
- It does not remove the four dead monitors found by the same sweep
  (`seal_writer_dropped`, `order_fill_lag_high`, `orders_placed_storm`,
  `api_auth_failed` — each alarms a metric with no producer or no EMF entry, so
  none can ever fire). They cost no pages; they cost false confidence.

  > **⚠ CORRECTED 2026-09-02 — "four dead monitors" is wrong; only ONE is dead,
  > and the other three have live producers.** Checked against the live account
  > and the source rather than carried forward:
  >
  > | Named above | Metric it reads | Rust emit sites | EMF-selected | Verdict |
  > |---|---|---:|---:|---|
  > | `order_fill_lag_high` | `tv_order_fill_lag_seconds` | **0** | **no** | **DEAD — confirmed** |
  > | `seal_writer_dropped` | `tv_seal_writer_drain_dropped_total` | 2 | yes | ALIVE |
  > | `api_auth_failed` | `tv_api_auth_failed_total` | 4 | log-filter | ALIVE |
  > | `orders_placed_storm` | `tv_orders_placed_delta_total` | derived from `tv_orders_placed_total` (live emits) | log-filter | ALIVE |
  >
  > The three "alive" rows are log-metric-filter alarms or EMF-selected
  > counters with real producers. The mistake is a specific and repeatable
  > one: a `grep` for `metrics::counter!` on the same LINE as the metric name
  > misses every counter built from a `const` identifier, which is the house
  > style here — the same trap caught `tv_dhan_feed_depth_total` and
  > `tv_dhan_ws_lag_excluded_total` in the 2026-09-02 sweep before they became
  > false findings too.
  >
  > **Cost of believing the stale row:** it names three monitors as unable to
  > fire, so a reader budgets work to rebuild watchdogs that already work —
  > the exact `day_ohlc_tracker` (2026-08-12) failure mode, on an
  > observability claim instead of a complexity one.
  >
  > `order_fill_lag_high` genuinely is dead and stays flagged:
  > `cloudwatch_app_alarms_wiring.rs` calls it "the ONLY entry in this whole
  > list with zero emit sites in crates/*/src", and
  > `cloudwatch_dormant_alarms_guard.rs` records it as dormant pending its
  > emitter. Live confirmation: it is absent from
  > `list-metrics --namespace Tickvault/Prod` entirely.

**What a PR that violates §2.3k looks like (REJECT):** restores
`enable_alarm_actions` before `set_alarm_state` in either gate; drops
`ok_actions` from the five alarms above as an alternative fix (it silences the
real recovery too); or removes the guard pinning the order.

### §2.3l — 2026-08-28: two counters that report permanent loss, one of them from inside the task whose death it reports

**The verbatim operator demand (2026-08-28, typed directly in-session — preserve
EXACTLY, typos included):**

> "hwo will youa cheiev always O(1) alogn with nto even a signel ticks loss across the entire codebase and workspace even Especially cover all kinds of extreme permuations combiantions of excpeitons errors situations conditions bugs scenarios ideas etc etc etc inclduign out of box also dude okay?"

> "esnire to fix and reosleve evrythgin entirley dude okay?"

Given while an adversarial permutation sweep of the aggregator and seal path was
running. Both findings below came out of that sweep and neither was known when
the session started. This dated row is the rule-file edit §3 requires before any
new Dhan-scoped page, recorded BEFORE the terraform.

#### Finding 1 — the seal-writer death signal is published from inside the dead task

`tv_dhan_feed_seals_rescued_total` has **zero occurrences anywhere under
`deploy/`**: not EMF-selected, not charted, not alarmed. It is the counter that
increments when a seal cannot be handed to the seal writer and is escalated to
disk instead — which is precisely what happens when that writer is dead or its
channel is saturated.

What *is* alarmed is `tv_seal_writer_drain_total{kind="dropped"}`
(`seal-drop-alarm.tf`). That counter is incremented **inside the seal writer's
own drain loop**. So if the loop exits, the alarm's input stops moving and the
alarm reads a flat, healthy zero — the failure silences its own detector. A
whole session running on disk-spill instead of the writer is invisible.

A panic is loud (`panic = "abort"`), so the dangerous case is the quiet one: a
clean exit of the task, or a persistently full channel. Neither aborts, and
neither moves the alarmed counter.

#### Finding 2 — slot exhaustion is permanent, latched, and unalarmed

`AGGREGATOR_MAX_SLOTS` is 25,000 against an authorized universe of ~24,600, and
slots are **never released** — a workspace scan for `remove` / `swap_remove` /
`release` / `reclaim` across the aggregator returns zero. The per-minute ATM
re-fit is additive by design, so the count only climbs through the session.

Past the cap, `slot_index` returns `None` and that instrument **derives no
candles for the process lifetime**. Ticks are still persisted — only the candle
is lost — so nothing else reports it.

Detection today is one latched log line (`exhausted_logged` means only the FIRST
instrument ever logs, so instruments 2..N are silent) plus
`tv_aggregator_slot_exhausted_total`, which is EMF-selected and on the dashboard
but in **no alarm**. There is also no gauge of slots-in-use, so the approach to
the cap is unobservable: an operator can see the crash after it happened, on a
chart, and never see it coming.

#### What this authorizes — family (5) gains a SIXTEENTH and SEVENTEENTH signal

| Alarm | Metric | Fires when | Why it is not noise |
|---|---|---|---|
| `tv-<env>-seal-writer-rescued` | `tv_dhan_feed_seals_rescued_total` | `Sum >= 1` in a period | A seal that could not reach the writer went to disk. On a healthy lane this is exactly zero; non-zero means the writer is dead, saturated, or the disk tier is carrying the session. |
| `tv-<env>-aggregator-slots-exhausted` | `tv_aggregator_slot_exhausted_total` | `Sum >= 1` in a period | An instrument was permanently refused a candle slot. Zero on a healthy lane by construction — the cap is above the authorized universe, so any non-zero means the universe outgrew it. |

Both `treat_missing_data = notBreaching` and ungated: the box is stopped
overnight so no-data is the normal off-hours state, and each reports a DEFECT
rather than silence. The dark-lane case is already owned by
`dhan-no-ticks-flowing` (§2.3b-i). Neither takes `ok_actions` — a counter aging
out of its window is not a repair, and in both cases the loss already happened.

**Threshold 1, not a rate.** Both are zero-on-a-healthy-lane by construction,
so there is no baseline to calibrate against and any non-zero reading is the
event itself. A rate threshold here would be a number invented and then
presented as a measurement.

#### Honest cost

`tv_aggregator_slot_exhausted_total` is already EMF-selected, so it is +1 alarm
≈ $0.10/mo. `tv_dhan_feed_seals_rescued_total` is +1 EMF name ≈ $0.30 and +1
alarm ≈ $0.10. **~$0.50/mo total.**

§2.3k put a maximal month at ~$120.68 and the depth-discriminator change earlier
today took it to ~$122.38; this takes it to **~$122.88** against the automatic
`STOP_EC2_INSTANCES` line at **$117.00**. The live account is far below it
(August MTD $48.87, forecast $61.51, measured 2026-08-25), so nothing fires
today, but the maximal-month arithmetic is now ~$5.88 past that line and widens
with each addition. Unchanged levers, neither taken here: the already-approved
Quote 10 Elastic IP release (−$3.60/mo), or an operator decision on
`limit_amount`.

#### ⚠ What this does NOT do (Rule 11)

An alarm on slot exhaustion does not create slots, and an alarm on rescued seals
does not restart the writer. Neither changes the ~400 slots of headroom, and
neither adds the slots-in-use gauge that would let an operator see the cap
approaching rather than being told it was crossed. Both convert a permanent loss
visible only on a chart into one that reaches the operator — that is the entire
claim.

The seal writer still has **no respawn**. This makes its death detectable; it
does not make it survivable.

#### What a PR that violates §2.3l looks like (REJECT)

- Alarms either counter on a RATE rather than `>= 1` — both are zero on a
  healthy lane, so a rate threshold invents a baseline that does not exist.
- Gives either `ok_actions` (the loss already happened; a counter aging out is
  not a repair).
- Makes either `breaching` on missing data (pages every night and weekend).
- Adds a per-INSTRUMENT dimension to either (the §2.3 cardinality rule stands).
- Claims the seal writer is now resilient — it is detectable, not respawned.

### §2.3m — 2026-08-28: the socket that carries nothing, and the flush that stalls the drain

**The verbatim operator demand (2026-08-28, typed directly in-session — preserve
EXACTLY, expletives and typos included):**

> "see aagin why this amrket depth stuck and why the fuck it failed see ebcasue of all tehse issues evenw e will face websocket disocennct wbesocket reocnenct right mtoehrfucekr am i irght how will you provid eme the rela tiem rpoven gauarnteed assured solution mtoehrfucke rokay?"

> "I dont need any hallucination or illusion I just need the working guaranteed assurance solution"

This dated row is written BEFORE the terraform, per the rule-file-first law.

#### The operator is right about the mechanism, and the source says so in its own words

He asserted that the depth stall is what produces the WebSocket disconnects and
reconnects. Traced end to end, that is exactly what the code does — and
`dhan_feed_stack.rs` had already written the sentence down:

> "Called bare on this task it pins a tokio worker; on a 2-worker host that is
> HALF the runtime, and the worker it pins is shared with the WS read loops —
> which stop pumping pongs and get the socket dropped. Worse, the drain stops
> draining, so the 65,536-frame ring fills in ~13 s at 5,000 fps and every frame
> after that is refused."

`tick_persistence.rs` carries the other half:

> "A slow database therefore stalled the fold, filled the receive buffer, and
> Dhan — which skips a slow consumer forward to 'the latest available state'
> with no sequence number — discarded the intermediate ticks at THEIR side,
> invisibly."

**The tick writer was taken off the drain on 2026-08-25. The depth writer was
not** — verified by source scan: `DepthWriter` had no `offload` field and
`split_for_offload` returned zero occurrences in that file. And depth is the
writer that needed it more:

| | ticks | depth |
|---|---:|---:|
| rows per session (MEASURED 2026-08-24) | 64,349,753 | **1,530,651,649** |
| modelled rows/sec | — | **~63,800** |
| ILP `request_timeout` | 5,000 ms | 5,000 ms |
| flush on the frame-drain task | no, since 2026-08-25 | **yes, until today** |

So the largest payload in the process ran a synchronous 5-second-timeout flush
on the tick fold's own task, up to ~5 times a second. That is the coupling, and
no amount of provisioned disk throughput removes it, because it is structural
rather than a matter of speed.

#### What was done about it

`DepthWriter::split_for_offload` — the same split, applied to the bigger writer,
with the same bounds and the same names so the two paths cannot drift into
different failure semantics: a `DEPTH_FLUSH_QUEUE_DEPTH` of 4, a
`MAX_DEPTH_RETAINED_FLUSH_SPANS` of 2, a `MAX_DEPTH_PRODUCER_BUFFER_BYTES`
const-asserted at or below half the questdb-rs wedge, and a `QueueFull` arm that
reports `Ok` because the rows are still held. Nine tests pin the semantics; the
one that matters most is that backpressure is never reported as loss.

#### The alarm this section authorizes — family (5) gains a SIXTEENTH signal

`tv-<env>-errcode-ws-gap-02-swap-emptied-socket`. An at-the-money depth swap
that unsubscribed the old contract and then failed to subscribe the new one
leaves that socket **carrying nothing** — and it stays transport-healthy: it
keeps ponging, `tv_dhan_ws_alive_connections` counts it, and
`dhan-no-ticks-flowing` reads the WHOLE lane, where fifteen other sockets are
flowing and the last tick is always ~1 s old. Every alarm in family (5) reads
green through it.

**Scoped by `$.source`, never by the bare code**, and that is the load-bearing
detail: `WS-GAP-02` is also emitted by the per-minute ATM top-up path, which
stops at its wire budget without emptying anything and is an ordinary,
non-paging outcome. A bare-code filter would page on routine top-up exhaustion
every minute — the RISK-GAP-03 noise trap, on a hotter path. Two conditions plus
the level, matching the shape §2.3f settled on after finding that filter-pattern
syntax is validated only by the real `PutMetricFilter` call at apply time.

`ok_recovery = true`, unlike the xverify entries: the same code arm schedules a
redial and the retained set already names the NEW contract, so a return to OK is
the remediation working and is worth telling.

**A second, quieter fix in the same arm.** The emptied-socket condition was
computed as `unsubscribe_succeeded && subscribe.is_some()` — which misses the
case where the unsubscribe **timed out**. A budget elapsing does not mean the
frame was never written; a slow flush that lands afterwards leaves Dhan holding
nothing, which is precisely the case the remediation exists for. Worse, the new
counter would have read **zero while a socket was empty** — a false-OK on the
very signal added to make this visible. A timed-out unsubscribe is now treated
as possibly-landed, because the two errors are not symmetric: a redial is
idempotent and costs a backoff, while an empty socket delivers nothing for the
rest of the session.

#### ⚠ Honest cost, and it crosses the automatic action line further

One log-filter alarm on an existing metric-filter lane ≈ **$0.10/mo**. No new
EMF name, no user-data byte — the two other new safety counters were
deliberately NOT shipped as EMF names, because their emit sites already carry
`WS-SPILL-01` and `HOT-PATH-02`, both of which are already filtered and alarmed.
Three names' worth of coverage for one name's cost.

§2.3k put a maximal month at **~$120.68** against the budget's automatic
`STOP_EC2_INSTANCES` line of **$117.00** (90% of the $130 `limit_amount`). This
takes it to **~$120.78 — about $3.78 above the line that switches the trading
box off** in a maximal month. The live account is nowhere near it (August MTD
$48.87, forecast $61.51, measured 2026-08-25), so nothing fires today, but the
gap widens with every addition. The levers are unchanged and neither is taken
here: the already-approved Quote 10 Elastic IP release (−$3.60/mo, which alone
would put the maximal month back under the line), or an operator decision on
`limit_amount`.

#### ⚠ What this does NOT do (Rule 11)

- **Taking the depth flush off the drain does not make QuestDB faster.** It
  removes the drain from the flush's critical path, so a database stall stops
  becoming upstream tick loss. The stall itself is untouched, and the 24×
  row volume that causes it is untouched.
- **The bounded queue is a shock absorber, not storage.** Four batches absorb
  roughly 600 ms of stall at the modelled depth flush cadence. A longer stall
  surfaces as backpressure, and past two retained spans the producer rescues to
  the depth spill tier — durable and re-ingestable, but not in QuestDB.
- **The alarm reports an emptied socket; it does not prevent one.** Detection
  latency is the CloudWatch window.
- **None of this has run against a live market open.** The depth lane has never
  faced one, and that residual stands from §2.3b.

#### What a PR that violates §2.3m looks like (REJECT)

- Filters the emptied-socket alarm on `WS-GAP-02` alone, or drops the `$.source`
  condition — it pages on every routine top-up budget exhaustion.
- Re-couples the depth flush to the frame-drain task, or removes
  `split_for_offload` from either writer.
- Makes the depth `QueueFull` arm report a failure — backpressure is not loss,
  and reporting it as loss decays feed health for rows that are still held.
- Removes either producer bound (`MAX_DEPTH_RETAINED_FLUSH_SPANS`,
  `MAX_DEPTH_PRODUCER_BUFFER_BYTES`) — "keep appending while the writer is
  behind" without a bound is an unbounded memory path in a costume.
- Uses a blocking `send` on the hand-off queue, which re-creates the exact
  coupling one queue further out.

### §2.3n — 2026-08-28: the shutdown that abandons a writer's queue was reported by a log line and nothing else

**The verbatim operator demand (2026-08-28, typed directly in-session — preserve
EXACTLY, typos included):**

> "hwo will youa cheiev always O(1) alogn with nto even a signel ticks loss across the entire codebase and workspace"

> "Always ensure auditing logging dashboard vislualisign extremeims analyzing easil yaccessibel eveyrhtign also"

> "I dont need any hallucination or illusion I just need the working guaranteed assurance solution"

Given while an adversarial sweep of the writer-offload path was running. The
finding below came out of that sweep and was not known when the session
started. This dated row is the rule-file edit §3 requires before any new
Dhan-scoped page, recorded BEFORE the terraform.

#### The gap

The lane joins two writer threads at shutdown — ticks and depth. Either join
can time out, and when it does the thread is deliberately detached rather than
joined (joining after a timeout is the unbounded wait the grace exists to
avoid). The batches that thread was holding — up to `FLUSH_QUEUE_DEPTH` plus
the one in flight, at the depth writer's 10,000-row threshold roughly **50,000
rows** — then die with the process.

**No counter moved.** `tv_ticks_dropped_total` and `tv_depth_rows_dropped_total`
are the only alarmed loss series on this path, and both are incremented by the
WRITER THREAD on rows it actually refused. A thread that never got to run its
drain increments neither. So the single largest loss the shutdown path can
produce was reported by free text in a log and by nothing an alarm can read.

The depth side was worse in a second way: its join arm ended in `drop(handle)`,
which detaches unconditionally, so a depth writer that finished-but-PANICKED
looked identical to a clean one. The tick path has always joined and checked.
Depth carries 24x the row volume and had the weaker check.

#### What this authorizes — family (5) gains an EIGHTEENTH signal

`tv-<env>-offload-writer-shutdown-incomplete` on
`tv_offload_writer_shutdown_incomplete_total`, `Sum >= 1` in a period,
`treat_missing_data = notBreaching`, `ok_actions = []`.

**An EPISODE counter, not a row count, and that is deliberate.** When the join
times out we do not know how many rows were in flight: the queue belongs to a
thread we have just given up on, and the batches are ILP buffers whose row
counts were consumed when they were sent. Publishing a guessed number into a
loss counter would be a fabricated figure inside the one metric whose purpose
is to stop fabrication. So it counts EPISODES — "a writer shutdown was
abandoned" — once, per writer.

`notBreaching` because the box is stopped overnight and no-data is the normal
off-hours state; the dark-lane case is already owned by `dhan-no-ticks-flowing`
(§2.3b-i). `ok_actions = []` because this fires once at process exit — a return
to OK is the datapoint aging out, never a repair. The rows do not come back.

The `writer` label separates ticks from depth. The EMF processor folds label
values into one summed series per host, so the alarm sees the total — the right
shape here, because either writer abandoning its queue calls for the same
operator action: check the spill directory before the next session.

#### Also fixed in the same change, and not alarmed

- **The lane shutdown budget was arithmetically wrong in the spawn-failure
  fallback.** Both offload spawns are allowed to fail and fall back to the
  synchronous writer. In that mode the tail flushes depth and then ticks, each
  able to block one full ILP request timeout, BEFORE the join deadline starts:
  5 + 5 + 12 = 22 inside a 20 s budget, so the outer timer won and abandoned the
  lane task mid-join — the exact failure the compile-time assert exists to
  prevent, arriving through the door it was not watching. The budget is now 30 s
  (still far inside the unit's `TimeoutStopSec=120`) and the margin is DERIVED
  from `ILP_REQUEST_TIMEOUT_SECS` rather than being a hardcoded 5 that happened
  to equal it.
- **A false loss claim in the depth-flush log.** It read "those depth rows are
  lost" on every error. Under the offload writer the only errors reachable are
  the two that RESCUED the rows to the depth spill tier. An operator triaging
  that line was told data was gone while a re-ingestable file existed.

#### ⚠ Honest cost, and it moves further past the automatic action line

+1 EMF name ≈ $0.30/mo, +1 alarm ≈ $0.10/mo ⇒ **~$0.40/mo**, no user-data byte.

§2.3l put a maximal month at **~$122.88** against the budget's automatic
`STOP_EC2_INSTANCES` line of **$117.00** (90% of the $130 `limit_amount`). This
takes it to **~$123.28 — about $6.28 above the line that switches the trading
box off** in a maximal month, and against the operator's $125 hard cap
(Quote 18) it now has under $2 of room. The live account is far below it
(August MTD $48.87, forecast $61.51, measured 2026-08-25), so nothing fires
today, but the maximal-month arithmetic is now the tightest it has been and
**the next addition of any size must come with a lever, not just a cost note.**
The levers are unchanged and neither is taken here: the already-approved
Quote 10 Elastic IP release (−$3.60/mo, which alone would put the maximal month
back under both lines), or an operator decision on `limit_amount`.

#### ⚠ What this does NOT do (Rule 11)

An alarm on an abandoned shutdown does not save the queue. It converts the
largest silent loss on the shutdown path into one the operator is told about —
that is the entire claim. It does not shorten the drain, does not add a respawn,
and does not make the rows recoverable: an abandoned queue is gone, and the
alarm's job is only to stop that being invisible.

It also does not address the finding that `discard_pending` performs a
synchronous spill write of up to 32 MiB ON the frame-drain task in the
`WidthCapped` and `SinkGone` arms. Moving the ILP flush off the drain removed a
5 s network round trip and left a filesystem write in its place, on the same
volume QuestDB is stalling. Smaller constant, same mechanism. Recorded here as
a known open item rather than quietly fixed, because the safe shape for it is a
design decision, not an edit.

#### What a PR that violates §2.3n looks like (REJECT)

- Turns the episode counter into a guessed ROW count (a fabricated figure in a
  loss metric is the thing the counter exists to avoid).
- Gives the alarm `ok_actions` (the rows do not come back; an aged-out
  datapoint is not a repair).
- Makes it `breaching` on missing data (pages every night and weekend).
- Restores `drop(handle)` on the depth join, hiding a panicking depth writer.
- Re-hardcodes `SHUTDOWN_GRACE_MARGIN_SECS` instead of deriving it from the ILP
  timeout.
- Raises `DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS` without re-checking it against
  the unit's `TimeoutStopSec` by hand — a `.service` file is invisible to the
  compile-time assert, which is that guard's honest limit.

### §2.3o — 2026-08-28 INCIDENT: five pagers, one cascade, and a discriminator that could not discriminate

**The operator's report (2026-08-28, a Telegram screenshot, typed verbatim):**

> "See why these Many errors bro can you fix and resolev all of these dude okay?"

The screenshot shows the same five alerts repeating on a ~7-minute cycle from
15:23 IST: *Market data persistence loss · Ticks dropped · Questdb wal probe
failed · Ticks spilling · Ws ring full.*

#### What actually happened — MEASURED from CloudWatch, not inferred

| Series | Session total (09:00–17:30 IST) |
|---|---:|
| `tv_dhan_feed_ingest_ticks_total` | **82,254,468** |
| `tv_ticks_dropped_total` | 308,818 |
| `tv_ticks_spilled_total` | **308,818 — identical** |
| `tv_dhan_ws_wal_dropped_total` | **0** |
| `tv_dhan_ws_ring_full_total` | 508,598 |
| `tv_questdb_wal_suspended_tables` | 0 (max) |
| `tv_wal_suspension_probe_failed_total` | 44 |
| `tv_depth_rows_dropped_total` | 104,540 |
| `tv_depth_rows_spilled_total` | **no series at all** |
| `tv_aggregator_tick_refused_total` | **5,748,026 (7.0% of ingest)** |
| `tv_spill_dir_free_bytes` | 314 GB → **176 GB** (−138 GB in one session) |

**On the TICK path nothing was permanently lost.** `dropped` and `spilled` are
equal to the row, which is the documented proof that every dropped tick was
rescued to the spill tier and is re-ingestable. `tv_dhan_ws_wal_dropped_total`
= 0 means the durable floor held for the entire session: no frame was ever
un-captured. So five alarms fired repeatedly, and the tick loss they implied
was **zero**.

#### The real defect the incident exposed

**`tv_depth_rows_spilled_total` published NO SERIES while its sibling counted
104,540 drops.** The CloudWatch agent computes a counter as the delta between
consecutive samples and drops the first sample of a series it has never seen,
so a counter that is never incremented is never registered — and an ABSENT
series is indistinguishable from a healthy zero one.

The tick side seeds both names at construction, which is exactly why the tick
verdict above is provable. The depth side seeded only `dropped`. So for depth
the answer is **Unknown**: 104,540 rows either all rescued or all permanently
gone, and the instrument built to tell those apart was silent in the one case
it exists for. That is the false-OK class, arriving inside the fix that was
supposed to end it — the discriminators were ADDED on 2026-08-28 (the FOURTH
count bump in `cloudwatch_app_alarms_wiring`) and shipped unseeded.

**FIXED the same day:** `register_depth_drop_baseline` now seeds
`tv_depth_rows_spilled_total`, both `stage` values of
`tv_depth_spill_write_errors_total`, and both of
`tv_depth_persist_errors_total`; `register_drop_baseline` gains
`tv_tick_persist_errors_total`. Pinned by
`crates/storage/tests/loss_series_seeding_guard.rs` (4 tests), including one
that asserts a drop counter is never seeded without its rescue discriminator,
and one that bite-proves the extractor cannot mistake a real `increment(n)`
for a seed. Verified by reverting the fix: 2 tests fail, restored: 4 pass.

#### ⚠ What is NOT fixed, and is the actual cause of the storm

1. **Disk burn.** 138 GB consumed in one session. That is the engine of the
   whole cascade: as free space falls, QuestDB's merges slow, ILP flushes reach
   their timeout, the frame ring fills (508,598 times today), rows spill — and
   spilling writes MORE to the same disk. It is a positive feedback loop, and
   no alarm added here slows it.
2. **Five pagers for one cascade, on a ~7-minute repeat.** Each alarm is
   individually correct and they describe one event. This file already records
   that ALARM records are never deduplicated between CloudWatch and Telegram
   (only OK is), so a repeating condition pages once per evaluation. Fixing it
   means editing a law written in capitals in the Lambda and belongs in its own
   change with its own row.
3. **`tv_aggregator_tick_refused_total` = 5,748,026 — 7.0% of the session.**
   The 2026-08-28 note recording this family's first EMF selection measured
   2.41% on 2026-08-27 and reported the out-of-band class fixed. It is now
   roughly three times higher. Unexplained, and recorded rather than guessed at.
4. **`tv_wal_suspension_probe_failed_total` = 44.** The gauge the
   `questdb-wal-suspended` alarm reads showed 0 all session while its producer
   failed 44 times. §2.3g states the principle: alarming a gauge whose producer
   can fail is alarming a lie. The probe now fails loud rather than open, so
   the 44 are visible — but a zero on that gauge today is worth less than it
   looks.

#### What a PR that violates §2.3o looks like (REJECT)

- Ships a drop counter without seeding its rescue discriminator in the same
  change (the guard fails the build; do not weaken it to pass).
- Reads an absent CloudWatch series as a zero anywhere — in an alarm, a
  dashboard, a runbook or a report.
- Reports the 104,540 depth rows of 2026-08-28 as either lost or rescued. The
  data to decide does not exist for that session and never will; the fix makes
  the NEXT session answerable, not this one.
- Claims the tick path lost data on 2026-08-28. It did not: dropped equals
  spilled, and the WAL drop count is zero.

#### §2.3o-i — 2026-08-28, SAME DAY: two claims in §2.3o above are WRONG, and both were mine

Written hours after §2.3o, from three parallel adversarial investigations that
were commissioned to check it. Recorded as corrections rather than edits,
because in both cases the reasoning that produced the wrong claim is the
reusable part.

**CORRECTION 1 — "the aggregator refusal rate is now roughly three times
higher" is FALSE. It is not a regression; it is a first reading.**

§2.3o records `tv_aggregator_tick_refused_total` = 5,748,026 (7.0% of ingest)
and compares it to "2.41% measured yesterday". Those are **two different
metrics**. The 2.41% figure comes from this file's own COST NOTE of
2026-08-28, which states in the same breath that
`list-metrics --metric-name tv_aggregator_tick_refused_total` returned
`{"Metrics": []}` — the family had NEVER reached CloudWatch — and reports
2,008,916 as ticks hard-refused **on timestamp**, read from a log line. That
is one reason of a DIFFERENT counter (`tv_dhan_feed_ingest_refused_total`).

The family was EMF-selected in that same change, so **6.99% is its
first-ever CloudWatch reading — a baseline, not a jump.** The arithmetic
agrees: `untraded_timestamp` (825,783, measured 2026-08-26) plus
`out_of_band_timestamp` (2,008,916, measured 2026-08-27) is already 3.4% of
the prior session, above 2.41%, and both predate today.

The lesson is specific: **a first measurement is not a trend.** Comparing a
new series' opening value against a differently-scoped number from a log line
manufactures a regression out of nothing, and that is what §2.3o did.

**CORRECTION 2 — the feedback loop in §2.3o has a false leg. Spilling does
NOT meaningfully accelerate the disk fill.**

§2.3o says rows spill to disk "which writes MORE to the same disk — a positive
feedback loop". Measured: the tick spill for the whole session is
308,818 rows × ~140 B ≈ **43 MB**, against a 138 GB fill. Three orders of
magnitude apart. The rescue tier is not the problem; it is the part that
worked, cheaply.

**Where the 138 GB actually goes** (derived from the DDL row widths and the
module headers; the depth row count is Assumed at the 2026-08-24 measured
order):

| Writer | Bytes/session | Share |
|---|---:|---:|
| `market_depth` (1.53 e9 rows × 72 B) | ~110 GB | ~80% |
| 24 candle tables (112 B/row) | ~35–55 GB upper-bounded | — |
| `ticks` (82.25 M × 144 B) | ~11.8 GB | ~9% |
| raw frame WAL | bounded ~40 GB active / 50 GB archive | — |
| tick + depth spill | ~43 MB + capped | ~0.03% |

**Depth is 80% of the burn, one row per level per snapshot.** No complexity
change and no alarm touches it.

**Three structural findings from the same investigation, none of them fixed:**

1. **The archiver cannot win, by construction.** `retention_class` puts
   `market_depth` and `ticks` in classes whose floor is ONE DAY, and the SQL is
   `minTimestamp < dateadd('d', -1, now())` — so today's partitions are
   unreachable while today is being written. `pressure_config` only ever
   `min()`s windows DOWN and cannot go below that floor. The maximum a
   mid-session pressure episode can reclaim is yesterday's data, which the
   post-market daily leg already dropped. On any day whose predecessor was
   archived, a pressure episode reclaims **zero**, burns its passes and fires
   `STORAGE-GAP-05`. The floor is one session; the inflow is one session. The
   partitions are already `PARTITION BY HOUR`, so an hour-granular floor would
   turn "never wins" into "runs six times a session".
2. **There is no backpressure from disk pressure to the subscription set.**
   `ShedLevel::allows_ticks()` is `const { true }` — no shed level reduces
   ticks, and nothing reduces the subscribed universe. Both safety nets are
   FRACTIONAL (`SHED_INLINE_DEPTH_BELOW_FREE = 0.15`, `pressure_high_water_pct
   = 75`), i.e. 48 GB and 81 GB free on this volume. The session closed at
   **176 GB free — 55%** — so neither armed, and neither can arm for about
   another 0.9 of a session. Growing the disk moves both further away in GB at
   a constant burn rate: **runway measured in sessions is free ÷ 138 GB, and it
   is 1.3.** A fractional threshold on a growing disk is a threshold that
   quietly stops meaning anything.
3. **No per-table byte metric exists anywhere.** A scan for a QuestDB
   `diskSize`/partition-size gauge returns nothing, so every attribution above
   is derived rather than observed. Nobody can say from telemetry where the
   138 GB went.

**What a PR that violates §2.3o-i looks like (REJECT):** compares a metric's
first CloudWatch reading against a differently-scoped figure and calls the
difference a regression; describes the tick spill tier as a driver of disk
growth; raises the volume size as a fix for the burn without also making the
shed thresholds absolute (a bigger disk makes a fractional net arm later, not
sooner); or claims the pressure archiver protects a session while its floor
remains one day.

### §2.3p — 2026-09-02: the app process had no memory ceiling, and its own early warning could not reach the line it watched

**The verbatim operator authorization (2026-09-02, typed directly in-session —
preserve EXACTLY, typos included):**

> "go ahead and fix the remaining open findings dude okay?"

> "Once fixed finished and resolved merge and deploy it also dude okay?"

Given in DIRECT response to the Second Sweep Ledger, whose row for this item
read verbatim **"No memory ceiling on the app process … infra + $0.10/mo"** —
a list of fourteen open findings each carrying a cost or a decision. That is
the §28.2-shape general go-ahead selecting an enumerated list, and the $0.10
was on the row he approved. This dated section is the rule-file edit §3
demands before any new Dhan-scoped page, recorded BEFORE the terraform.

#### What was measured in source, not inferred

| Claim | Evidence |
|---|---|
| The unit set NO memory directive | `grep -n 'Memory\|OOMScore' deploy/systemd/tickvault.service` → **0 lines** before this change; only `LimitNOFILE` / `LimitNPROC` |
| QuestDB is ranked safer than the app under OOM | `deploy/docker/docker-compose.yml:196` `oom_score_adj: -500`; the app's cgroup carried the kernel default `0`, so the kernel would kill the **app first** |
| QuestDB's committed ceiling | `mem_limit: ${QDB_MEM_LIMIT:-12g}` + `shm_size: 512m` = **12.5 GiB** |
| Sidecar ceilings | `tv-alloy` `mem_limit: 384m` + `tv-loki` `mem_limit: 256m` ≈ **0.64 GiB** |
| What RESOURCE-02 compared against | `resource_monitor.rs::resolve_memory_ceiling`: cgroup `memory.max` first, `/proc/meminfo` `MemTotal` as fallback; with no directive the cgroup reads `max`, so the denominator was **~31.3 GiB** and its 80% line (~25 GiB) sat above anything the kernel tolerates with QuestDB resident |
| RESOURCE-02 reached a page | `LOG_SINK_ONLY_EXEMPT` in `error_code_alarm_coverage_guard.rs` listed it; `error-code-alarms.tf` had no entry — **log-sink-only** |
| Emit sites | two: `resource_monitor.rs` (the RSS-vs-ceiling arm) and `subsystem_memory.rs` (the sampler-died respawn arm); both are memory-observability failures, so a plain coded filter is correct |
| The unit has ONE copy | `user-data.sh.tftpl:243` is a plain `cp` from the repo checkout — not templated, so nothing can drift from the file in git |

**Why the kill order was backwards.** The app is the ONLY tick-capture path.
An upstream tick that arrives while it is dead is never resent — Dhan's feed
carries no sequence number and skips a slow consumer forward. QuestDB is
recoverable: a stalled or restarted database is absorbed by the tick and
depth spill tiers and re-ingested. Under host exhaustion the kernel picks the
largest unprotected RSS, which after QuestDB's `-500` was this process.

#### What is authorized

1. **`OOMScoreAdjust=-900` + `MemoryHigh=15G` on `deploy/systemd/tickvault.service`, and explicitly NO `MemoryMax=`.** `MemoryHigh` is a THROTTLE — past it the kernel reclaims aggressively and slows allocation. A `MemoryMax` would turn a spike into an OOM kill of the one process whose death loses ticks, which is the outcome the ranking exists to prevent. Arithmetic for 15G: host usable ~31.3 GiB − QuestDB 12.5 − sidecars 0.64 − OS/page-cache floor 2–4 = **~14.2–16.2 GiB** for the app; its measured envelope is 4.3–11.2 GB, so 15 GiB sits above the envelope and below exhaustion. AL2023 runs no swap, so "before swap" and "before OOM" are the same line. Rollback is a two-line drop-in recorded in the unit itself.

2. **ONE new alarm — family (5) gains a NINETEENTH signal:** `tv-<env>-errcode-resource-02`, a plain coded log-filter on `error-code-alarms.tf`, `eval 3 / dta 1 / period 300`, `ok_recovery = true` (the monitor re-evaluates every cycle and re-emits while RSS stays over the line, so an OK genuinely tracks recovery — the DH-901 shape, not the discrete proc-01 shape). `RESOURCE-02` leaves `LOG_SINK_ONLY_EXEMPT` in the same change, which that guard's stale-entry test requires. The Telegram phrase: *"The trading app's memory is close to its ceiling — it will be slowed, not killed, but check what is growing."*

#### ⚠ Honest limit (Rule 11)

The alarm pages on a line that, before this change, could not be reached. What
makes it reachable is the resolver preferring a REAL ceiling: until this
batch, `resolve_memory_ceiling` read `memory.max` → `MemTotal`, and
`MemoryHigh` writes `memory.high`, not `memory.max` — so with only this unit
change the resolver would STILL see `max` and fall back to ~31 GiB. **Another
agent is changing the resolver to prefer `memory.high` in the same batch**;
this row records the alarm and the unit half, and the two must land together
or the page keeps measuring against the whole machine. Until the first live
session on the new unit, "RESOURCE-02 fires at 12 GiB" is what the code must
do, not what the box has reported.

The throttle does not stop growth either. A queue that grows without bound
under `MemoryHigh` is a slowed process that still loses ticks upstream; the
alarm converts that from invisible to a page, and the operator's action is a
restart in the next quiet window, not a wait.

#### ⚠ Honest cost, and the budget position

One log-filter alarm ≈ **+$0.10/mo**, no new EMF name, no user-data byte.

§2.3n put a maximal month at **~$123.28** against the budget's automatic
`STOP_EC2_INSTANCES` line of **$117.00** (90% of the $130 `limit_amount`).
This takes it to **~$123.38 — about $6.38 above the line that switches the
trading box off** in a maximal month, and under $2 of room against the
operator's $125 hard cap (Quote 18). The live account is far below it (August
MTD $48.87, forecast $61.51, measured 2026-08-25), so nothing fires today.
§2.3n said the next addition "must come with a lever, not just a cost note".
**The lever is NOT taken here** — the already-approved Quote 10 Elastic IP
release (−$3.60/mo) and the `limit_amount` decision both remain the
operator's, and this row does not pretend a ten-cent alarm changed that. It
is stated rather than absorbed.

#### What a PR that violates §2.3p looks like (REJECT)

- Adds `MemoryMax=` to the unit (turns a spike into a kill of the only
  tick-capture process).
- Removes `OOMScoreAdjust=-900`, or sets it at or above QuestDB's `-500`
  (restores the backwards kill order).
- Ships the alarm without this dated row, or re-adds `RESOURCE-02` to
  `LOG_SINK_ONLY_EXEMPT` while the terraform entry stands.
- Gives the alarm `ok_recovery = false` on the strength of the proc-01
  precedent — this emitter repeats while the condition persists, so its OK
  is real.
- Claims the page fires at 15 GiB before the resolver reads `memory.high`.

---

## ⚠ CORRECTED 2026-09-02 — every cost note above reasons from a $130 ceiling. The live limit is $150, and the arithmetic has been wrong in the ALARMING direction for eight days.

**No authorization is claimed or needed here.** This records a live
measurement and retires a dead constraint; it changes no alarm, adds no
metric, and spends no money.

**Read live from the account, 2026-09-02:**

| Reading | Value | Source |
|---|---|---|
| `limit_amount` | **$150.00** | `budgets describe-budgets` |
| 90% `STOP_EC2_INSTANCES` action line | **$135.00** | derived |
| Actual spend, month to date | **$2.77** | ActualSpend (2 days in) |
| AWS forecast, September | **$114.01** | ForecastedSpend |

**What the sections above say instead.** Fifteen separate statements in
this file reason from `$130` / a `$117.00` action line, and each new cost
note inherited the figure from the one before it rather than re-reading
the budget:

| Section | Claim | Against the real $135 line |
|---|---|---|
| §2.3j | "~$120.58 — about $3.58 above the line" | **$14.42 UNDER** |
| §2.3l | "~$122.88 … ~$5.88 past that line" | **$12.12 UNDER** |
| §2.3m | "~$120.78 — about $3.78 above" | **$14.22 UNDER** |
| §2.3n | "~$123.28 … under $2 of room" against the $125 cap | the cap is **$150** (Quote 19) |
| §2.3p | "~$123.38 … about $6.38 above" | **$11.62 UNDER** |

The ceiling moved to $150 on **2026-08-25** — the same day, and in the same
`daily-universe-scope-expansion-2026-05-27.md` Quote 19, that authorized
the EBS grow. The operator's words were *"even if we need to reach the max
150 usd also let su gfo ahead dude but ensure to achieve alwyas O(1)"*.
Every cost note written after that date still costed against $130.

**Why this is worth a correction rather than an edit-in-place.** These are
stale in the REASSURING direction's opposite — they overstate the danger —
and this file's own header warns that a partial or stale disclosure reads
exactly like a current one. §2.3n went further and set a policy from the
wrong number: *"the next addition of any size must come with a lever, not
just a cost note."* That policy was adopted against a margin that did not
exist. Real observability gaps have been left open on it — §2.3g's
`tv_wal_suspension_probe_failed_total`, §2.3d's headroom gauge, §2.3f's
`tv_dhan_feed_xverify_runs_total` — each costing about $0.30/mo against a
forecast that is **$21 below** the action line.

**This is the second constraint in this file to expire and keep being
quoted.** The first was the user-data byte budget: §2.3d-ii measured
"zero bytes free", the boot template was restructured three days later,
and §2.3d / §2.3f / §2.3g each went on citing the dead figure as a live
blocker until the 2026-08-26 note retired it. The pattern is identical and
so is the lesson that note recorded: **a measurement carries a date, and a
measurement quoted from a quote is not a measurement.** A budget limit is
one `budgets describe-budgets` call; a byte count is one guard run. Both
must be re-run at the moment of writing, not inherited from the section
above.

**Not claimed:** that any of the deferred metrics should now be shipped.
Each is still an operator decision with its own cost line — what changes
is only that "we cannot afford it, we are past the shutdown line" has
stopped being a true reason. The recorded per-section arithmetic is left
in place as the audit trail; this row is the correction to all of it.
