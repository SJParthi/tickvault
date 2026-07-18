---
paths:
  - "crates/app/src/option_chain_1m_boot.rs"
  - "crates/app/src/spot_1m_rest_boot.rs"
  - "crates/common/src/constants.rs"
---

# Cross-Source Chain Coverage + Dhan Chain Capture Hardening (Operator Directive 2026-07-14)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `dhan-rest-only-noise-lock-2026-07-14.md` (§2.1 carries the dated directive
> this file implements; the family-(2) page-set extension lives THERE) >
> `no-rest-except-live-feed-2026-06-27.md` §8 (the chain grant — scope,
> cadence, tables, budgets UNCHANGED by this file) >
> `rest-1m-pipeline-error-codes.md` (the CHAIN-02/SPOT1M-01 runbook index —
> its §2c end paragraph points here) > this file.
> **Scope:** the Dhan per-minute chain leg's bounded retry + decision
> ceiling, the per-underlying not-served pager (the #1537 Groww mirror), the
> strike-ladder shrink watermark, the loud trading-day-flip exits, and the
> Dhan↔Groww cross-source coverage contract.
> **Companion code:** `crates/app/src/option_chain_1m_boot.rs`
> (`chain_retry_allowed`, `retry_launch_elapsed_ms`, `UnderlyingServedTracker`,
> `LadderWatermark`, `ladder_shrunk`, `record_chain_underlying_verdicts`),
> `crates/app/src/spot_1m_rest_boot.rs` (the two spot flip-exit sites),
> `crates/common/src/constants.rs` (`CHAIN_1M_DECISION_CEILING_SECS`,
> `CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD`),
> `crates/core/src/notification/events.rs` (`Chain1mUnderlyingNotServed`,
> `Chain1mUnderlyingServedRecovered`).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The directive + the incident (why this exists)

**Coordinator-relayed operator directive 2026-07-14 (verbatim intent, labeled
as such — the groww-scope §38.0-Context-3 convention):** *"make the Dhan
option-chain capture complete and precise, cross-cover the Groww gaps, and be
loud on any empty or partial chain — never a silent gap."*

**The motivating incident (2026-07-14, Verified):** Groww stopped serving the
same-day-expiring NIFTY chain at 14:54 IST (2xx / zero strikes) while
BANKNIFTY + SENSEX kept working — `ok=2/empty=1` all afternoon, and the
`ok == 0` escalation edge (`chain_minute_fully_failed`) paged NOBODY. The
Dhan chain leg carried the IDENTICAL blind spot. PR #1537 closed the Groww
side; this contract closes the Dhan side AND records the cross-source
coverage doctrine so a single-feed gap is understood as covered-by-the-sibling
rather than papered over.

---

## §1. The cross-source coverage contract (Dhan ↔ Groww)

> Both feeds' per-minute REST legs write the SAME tables (`spot_1m_rest`,
> `option_chain_1m`) with `feed` in every DEDUP key
> (option_chain_1m_persistence.rs:77-78; spot_1m_rest_persistence.rs:46) — a
> Dhan row and a Groww row for the same logical
> (minute, underlying, expiry, strike, leg) are DISTINCT rows by construction.
> **Nothing is ever overwritten, cross-copied, or synthesized across feeds**
> (Verified: no cross-source fill path exists — each leg's backfill/sweep
> re-fetches from its OWN vendor only). A per-minute gap in feed X is COVERED
> by feed Y's row when feed Y served that minute; coverage is a QUERY-TIME
> union keyed by CONTRACT IDENTITY — `underlying_symbol` +
> `exchange_segment` + `expiry` + `strike` + `leg` + `ts` — NEVER by native
> id (Dhan pins 13/25/51; Groww stamps FNV bit-62 ids — disjoint spaces; the
> futidx-4 join rule). Any future decision consumer (authorized under the §28
> boundary + §38.8 freshness gate) selects the FRESHEST NON-STALE feed's
> value per (underlying, minute) with explicit feed attribution — no
> blending, no averaging, no silent fallback; both stale ⇒ fail closed, no
> decision that minute. Both per-underlying not-served pagers NAME the
> sibling broker as the covering source with the honest MAY ("may still be
> coming in…") — serving states are independent; 2026-07-14 proved the
> Groww-cut-while-Dhan-served direction (735/735); the
> Dhan-cut-while-Groww-serves direction is the designed mirror. A BOTH-feeds
> gap is a real gap — absent in both feed slices, visible via both legs' own
> edges/pagers, never a fabricated row.

**Decision-spot corollary (recorded so the pacing work is never misread):**
any future (§28-gated) decision path reads the underlying spot from (a) the
Groww live-feed RAM structures and (b) the Dhan chain response's own
`data.last_price` (persisted per-row as `underlying_spot`). The Dhan
`spot_1m_rest` table is a RECORD/audit/parity artifact — Verified ZERO OHLC
readers on main (the only reader is the scoreboard's latency-column
fallback); §38.8 binds any future consumer to fail closed on staleness.

---

## §2. The retry + decision-ceiling contract (pacing — concurrency KEPT)

**The Dhan doc (option-chain rate-limit page, quoted 2026-07-14):** *"Rate
limit for Option Chain API is set to one unique request every 3 seconds. This
means you can fetch entire option chain for multiple different underlying
instrument or multiple expiries of same instrument concurrently every 3
seconds."* — the 3s bound is per UNIQUE (underlying, expiry) KEY; DISTINCT
underlyings concurrently is DOCUMENTED. The fire therefore KEEPS its JoinSet
concurrency (3 concurrent requests, one per distinct underlying), with the
same-key ≥3s bound enforced by `min_gap_wait_ms` through the per-underlying
`last_request_ms` stamps — ratcheted by
`ratchet_chain_fire_keeps_joinset_concurrency`; a future sequentialization
must be a deliberate, rule-edited change.

**The bounded retry (NEW):** the fire restructures into collect → retry →
process. First-pass `Empty` / transport-`Failed` outcomes get **at most ONE
retry per underlying per minute**, sequential over the (≤3, usually ≤1)
queue: each retry waits the same-key ≥3s gap and may LAUNCH only while the
REAL wall clock (including that wait) sits inside
`CHAIN_1M_DECISION_CEILING_SECS = 15` seconds after the minute close — the
operator's ≤~15s decision-data window. A refused retry is COUNTED
(`tv_chain1m_retry_total{outcome="skipped_ceiling"}`) + coded (CHAIN-02
`stage="retry_skipped_ceiling"` warn), never silent. NEVER retried: `Found`,
`Entitlement` (per-UNDERLYING precision, refuter round 2 LOW-1: an
entitlement reject is never retried FOR THAT UNDERLYING; sibling
underlyings' already-eligible `Empty`/`Failed` retries in that same minute
may still fire once each before the day-stop engages — bounded ≤2 extra
requests, once per day, each a harmless reject; a retry RETURNING
Entitlement is honored → EntitlementStop, CHAIN-01 owns the day), the
no-token minute, persist failures (our side).
The retry's outcome REPLACES the first pass's for ALL downstream processing —
`tv_chain1m_fetch_total` counts the minute-FINAL outcome per underlying;
`tv_chain1m_retry_total{outcome="recovered"|"still_failed"}` counts the retry
verdicts.

**Const-asserted schedule proofs** (constants.rs): 2.5s fallback + 10s
request timeout = 12.5s ≤ 15s (a worst-case timed-out first attempt leaves
its retry launchable); 15s launch + 20s budget = 35s < 60s (a ceiling-edge
retry never overruns the minute); the 3s same-key gap fits inside
15s − 2.5s. Threshold pin: `CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD ==
SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD` (= 10 — one not-served threshold
family across the REST legs).

**Live-probe honesty + the 429 safety net:** whether Dhan tolerates
concurrent-distinct in PRACTICE is verified the first live session; a 429
response feeds `record_429` → the shared Data-API limiter's `RpsTuner`
step-down to 2 rps (the existing safety net, `rest-1m-pipeline-error-codes.md`
§2f — NO limiter change in this contract). At the 2 rps floor under a 429
storm the REAL-clock ceiling gate may refuse retries — counted, never silent.

---

## §3. Stage taxonomy + triage (CHAIN-02 / SPOT1M-01 — no new ErrorCode)

| Stage | Level | Edge | Meaning |
|---|---|---|---|
| CHAIN-02 `underlying_not_served` | `error!` (field-less on `feed` — the Dhan-sites convention) | rising edge @ 10 counted minutes, once per underlying per episode | ONE underlying's chain came back empty/failed for `CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD` consecutive COUNTED minutes (each with ≥1 sibling OK) — the vendor is not serving THIS underlying. Pages the typed HIGH `Chain1mUnderlyingNotServed`; re-armed only by that underlying's own recovery (`Chain1mUnderlyingServedRecovered` Info). Counter `tv_chain1m_underlying_not_served_total{underlying}` (3 static label values) per counted minute. |
| CHAIN-02 `retry_skipped_ceiling` | `warn!` | per refused retry (≤3/minute, bounded) | a retry-eligible outcome could not launch inside the 15s decision ceiling — counted `skipped_ceiling`; the minute's final failure still rides the existing `minute_failed` accounting. |
| CHAIN-02 `ladder_shrunk` | `warn!`, edge-latched | first shrunk Found per episode; silent recovery clears the latch | a Found chain's strike count dropped below ⌈day_max/2⌉ of the per-underlying DAY-MAX watermark — a HEURISTIC (expiry-day ladder narrowing is legitimate vendor behavior). Counter `tv_chain1m_ladder_shrunk_total{underlying}` per shrunk minute. **NEVER a page.** Honest limits: the day's FIRST Found seeds the watermark (an all-day-small chain never flags — that is Empty/not-served territory); intra-day only; a respawn resets; half-watermark is counted evidence, never a verdict; shrink-to-ZERO is the existing `empty` class and feeds BOTH paging edges, never this detector (Found-only input). Companion histograms: `tv_chain1m_strikes_per_chain` + `tv_chain1m_legs_per_chain` (the Dhan mirrors of the Groww-only pair). |
| CHAIN-02 / SPOT1M-01 `trading_day_flip_exit` | `error!`, one-shot | mid-session calendar flip | the per-iteration trading-day verdict flipped mid-session and the leg's loop exited (3 sites: the chain loop + the spot per-minute loop + the spot batch loop). A suspend that crossed IST midnight is a legitimate cause — named in the body. Counters `tv_chain1m_trading_day_flip_exit_total` / `tv_spot1m_trading_day_flip_exit_total`. Log-sink-only — NO Telegram (a calendar flip is not broker failure). |

**Triage — `underlying_not_served`:**
1. `mcp__tickvault-logs__tail_errors` — find the CHAIN-02
   `stage="underlying_not_served"` line (Dhan lines are field-less on `feed`;
   Groww's carry `feed="groww"` — grep to split); the payload names the
   underlying + consecutive counted minutes.
2. On an expiry day this is usually the vendor cutting off the expiring chain
   early (the 2026-07-14 Groww NIFTY class) — it comes back with the next
   expiry at tomorrow's warmup. Cross-check the sibling feed's
   `option_chain_1m` rows for the same minutes (the §1 coverage union).
3. `tv_chain1m_underlying_not_served_total{underlying}` is the trend store;
   `tv_chain1m_retry_total` says whether retries recovered any minutes.
4. Missing minutes stay blank — DEDUP-idempotent re-appends make a later
   manual re-run safe; nothing is fabricated, nothing cross-copied.

**Cannot double-page:** the tracker counts ONLY when ≥1 sibling served
(`any_served`); the escalation edge (`chain_minute_fully_failed`) needs
ok==0 — disjoint for the fetch class. The one honest overlap (persist-failed
with ok≥1 while a sibling is empty) is two DISTINCT wanted signals (the
#1537 documented case). A supervisor respawn resets the streak — documented
~10-minute worst-case re-detection envelope (~doubled on a mid-episode
respawn).

**Recovery-ping asymmetry (refuter round 2 LOW-2, honest note):** if a
paged (not-served-latched) underlying recovers (`Found`) in the SAME minute
a sibling triggers the entitlement day-stop, the verdict sink is skipped
for that minute — the falling-edge `Chain1mUnderlyingServedRecovered` Info
ping is NOT emitted; the day-stop's CHAIN-01 page supersedes it. Cosmetic
only: the tracker state is run-scoped, so no stale streak carries into the
next session.

**Delivery boundary (honest):** CHAIN-02 / SPOT1M-01 stay log-sink-only
(`rest-1m-pipeline-error-codes.md` §3); the typed
`Chain1mUnderlyingNotServed` HIGH Telegram IS the page for this class; no
CloudWatch filter is added (the paging drift guard sees no drift).

---

## §4. Noise-lock cross-reference

The typed page pair is recorded in
`dhan-rest-only-noise-lock-2026-07-14.md` §2 (row-2 variant-cell extension) +
§2.1 (the dated 2026-07-14 directive) + the §4 honest-envelope clause — a
variant EXTENSION of family (2) ("the Dhan option-chain pull is failing",
scoped to one index), NOT a 5th alert family. Any further Dhan page class
still requires a fresh dated quote in the noise-lock file FIRST.

---

## §5. What a PR that violates this contract looks like (REJECT)

- Sequentializes the chain fire's distinct-underlying requests (or removes
  the JoinSet) without a dated rule edit HERE + a live-probe record.
- Adds a second retry per underlying per minute, retries `Found`/
  `Entitlement`/persist failures, or lifts the 15s decision ceiling without
  a fresh dated quote.
- Makes a refused retry silent (drops the counter or the coded warn).
- Pages on `ladder_shrunk` or `trading_day_flip_exit` (both are deliberate
  log-only classes), or silences either.
- Cross-copies/synthesizes rows across feeds, joins by native id, or blends
  feed values in any future decision consumer (§1).
- Adds a VIX chain slot (the existing const-assert forbids it) or a
  non-nearest expiry.

---

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/app/src/option_chain_1m_boot.rs` or the spot flip-exit sites in
  `crates/app/src/spot_1m_rest_boot.rs`
- `crates/common/src/constants.rs` (`CHAIN_1M_DECISION_CEILING_SECS` /
  `CHAIN_1M_UNDERLYING_NOT_SERVED_THRESHOLD`)
- Any file containing `chain_retry_allowed`, `CHAIN_1M_DECISION_CEILING_SECS`,
  `UnderlyingServedTracker` (Dhan), `Chain1mUnderlyingNotServed`,
  `tv_chain1m_underlying_not_served_total`, `ladder_shrunk`,
  `trading_day_flip_exit`, or `cross-source-chain-coverage`
