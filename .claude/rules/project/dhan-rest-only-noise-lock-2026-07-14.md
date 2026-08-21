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
