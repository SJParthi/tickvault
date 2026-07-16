# Judge-Locked Cadence Scheduler — Error Codes (CADENCE-01 / CADENCE-02 / CADENCE-03)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `no-rest-except-live-feed-2026-06-27.md` §8/§9 (the per-minute scheduled-pull
> grants whose FIRE TIMING this scheduler owns) >
> `rest-1m-pipeline-error-codes.md` (the capture legs' own codes — unchanged) >
> this file.
> **Operator directive (2026-07-14, relayed via the coordinator session):** the
> judge-locked cadence design — pre-fire the option chains BEFORE each minute
> close, burst the Groww lane at the boundary, ladder backward on failure, and
> emit ONE moneyness decision snapshot per (lane, minute) the instant the data
> is complete — structurally incapable of a 429 (pure CAS min-spacing gates in
> the monotonic domain; every Dhan fire passes its gate or defers, never
> violates).
> **Companion code:** `crates/core/src/cadence/` (schedule / gate / ladder /
> executor / assembly / decision / runner), `crates/app/src/cadence_boot.rs`
> (config-gated dual-spawn), `crates/common/src/config.rs::CadenceConfig`
> (`[cadence]`, serde default OFF; base.toml ships `enabled = false`),
> `crates/common/src/error_code.rs::ErrorCode::{Cadence01LaneDegraded,
> Cadence02DecisionSkipped, Cadence03SchedulerDegraded}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `Cadence0*` variant verbatim — `CADENCE-01`,
> `CADENCE-02`, `CADENCE-03`, `Cadence01LaneDegraded`, `Cadence02DecisionSkipped`
> and `Cadence03SchedulerDegraded` appear here.

---

## §0. Why these codes exist (the locked cadence)

The cadence scheduler decides WHEN the per-minute REST legs fire so that a
complete (3 chains + 3 spots, VIX advisory) moneyness picture exists per broker
lane as close to each minute close T as the brokers' rate rules allow:

- **Dhan chains** (reshaped POST-CLOSE by the 2026-07-16 operator directive
  — see the §0b SUPERSESSION) fire ALL THREE CONCURRENTLY at the burst
  second T + `dhan_burst_offset_ms` (default T+1000; NIFTY / BANKNIFTY /
  SENSEX): the broker's 1-unique-request-per-3s rule applies to the SAME
  (underlying, expiry) chain only, so different underlyings are explicitly
  parallel. Every fire still passes the per-(underlying, expiry) CAS
  min-spacing gate (≥3000 ms, monotonic domain) — in-cycle retries re-fire
  at burst + 3s, each a different key, so retries are concurrent too.
  TWO-BUCKET budget model (the 2026-07-16 all-7 correction — §0b; the
  chain-bucket exemption is **Assumed / UNVERIFIED-LIVE**, see the §0b
  RS1 honesty marker): chain
  fires are governed SOLELY by that per-key gate (the option-chain API's
  OWN budget); they neither consult nor consume the Data-API COMBINED
  ring, so the R2 (2026-07-15) combined-denial arm is SUPERSEDED for
  chains — a chain fire can no longer be deferred by the combined budget
  at all.
- **Dhan spots** (operator spec addition 2026-07-15; second-bucket packing
  per the 2026-07-16 shape + the same-day all-7 correction) fire as 4
  single-symbol calls assigned to 1000ms SECOND BUCKETS by
  `spot_second_buckets(shape, tier)` — shape 0 bases ALL 4 spots in the
  BURST second beside the 3 chains (the operator's literal "all 7
  parallel at first second"; cap-legal under the two-bucket model — 4
  spot fires ≤ the Data-API 5/sec bucket, the 3 chains in the
  option-chain API's own per-key budget — where the chain-bucket
  exemption itself is the §0b RS1 Assumed/UNVERIFIED-LIVE marker, not
  established fact); shape 1 bases all 4 in the
  second after the burst. The 2026-07-15 ADAPTIVE CONCURRENCY tiers then
  cap spots-per-second at 4/3/2/1, greedy overflow spilling to later
  buckets (both brokers' candle endpoints are single-symbol-per-request —
  never one batched HTTP request). Every fire passes the spot
  ROLLING-1000ms-WINDOW gate (≤ `spot_window_cap`, default 4,
  hard-bounded 1..=5 — Dhan's 5/sec cap). Since verifier L1 (2026-07-15),
  RE-SCOPED by the 2026-07-16 all-7 correction: every Dhan SPOT and
  EXPIRY-LIST fire additionally records into ONE COMBINED rolling-1000ms
  window (cap 5, Dhan's Data-API per-second hard budget) inside
  `DhanGates`, checked-then-recorded atomically — CHAIN fires are
  EXCLUDED (their budget is the per-(underlying, expiry) gate), so a full
  spot group + an expiry fire can never jointly exceed 5 Data-API
  requests in any rolling second (the replay ledger asserts the COMBINED
  spot+expiry cap plus the per-key chain floors). Degrade one step after
  2 CONSECUTIVE spot-dirty (rate-limited) cycles; recover one step after
  3 consecutive clean cycles (both config-keyed, Assumed pending operator
  confirm).
- **Groww** fires per its fallback-shape ladder (operator verbatim
  2026-07-16 two-rung prescription + the retained choice-3 last resort —
  §0b): rung 0 (default) = all 7 requests in parallel at T+0 (gate-free
  lane BY CONSTRUCTION — no Groww arm ever touches the gates); rung 1 =
  :01 all 3 chains / :02 ALL 4 spots (VIX included); rung 2 (KEPT beyond
  the operator's two-rung prescription as the last resort — dated note in
  §0b) = :01 chains / :02 core spots / :03 VIX alone.
  Verdict = last wave + the burst timeout, sequential fallback on failed
  legs; same 2-dirty/3-clean streak rules as the spot ladder. VIX absence
  NEVER blocks the Groww data-complete predicate (coordinator-confirmed
  2026-07-15 — VIX stays advisory).
- **Shape ladder** (2026-07-16 — replaces the retired pre-close anchor-shift
  failure ladder; arming re-scoped by the same-day correction): rung 0 ⇄ 1
  between the all-7 primary and the split fallback, driven by the SAME
  streak thresholds the concurrency ladders use (degrade after 2
  CONSECUTIVE dirty cycles — the operator's "tried that multiple times" —
  recover after 3 consecutive clean; dirty = ≥1 RateLimited in the cycle,
  the SOLE arming class — Timeout / Transport / Empty / QueueDelay NEVER
  reshape); a RateLimited leg KEEPS its ONE bounded in-cycle retry
  (through the gates, after per-key spacing) — never more than the
  bounded budget, never a blind storm.
- **Decision** per (lane, cycle minute): exactly-once latch, Decided XOR
  Skipped, spot provenance own → cross-source → chain-embedded, honest-skip at
  the lane cutoff. Dry-run day 1: both lanes run the `DryRunLoggingExecutor`
  (logs the fire, returns Empty, NEVER synthesizes a price).
- **Pre-market expiry resolution** (Workstream A, operator spec 2026-07-15):
  per (broker, underlying) the runner's resolution loop fetches the vendor
  expiry list with bounded retry (`expiry_retry_interval_ms`, default 60s)
  and records the POLICY date into the process-global day-locked store —
  NIFTY/SENSEX = NearestActiveDate (min valid date ≥ today); BANKNIFTY =
  LastExpiryOfNearestActiveMonth (group by (year, month), nearest group with
  ANY active date, that group's LAST date — NEVER a flat min; the
  no-BANKNIFTY-weeklies premise is **Assumed** post the Nov-2024 NSE
  expiry-rationalisation). The day lock keys on the IST trading day
  (`trading_calendar::ist_offset()` — never UTC); re-resolution happens ONLY
  at the day flip, so a supervisor respawn RE-READS, never re-resolves
  mid-day. The IST deadline (`expiry_deadline_secs_of_day_ist`, default
  08:55) gates the edge-latched PAGE, never the attempts — a boot after the
  deadline still resolves on its first success, and the background retry
  continues at the same cadence until session end; IN-SESSION retry waves
  anchor at MID-MINUTE (:30 — `next_expiry_wave_instant_ms`, R1 2026-07-15),
  maximally far from the post-close burst region (2026-07-16: every fire
  packs into T+0..≈T+5s, retries ≤ T+15s), so a vendor-outage retry cadence can never phase-lock into the
  burst window and evict a NOMINAL fire from the combined budget (the L2
  expiry gate stays the backstop); a process that BOOTS
  after the deadline requires ≥2 consecutive failed attempt waves
  (`POST_DEADLINE_BOOT_MIN_FAILED_WAVES`) before the page fires — never the
  first-wave hair trigger (E4, 2026-07-15; the pre-deadline path is
  unchanged), and those waves count REAL dispatched attempts per
  (broker, underlying) pair only (R3, 2026-07-15 — disabled-lane and
  gate-deferred/conceded iterations never advance the threshold). The read facade the runner stamps requests from is
  DAY-CHECKED (E1 fix, 2026-07-15): the IST trading day is threaded from
  the injected clock into every `resolved_expiry` call, so a process
  crossing IST midnight whose morning re-resolution keeps FAILING stamps
  `None` (degraded, loud) — never yesterday's (potentially expired) winner.
  Both brokers resolving to
  DIFFERENT dates ⇒ **Dhan WINS for keying BOTH lanes** (the
  exchange-sourced expirylist is authoritative), loudly via the edge-latched
  `expiry_disagreement` stage; both raws stay recorded for provenance.
  **HONEST RESIDUAL (E2, dated 2026-07-15 — process-restart re-resolution):**
  the day lock is IN-MEMORY (process-global `OnceLock`), so it is
  TASK-RESPAWN-proof ONLY — a mid-session PROCESS restart (crash-boot,
  deploy) re-resolves from scratch against the vendor's CURRENT list. On
  expiry day, a vendor list that drops today's date intraday would then
  silently roll BOTH lanes to the next series mid-session (pinned by the
  passes-by-design demo test
  `test_cadence_expiry_process_restart_reresolution_rolls_forward_when_vendor_drops_today`).
  **Flagged follow-up:** persist the day lock as a tiny date+expiries JSON
  file (the `data/instrument-cache` plan-snapshot precedent —
  `instrument_snapshot.rs`, incl. its fail-closed `is_valid_trading_date`
  validation) that the boot resolution phase consults FIRST: same IST day +
  parseable → adopt without re-resolving; else cold-resolve and rewrite;
  fail-closed on corrupt/mismatched files. Deliberately NOT smuggled into
  the 2026-07-15 fix round — it adds file I/O + a path knob to the
  resolution loop and every deps construction site.

**HONEST COMPOSITION WORDING (verifier F6, dated 2026-07-15 — SUPERSEDED
2026-07-16 by the §0b ruling):** the F6 paragraph reasoned about cadence
fires queueing inside the shared `dhan_data_api_limiter`; per the
2026-07-16 coordinator ruling the cadence lane does NOT route through that
limiter at all (the combined cap-5 ring is the binding pacing — §3b item
1), so the queue-spill analysis is moot for cadence fires. The `QueueDelay`
typing + non-arming rule it motivated is KEPT for any executor-side
self-inflicted pacing source.

Everything is COLD-PATH (a handful of scheduled fires per minute); the tick hot
path, the WS read loops, the aggregator, and trading are never touched. The
`[cadence]` config ships `enabled = false` — a disabled boot is byte-identical
to today.

## §0b. 2026-07-16 SUPERSESSION — post-close burst reshape (operator directive)

**The verbatim operator demand (2026-07-16, relayed via the coordinator
session — preserve exactly, do not paraphrase):**

> "for both dhan and groww trigger everything precisely parallelly entire 7
> request instantly at the first second... one and only when it fails then
> split it: first second pull option chain for both dhan and groww and
> second second make it as spot... 1 request per 3 second is nowhere
> applicable for different option [underlyings], clearly precisely
> applicable for one and only same option chain expiry."

**The same-day RULING CORRECTIONS (2026-07-16, relayed via the coordinator
session — verbatim, superseding the first implementation pass):**

Correction 1 — the Dhan primary is ALL 7 CONCURRENT (the interim "honest
5+2 packing" reading is RETIRED as the primary):

> "i clearly told you for dhan also as the primary all 7 parallel at first
> second one and only when it fails or rate limited alone only then this
> option chain first second and spot second."

All-7 is taken as cap-legal via the TWO-BUCKET budget model: the 4 spot
fires sit in the Data-API 5/sec bucket (4 ≤ 5, enforced by the COMBINED
spot+expiry rolling ring); the 3 chain fires sit in the option-chain
API's OWN per-(underlying, expiry) budget (different underlyings
explicitly concurrent per the directive). **The chain-bucket EXEMPTION
is Assumed / UNVERIFIED-LIVE (RS1 honesty marker, 2026-07-16 —
supersedes this section's earlier "breaches NEITHER documented budget"
assertion of fact):** whether Dhan exempts `optionchain` fires from the
Data-API 5/sec bucket is documented nowhere we can cite — the
counter-evidence is that `dhan/api-introduction.md` rule 10 lists
Option Chain under Data APIs (5/sec), and the §8 grant math
(`no-rest-except-live-feed-2026-06-27.md` §8.1) historically counted
chains + spots jointly against one budget. The DISCRIMINATOR: the
2026-07-16 15:35 IST post-market wire probe (all-7 concurrent on both
brokers + the per-(underlying, expiry) 3s test) is the first live
evidence — if it confirms the exemption, this marker upgrades to
Verified-live with the probe evidence cited here. If the wire instead
enforces ONE bucket, the burst 429s, the RateLimited-only shape ladder
demotes to the split fallback — and the per-IST-day rung-0 RE-ENTRY
CAP (`CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY` = 3, `ladder.rs`) stays
as the BELT either way: after 3 same-day recoveries back to rung 0,
the next demotion HOLDS rung 1 for the rest of the session (ONE
edge-latched CADENCE-01 `rung0_reentry_cap_latched` log per day; the
IST day-start reset re-arms it) — a one-bucket wire can never
oscillate the shape 0⇄1 all day, and the pre-cap `ladder_exhausted`
blind spot (rung 1 stays clean, so the exhausted edge never fires
during a 0⇄1 oscillation) is closed by the same cap. Honest envelope:
the cap's latch is runner-TASK-local, so an unwind-build task respawn
resets it — bounded (release builds abort on panic per `panic =
"abort"`, making the respawn arm release-unreachable, and respawn
storms are independently paged via `tv_cadence_runner_respawn_total`).
The combined ring is therefore RE-SCOPED to spot + expiry-list fires
ONLY; chain fires are governed solely by the per-key ≥3s CAS gate.

Correction 2 — demotion is RATE-LIMIT-ONLY, after MULTIPLE attempts:

> "see that too instantly dont commit — one and only when you tried that
> multiple times and gets rate limited alone alone fallback."

`RateLimited` is the SOLE ladder-arming class (Timeout / Transport /
Empty / QueueDelay never reshape); "multiple times" = the existing
2-consecutive-dirty-cycles trigger; and a rate-limited leg KEEPS its one
bounded in-cycle retry (through the gates, after per-key spacing) —
reversing the first pass's "429 is never blind-retried in-cycle" rule.
One retry per leg per cycle, never more.

**The new slot tables (all instants relative to each minute close T):**

| Broker | Rung | Second 1 | Second 2 | Second 3 |
|---|---|---|---|---|
| Dhan | 0 (primary) | ALL 7 CONCURRENT — 3 chains + all 4 spots (the operator's "all 7 parallel at first second"; two-bucket cap-legal, resting on the §0b RS1 Assumed/UNVERIFIED-LIVE chain-bucket exemption) | — | — |
| Dhan | 1 (fallback) | 3 chains concurrent | ALL 4 spots | — |
| Groww | 0 (primary) | all 7 requests parallel at T+0 | — | — |
| Groww | 1 (fallback) | 3 chains at :01 | all 4 spots at :02 | — |
| Groww | 2 (last resort — KEPT beyond the operator's two-rung prescription, dated note below) | 3 chains at :01 | core spots at :02 | VIX alone at :03 |

Dhan in-cycle retries: burst + 3s (T+4s nominal — each failed underlying's
retry is a DIFFERENT (underlying, expiry) key, concurrent); spot retries
append one 1000ms window past the last spot bucket. The 2026-07-15 spot
CONCURRENCY tiers (4/3/2/1 per second group, greedy overflow to later
buckets) compose WITH the shape — `spot_second_buckets(shape, tier)` is the
single pure source of the packing, proptested per (shape × tier × failure)
permutation by the zero-429 replay.

**RETIRED machinery (deleted, not dormant):** the Dhan pre-close chain
instants (:55/:58/:02 — `dhan_chain_offsets_ms`), the T+3s spot group
anchor (`dhan_spot_start_offset_ms`), the anchor-shift failure ladder
(`LadderState` / `CycleVerdict` / `next_rung` / `dhan_ladder_step_ms` /
`dhan_ladder_max_rungs` / `recovery_mode`), the GLOBAL 3s chain
min-spacing gate (the directive: the 3s rule binds the SAME chain expiry
only), and the lender-aware cross-fill freshness widening
(CADENCE-XFILL-RUNG-1 — every fire is post-close, so the plain base
T−5000 floor suffices; coordinator addendum item 3). KEPT as THE binding
Dhan enforcement: the per-(underlying, expiry) ≥3s gate (the SOLE chain
budget per the all-7 correction), the combined rolling-1000ms cap-5
window (RE-SCOPED to spot + expiry-list fires only), the expiry-wave :30
mid-minute anchor + the 1-per-rolling-second expiry spacing, and the
QueueDelay non-arming rule.
The `ladder_shift` stage + `tv_cadence_ladder_rung` gauge +
`tv_cadence_ladder_shifts_total` counter are replaced by
`dhan_shape_shift` / `tv_cadence_dhan_shape_step` /
`tv_cadence_dhan_shape_shifts_total`; the `ladder_exhausted` CADENCE-01
edge is KEPT (a dirty cycle while ALREADY at the split-fallback rung —
cross-source steady state until the first clean Dhan cycle).

**Groww rung 2 retention (dated note):** the operator's 2026-07-16
prescription names exactly two rungs (all-7, then the chains/spots split).
Choice 3 (core spots :02 / VIX alone :03) is RETAINED as a last resort
BEYOND that prescription (coordinator addendum item 1, 2026-07-16) — it is
reachable only after repeated dirty streaks at rung 1 and degrades
gracefully toward the advisory-VIX-last shape; removing it needs no new
quote, keeping it is the recorded default.

**Cadence-lane pacing authority (coordinator ruling A, 2026-07-16):** the
2026-07-16 directive supersedes the 2026-07-14 3-rps pacing FOR THE
CADENCE LANE ONLY — cadence fires are governed by the combined
rolling-1000ms cap-5 ring (= the annexure's documented 5/sec Data-API
broker cap), NOT routed through the shared `dhan_data_api_limiter`; the
3 rps self-tuner remains the pacing authority for the LEGACY per-minute
paths (`rest-1m-pipeline-error-codes.md` §2f carries the mirrored dated
note). The shape ladder's DH-904/429 demotion is the guard if the broker
disagrees with the packing. The limiter bypass is rule-text-only today —
a code-side limiter-free source scan (asserting no cadence executor
routes through `dhan_data_api_limiter`) lands with the executor PR
(flagged follow-up). HONEST NOTE: day 1 both lanes run the
`DryRunLoggingExecutor` (zero REST calls), so the live rung-0 burst
behavior ACTIVATES only when the real broker executors land and
`[cadence] enabled` flips per the §4 governance — nothing about this
supersession fires a request today.

**Capture-leg coexistence (coordinator ruling B, 2026-07-16 — SUBSUME,
NEVER SHARE, via the INTERIM mutual exclusion; hardened same day by
RS3):** no double demand against a broker's budget is ever legal.
`AppConfig::validate()` fail-closes a config that enables the cadence
scheduler while ANY per-minute capture leg is enabled (`[spot_1m_rest]` /
`[option_chain_1m]` / `[groww_spot_1m]` / `[groww_option_chain_1m]` /
`[groww_contract_1m]`) — **keyed on the LEG configs ALONE, deliberately
NOT on `feeds.*_enabled` (RS3, 2026-07-16 — supersedes the first-pass
per-lane key):** the cadence lanes activate on the RUNTIME feed atomics
(one toggle away from the boot flags) while the legacy legs spawn on
their OWN config gates regardless of the feed flags, so the original
boot-time key admitted cadence=ON + feed=OFF + legs=ON — one runtime
`/api/feeds` enable away from reconstructing the double demand (today
both enable directions happen to be unconditionally 409'd at the API —
the PR-C2/S2b retired-lane refusals — but those exist for unrelated
reasons and must not be this invariant's only wall; a handler-side
re-check was rejected as unreachable dead code behind those 409s plus a
config-plumbing cascade into the api crate). Pinned both directions,
including the feed-flag-irrelevance arms, by
`test_application_config_validate_cadence_capture_leg_mutual_exclusion`.
Flipping `[cadence] enabled = true` therefore REQUIRES standing ALL the
legs down first; full subsumption (the cadence lane feeding the legs'
tables) is the flagged follow-up.

## §1. CADENCE-01 — a broker lane degraded inside a cycle

**Severity:** High. **Auto-triage safe:** Yes (the cycle already ended; the
next minute boundary re-fires automatically — the operator inspects, never
manually re-fires).

**Trigger:** `ErrorCode::Cadence01LaneDegraded`, distinguished by the `stage`
field. Coalesced ONCE per (lane, cycle) at cycle wrap-up — never per-request —
EXCEPT `rate_limited`, which fires per-request by design (see below):

| stage | Meaning |
|---|---|
| `fetch_failed` | ≥1 chain/spot request on the lane ended Timeout / Transport / Auth / Malformed AFTER the retry budget (Dhan: no in-cycle retry admitted / the retry itself failed; Groww: the fallback attempt failed) with the cell still missing — never a first-attempt-then-retried-OK blip, and never the Empty class (that has its own stages below) |
| `rate_limited` | a broker 429 arrived DESPITE the gates — per-request (rare by construction; every occurrence is a gate-bug signal worth its own line), arms the shape ladder (the SOLE arming class per the 2026-07-16 correction) and keeps its ONE bounded in-cycle retry through the gates — never more than the bounded budget, never a blind storm |
| `spot_empty` | a spot leg returned 2xx-without-data (the Dhan 200-empty saga class; EITHER lane — dry-run returns Empty on every fire by design, see the dry-run note below); does NOT arm the ladder |
| `chain_empty` | a chain leg returned 2xx-without-usable-data (EITHER lane); does NOT arm the ladder — kept distinct from `fetch_failed` so a 200-empty is never misread as a transport failure |
| `groww_fallback` | the Groww T+800 verdict found failed burst legs — the sequential fallback engaged |
| `cross_fill` | the lane's OWN fetch path exhausted (every own fire completed/failed, retries spent) without the cell; the OTHER lane's fresh same-minute data filled it (provenance stamped `CrossSource`). Freshness floor: the plain base T−5000 window for BOTH lenders (`CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS`). *(Superseded 2026-07-16: the LENDER-aware widening — CADENCE-XFILL-RUNG-1, 2026-07-15, which widened the window to the Dhan lender's rung-shifted earliest chain PRE-fire — was RETIRED with the pre-close anchor ladder in the §0b reshape; every fire is post-close now, so the pre-fire instant it widened for no longer exists and the base floor suffices — §0b RETIRED-machinery list.)* The fallback rungs run only on own-path exhaustion or at the cutoff — never preempting a still-scheduled own fire (design §5 resolution order) |
| `chain_embedded_spot` | third-rung provenance: the chain response's own embedded underlying spot filled the cell (own path exhausted first, as above) |
| `moneyness_unknown` | ≥1 underlying's fold classified Unknown (spot unusable / rows unclassifiable / registry snapshot refused by the decide-time guard: unconfirmed publish, wrong minute, stale, or the boot sentinel) |
| `queue_delay` | a fetch was refused by a SELF-INFLICTED pacing-queue deadline — our own machinery (e.g. an executor-internal queue), never the broker (F1(iii) 2026-07-15). *(Superseded 2026-07-16, coordinator ruling A: cadence fires no longer route through the shared `dhan_data_api_limiter` at all — §0b/§3b item 1 — so the originally-named shared-limiter source can no longer produce this stage on the cadence lane; the typing is KEPT for any future executor-side self-inflicted pacing source.)* Stage-tagged distinctly, NEVER folded into `fetch_failed`, NEVER arms any ladder |
| `expiry_unresolved` | TWO emission points share this stage: (a) the per-cycle coalesced flag — ≥1 chain request was stamped `expiry_yyyymmdd = None` (the day-locked store has no policy date yet; the scheduler NEVER guesses — the executor impl may fall back to its warmup expiry; ALWAYS present in dry-run, where every expiry-list fetch returns Empty); (b) the resolution loop's EDGE-LATCHED deadline page — ONE `error!` per (broker, underlying) per IST day the instant `expiry_deadline_secs_of_day_ist` (default 08:55) passes unresolved; the lanes run degraded meanwhile and the background retry continues at `expiry_retry_interval_ms` until session end (the deadline gates the PAGE, never the attempts, and a post-deadline BOOT requires ≥2 consecutive failed waves before the page — E4, 2026-07-15; R3, 2026-07-15: waves count REAL dispatched attempts per pair only — a disabled-lane or gate-deferred iteration never advances the threshold). FALLING EDGE (E3, 2026-07-15): a LATER successful resolution for a pair whose page HAD fired emits one coded recovery `info!` (`stage = "expiry_resolved_late"` on the same CADENCE-01 code — no new variant) + `tv_cadence_expiry_resolved_late_total{broker, underlying}`, at most once per pair per day (first write wins) |
| `expiry_disagreement` | both brokers resolved the day's policy expiry for one underlying and the dates DIFFER — **Dhan WINS for keying BOTH lanes** (exchange-sourced expirylist authority); edge-latched ONCE per (underlying, day); both raw dates ride the payload + the store's provenance view (`tv_cadence_expiry_disagreement_total{underlying}`). SINCE 2026-07-16 (R6) the same edge ALSO dispatches the REAL typed `CadenceExpiryDisagreement` HIGH Telegram page (authority: `dhan-rest-only-noise-lock-2026-07-14.md` §2.2) — the ONE cadence signal that pages directly today |
| `expiry_rate_limited` | an expiry-list fetch returned a broker 429 (verifier L2, 2026-07-15 — was `debug!`-only): one coded `warn!` per occurrence + `tv_cadence_expiry_rate_limited_total{broker}`; never blind-retried in-wave — the next `expiry_retry_interval_ms` wave re-attempts THROUGH the gates. Dhan expiry fires pass `DhanGates::try_acquire_expiry` (the L1 COMBINED 5-per-rolling-second budget + a 1-per-rolling-second expiry spacing) BEFORE dispatch, so a Dhan expiry 429 despite the gates is a gate-bug / shared-budget-co-tenant signal; a gate deferral skips the fire to the next wave (`tv_cadence_expiry_gate_deferred_total{broker}` — a deferral, never a violation). Groww expiry fires stay ungated by design (no Groww rate rule) |
| `ladder_exhausted` | a dirty (rate-limited) cycle while the Dhan SHAPE ladder is ALREADY at its max rung — **rung 1, the split fallback** *(superseded 2026-07-16: the earlier "max rung (5)" wording described the RETIRED pre-close anchor ladder — §0b)* — edge-latched ONCE per episode, re-armed by a clean cycle. Per-ladder precision: this edge is DHAN-SHAPE-ONLY by construction (runner.rs folds only the Dhan shape ladder into it); the Groww fallback-shape ladder tops out at its own max rung 2 (the choice-3 last resort) with NO exhausted edge — its last rung IS the bounded degrade, reported via the lane's own stages — and the Dhan spot-concurrency ladder (max step 3, fully sequential) likewise has no exhausted edge |
| `rung0_reentry_cap_latched` | the Dhan shape ladder's per-IST-day rung-0 RE-ENTRY CAP latched (RS1(b), 2026-07-16): after `CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY` (3) same-day recoveries back to the all-7 rung, the NEXT demotion holds rung 1 for the rest of the session — the termination belt for the Assumed/UNVERIFIED-LIVE chain-bucket exemption (§0b): a one-bucket wire would otherwise oscillate the shape 0⇄1 all day with `ladder_exhausted` never firing (rung 1 stays clean). Edge-latched ONCE per IST day; the day-start reset re-arms. Counter: `tv_cadence_dhan_rung0_reentry_cap_latched_total` |

**Dry-run note (honest — not an incident; DEMOTED per verifier F10, dated
2026-07-15):** with the `DryRunLoggingExecutor` every fire returns Empty, so
EVERY enabled dry-run cycle produces ONE coalesced degrade per lane whose
stages are the empty classes (`chain_empty,spot_empty` on Dhan;
`chain_empty,spot_empty,groww_fallback` on Groww) — ~1,500 High `error!`
lines/day of pure expected-shape noise pre-fix. The boot wiring therefore
passes `dry_run = true` and the runner DEMOTES dry-run-shaped degrades (and
the §2 skips) to `info!` with a `dry_run = true` field — counters unchanged,
the trend survives. REAL failure classes (`fetch_failed` / `rate_limited`)
keep the coded `error!` even in dry-run (they cannot come from the dry-run
executors). The REAL-executor PR flips the flag to `false`.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CADENCE-01`; the payload carries
   `lane`, `stage`, the cycle minute (IST), and per-leg outcome counts.
2. `tv_cadence_fetch_total{lane,leg,outcome}` rates name the dominant failure
   class; cross-check the capture legs' own codes (SPOT1M-01 / CHAIN-02 — same
   broker surface) and the token machinery (AUTH-GAP-*) on `auth` outcomes.
3. `stage="rate_limited"` with the gates enabled means the broker tightened its
   window BELOW our floor OR another consumer shares the budget (the BruteX
   co-tenancy note in `no-rest-except-live-feed-2026-06-27.md` §9.3) — read
   `tv_cadence_gate_denials_total` (must stay 0 on nominal slots) and the
   ladder gauge before touching any spacing constant.
4. A sustained `ladder_exhausted` (the Dhan shape rung pinned at 1 — the
   split fallback; *superseded 2026-07-16: "rung pinned at 5" described the
   retired anchor ladder's max*) means the broker keeps rate-limiting even
   the chains-then-spots split — the decision quality is already degraded
   honestly via CADENCE-02 skips; investigate the broker surface, not the
   scheduler.

## §2. CADENCE-02 — decision skipped (fail-closed, exactly-once)

**Severity:** High. **Auto-triage safe:** Yes — **the skip IS the fail-closed
action** (no decision is emitted on incomplete/stale data; nothing to undo).

**Trigger:** `ErrorCode::Cadence02DecisionSkipped` — the (lane, cycle minute)
latch closed with a Skipped outcome instead of a Decided one. Stages:

| stage | Meaning |
|---|---|
| `cutoff` | the lane cutoff (groww 6000 ms / dhan 15000 ms past T) elapsed with the data-complete predicate still false — older data is stale for a next-minute fill model, so the minute is honestly skipped |
| `both_sources_dead` | neither the lane's own fetches nor cross-source/chain-embedded fallbacks produced the required cells |
| `all_unknown` | data arrived but ALL 3 underlyings folded to Unknown moneyness — nothing usable |

Late responses arriving AFTER the latch are audit-only: the chain snapshot is
STILL published to the registry (never dropped, never duplicated) and counted
by `tv_cadence_late_response_total{lane}`; the decision is untouched.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CADENCE-02`; the payload carries
   `lane`, `stage`, `cycle_minute_ist`, and which cells were missing/unknown.
2. One skip = one degraded minute, self-healing at the next boundary. A RUN of
   skips on one lane = that broker's surface is down — cross-check CADENCE-01
   stages and the capture legs' codes for the same window.
3. Dry-run note (F10, 2026-07-15): with the `DryRunLoggingExecutor` EVERY
   cycle skips (`cutoff` — Empty results by design) AND produces the §1
   dry-run empty-stage line per lane — BOTH demoted to `info!` with
   `dry_run = true` (the boot wiring's flag; the pre-fix coded `error!`
   flood was ~1,500 High lines/day of expected shape). The counters keep
   counting; a REAL-executor boot keeps the coded `error!`.

**Delivery boundary (honest — no false-OK):** CADENCE-02 is **log-sink-only on
day 1** — NO `error_code_alerts` map entry, NO Telegram event. The typed
edge-latched Telegram page (3 consecutive skips per lane, the rest-1m edge
pattern) is a **flagged follow-up that ships with the enable flip**, NOT now:
the 2026-07-14 Dhan noise lock (`dhan-rest-only-noise-lock-2026-07-14.md`)
forbids dry-run page noise, and a dry-run boot skips every minute by design
(the SCOREBOARD-01 delivery-boundary precedent). A skip is NEVER rendered as
OK anywhere (audit Rule 11): the decision counter's `outcome` label and the
structured log both say `skipped_*`.

## §3. CADENCE-03 — scheduler machinery degraded

**Severity:** Medium. **Auto-triage safe:** Yes (every arm already
self-corrected or self-reported; the operator inspects trends).

**Trigger:** `ErrorCode::Cadence03SchedulerDegraded`, `stage` field:

| stage | Meaning |
|---|---|
| `dhan_shape_shift` | the 2026-07-16 Dhan SHAPE ladder moved a rung (0 = the all-7 primary ⇄ 1 = split fallback; `tv_cadence_dhan_shape_shifts_total{direction}` — `up` = degraded toward the fallback, `down` = recovered; the NEXT cycle uses the new shape; armed by RateLimited ONLY after 2 consecutive dirty cycles). Replaces the retired `ladder_shift` anchor-shift stage |
| `spot_concurrency_shift` | the 2026-07-15 Dhan spot concurrency ladder moved a step (`tv_cadence_spot_concurrency_shifts_total{direction}`; `up` = degraded toward less concurrency, `down` = recovered toward step 0 — the NEXT cycle uses the new grouping) |
| `groww_shape_shift` | the 2026-07-15 Groww three-choice fallback-shape ladder moved a choice (`tv_cadence_groww_shape_shifts_total{direction}`; same direction convention — the NEXT cycle uses the new wave shape) |
| `late_wake` | a cycle wake fired ≥1000 ms after its target instant (scheduler starvation / suspend) — the cycle proceeds with the remaining slots |
| `boundary_skipped` | ≥1 minute boundary elapsed entirely un-fired (overrun / suspend / clock step); counted, coalesced, next boundary joins cleanly (no-mid-cycle-join) |
| `skew_clamped` | the wall-clock target computation clamped an implausible skew — targets re-picked, the monotonic gates were never at risk |
| `respawn` | the supervised runner task died and was respawned (`tv_cadence_runner_respawn_total{reason}`) |
| `gate_deferred_nominal` | a NOMINAL slot fire was deferred by a gate — a should-never signal (the schedule serializes nominal slots wider than every spacing); any occurrence is a schedule/gate consistency bug worth a report |
| `double_latch` | a decision double-latch attempt was REFUSED by the exactly-once guard (should-never scheduler-logic signal; `tv_cadence_double_latch_total{lane}`). Replaced the pre-fix `debug_assert!(false)` — a coded loud refusal, never a panic path (verifier F10, dated 2026-07-15) |
| `illegal_fsm_move` | a lane-FSM transition with no legal target was REFUSED — the state HOLDS (should-never scheduler-logic signal; `tv_cadence_illegal_fsm_move_total{lane}`). Replaced the pre-fix `debug_assert!(false)` at `LaneRun::fsm` — a coded loud refusal, never a panic path (verifier nuance-b, dated 2026-07-15; the F10 double-latch precedent) |

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `CADENCE-03`; the `stage` names
   the arm.
2. `late_wake` / `boundary_skipped` trends correlate with host pressure — cross
   check PROC-01 / RESOURCE-01..03 and the sibling per-minute legs'
   `boundary_skipped` counters (same scheduler pressure shows on all of them).
3. A `respawn` storm (sustained `tv_cadence_runner_respawn_total`) is a real
   bug — capture the panic backtrace in `data/logs/errors.jsonl.*`. Release
   builds abort on panic (`panic = "abort"`) — the respawn arms are
   unwind-build self-heal paths, the TICK-FLUSH-01 honesty note.
4. `gate_deferred_nominal` — do NOT widen spacings; the 2026-07-16 all-7
   burst is validated cap-legal at boot AGAINST OUR OWN GATES ONLY
   (`dhan_burst_offset_ms` band + the two-bucket split: 4 nominal spots ≤
   the combined spot+expiry cap, chains per-key only — a structural check
   of the schedule vs our gate math, NOT a claim the broker's wire
   accepts the burst; the chain-bucket exemption stays the §0b RS1
   Assumed/UNVERIFIED-LIVE marker), so a live occurrence implies a
   runtime retry interleaving bug — file it.

**Honest-envelope note — `tv_cadence_gate_deferred_total` is TREND-ONLY
(verifier F5, dated 2026-07-15):** the deferred counter has NO alarm, and the
zero-tolerance dispatch-lateness demotion
(`CADENCE_NOMINAL_DISPATCH_TOLERANCE_MS = 0`) means even 1ms of upstream
jitter demotes the rest of the cycle's nominal fires — so the
`gate_deferred_nominal` page class is effectively DEAD in production
(nominal-denial pages fire only on a mathematically-on-time deferral, which
jitter makes vanishingly rare). Stated plainly: a REAL schedule/gate math bug
surfaces as a `tv_cadence_gate_deferred_total{key}` trend STORM, which the
operator reads from the counter, not from a page. Do not treat a quiet page
stream as proof of gate health — read the counter trend.

**Level-triggered enable-flag note (verifier F8, dated 2026-07-15 —
HARDENED by CONC-NEW-1, hostile round 1 2026-07-15):** the original F8
wording ("read once per cycle; a disable is observed within ~15s / at the
next boundary") was WRONG in both directions: `run_cycle` is ENTERED the
moment a joinable boundary exists — the day's FIRST cycle is joined near
IST midnight and waits ~9h for the 09:15:55 anchor with the midnight flag
snapshot frozen, and even mid-day the next cycle's snapshot was taken
~45s before its first fire. The runner now (a) RE-OBSERVES the
`dhan_enabled` / `groww_enabled` atomics every ~5s wake chunk while the
cycle is PRISTINE (no fire dispatched) and re-arms the whole cycle from
the fresh flags on any change — so a pre-fire toggle in EITHER direction
is honored within ~5s, and an enable joins the CURRENT cycle; and (b)
re-checks the atomics at every dispatch/completion instant — a mid-cycle
disable stops every not-yet-dispatched fire (the lane drops
shutdown-shaped: no partial emit, no degrade page, no decision).
Already-IN-FLIGHT requests complete as audit-only late responses, and a
completion-driven single deferred fallback racing the disable is bounded
to at most one request. An enable AFTER the cycle's first fire joins at
the next minute boundary (unchanged). Ratchets:
`test_pristine_cycle_observes_disable_before_first_fire` /
`test_pristine_cycle_observes_enable_before_first_fire` /
`test_midcycle_disable_stops_not_yet_dispatched_fires` in
`cadence_runner_dry_run.rs`. Never a bug report: this is the documented
lifecycle envelope (the same wording lives on `CadenceConfig`'s doc).

## §3b. COMPOSITION CONTRACT (2026-07-15) — binding on every future executor impl

Verifier F1(iii), dated 2026-07-15. Every FUTURE `CadenceExecutor`
implementation that fires a REAL Dhan REST call MUST compose with the two
existing floors — never fight them, never bypass them:

1. **Cadence fires do NOT route through the shared `dhan_data_api_limiter`**
   (coordinator ruling A, 2026-07-16 — §0b): the scheduler's combined
   rolling-1000ms cap-5 window (= the broker's documented 5/sec Data-API
   budget) is THE binding cadence-lane pacing; routing a cadence fire
   through the 3 rps self-tuner (`crates/app/src/dhan_data_api_limiter.rs`,
   `rest-1m-pipeline-error-codes.md` §2f — which REMAINS the authority for
   the LEGACY per-minute paths) would re-serialize the operator's mandated
   burst. The shape ladder's DH-904/429 demotion is the guard if the
   broker disagrees.
2. **Record/consult the PROCESS-GLOBAL gate registry**
   (`gate::global_dhan_gates()` — one shared `DhanGates` per process, F1(ii)):
   an executor-side fast path or a second scheduler instance must share the
   SAME budget, or two innocent components jointly exceed Dhan's window. The
   per-underlying 3s chain gate is ALWAYS enforced; when the expiry is known
   the per-(underlying, expiry) stamp is recorded/consulted too — an
   expiry-LESS fire is strictly MORE conservative (subsumption, pinned by
   `test_cadence_gate_expiryless_fire_subsumes_expiry_stamp`).
3. **Type self-inflicted pacing-queue deadline misses as
   `CadenceFetchError::QueueDelay`** — NEVER `Timeout`.
   A queue delay is SELF-INFLICTED pacing (our own machinery — e.g. a
   legacy-path shared limiter, an executor-internal queue), not a broker
   signal: it is stage-tagged distinctly
   (`queue_delay`), excluded from `fetch_failed`, and NON-ARMING for every
   ladder (`failure_arms_ladder` refuses it — implemented NOW with the
   variant, tested in `ladder.rs`; arming on it would let our own
   defense-in-depth walk the anchor earlier forever).

The heading of this section and the `pub fn global_dhan_gates` handle are
pinned by the source-scan ratchet
`crates/core/tests/cadence_composition_contract_guard.rs` — deleting either
fails the build.

## §3c. Groww request-volume envelope (verifier F4, dated 2026-07-15)

The Groww verdict must NOT refetch a leg whose ORIGINAL request is still IN
FLIGHT at the ~T+800ms verdict instant — the pre-fix "Err OR still pending"
read fired a duplicate concurrent same-leg request while the original was
mid-flight, doubling the lane's request volume against a SLOW broker (the
exact condition under which extra volume hurts most) for zero data gain
(first-write-wins discarded one of the two). The fix SKIPS in-flight legs
(await-or-skip; the slow original still lands first-write-wins, and an Err
completion after the verdict is terminal on its 1st attempt). Request-volume
envelope per Groww cycle: ≤ 7 burst + ≤ 7 fallback = ≤ 14, with the fallback
now bounded to GENUINELY-FAILED legs only — a slow-broker cycle no longer
doubles a leg. Pinned by
`test_groww_verdict_skips_inflight_leg_never_duplicates`.

Two honesty notes on the deferred-fallback composition (R6/R7, dated
2026-07-15): (a) an in-flight-skipped leg whose original request completes
`RateLimited` gets its ONE deferred fallback dispatched at that completion
instant — a zero-delay single bounded retry against a broker that just
429'd (Groww lane only; Groww documents no rate rule, and the retry is
bounded to exactly 1 per leg per cycle — never a storm); (b) deferred
fallbacks run CONCURRENTLY with the deliberately-sequential verdict-fallback
pass — they target DIFFERENT legs by construction (a leg is either
verdict-failed or in-flight-skipped, never both), so no duplicate fire is
possible, but this is a documented deviation from the strict
second-1/second-2 fallback shape.

## §3d. Any-failure ladder arming — SUPERSEDED 2026-07-16 (RateLimited-only arming; the F7 amplitude-1 oscillation is retired with the anchor ladder)

*(Superseded 2026-07-16 by the §0b operator corrections — retained as dated
history per the house convention; the paragraph below described the
2026-07-15 contract, which is NO LONGER CURRENT.)* The F7 finding
(2026-07-15) recorded the then-current contract of the RETIRED pre-close
ANCHOR ladder: it armed on ANY arming failure in the cycle even when an
in-cycle retry recovered the leg, and under a perfectly alternating
fail/clean minute pattern this yielded an accepted PERMANENT AMPLITUDE-1
rung oscillation (0 ↔ 1, one `ladder_shift` pair per two minutes), pinned
by the then-test `test_ladder_any_failure_arming_amplitude_1_oscillation`
(DELETED with the anchor ladder). BOTH halves are superseded by the
2026-07-16 operator corrections (§0b): (a) arming is **RateLimited-ONLY**
(*"rate limited alone alone fallback"* — Timeout / Transport / Empty /
Auth / Malformed / QueueDelay never reshape), and (b) the shape ladder's
2-CONSECUTIVE-dirty streak threshold means an alternating pattern never
reshapes AT ALL — the replacement pin
`test_dhan_shape_ladder_alternating_pattern_never_degrades` (`ladder.rs`)
proves the OPPOSITE of the old contract (the shape holds rung 0 forever
under alternation); the deleted test's name is retained above as history
only, never a live citation. The "recovered-in-cycle does not arm"
refinement the F7 flag awaited is MOOT under the new contract at the
CYCLE level (a RateLimited leg whose bounded retry recovered still marks
the cycle dirty — the operator's "tried that multiple times" counts
attempts, not final outcomes); the 2-dirty/3-clean streak thresholds
remain **Assumed** pending operator confirm (the flag lives on the
`concurrency_*` config docs), and the RS1(b) per-day rung-0 re-entry cap
(§0b) bounds the residual streak-driven oscillation.

## §3e. ONE-SOURCE-OF-TRUTH DELEGATION (2026-07-15, pending)

The day-locked expiry store (`crates/core/src/cadence/expiry.rs::
DayLockedExpiryStore`, process-global via `global_expiry_store()`) is the
SINGLE source of truth for the per-day current-expiry decision. THREE
pre-existing sites still resolve expiry independently and MUST delegate to
the store in follow-up PRs (each is a behavior-preserving read-path swap;
none may keep a private "nearest expiry" rule once delegated):

| # | Duplicate site | What it does today |
|---|---|---|
| 1 | `crates/app/src/option_chain_1m_boot.rs::select_current_expiry` (~:206) | Dhan chain leg's day-start expirylist warmup picks nearest ≥ today |
| 2 | `crates/core/src/feed/groww/instruments.rs::select_current_option_expiry` (~:677) | Groww chain leg resolves nearest ≥ today from the daily master CSV |
| 3 | `crates/app/src/groww_contract_1m_boot.rs` (own master download ~:1826 + call ~:398 + books cache ~:1918) | the contract leg re-downloads the master and re-resolves independently |

NOTE the policy split: sites 1–3 currently apply nearest-active-date to ALL
underlyings — the store's BANKNIFTY month-last policy is the operator-locked
correction; delegation must NOT preserve the flat-min behavior for
BANKNIFTY. `crates/core/src/instrument/index_futures.rs`'s monthly selector
is a DISTINCT instrument class (index FUTURES serials, §36.7 all-months) —
its month rule stands and must not be conflated with (or contradicted by)
the option-chain month policy here.

## §3f. Flagged follow-ups from the round-4 rebase verdict (dated 2026-07-16)

Recorded at the post-#1540 rebase (verifier round-4 verdict) — not a
silent-failure class; a belt on an already-LOUD arm:

1. **`validate()` still admits cross-cycle overlap** — RESOLVED by the
   2026-07-16 reshape: the pre-close chain instants and the free spot
   offset knob are retired; `validate()` now pins `dhan_burst_offset_ms`
   within (0, cutoff) AND requires the deepest possible spot bucket + a
   full window to land before `dhan_lane_cutoff_ms` (≤ T+15s), so no fire
   band can cross into the sibling cycle's band. Retained here as the
   dated history of the round-4 finding.

(The verdict's second item — the graceful-shutdown boot-wiring guard
scanning the WHOLE of main.rs instead of its production region — was
IMPLEMENTED in the same 2026-07-16 pass: the guard now splits main.rs at
the first column-0 `#[cfg(test)]` line, the house production-region
pattern, so a test-module mention of the notify call can never satisfy or
double-count the production pin.)

## §4. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> zero-429 property is STRUCTURAL — every Dhan chain fire (primary, retry, at
> either shape rung) passes its pure per-(underlying, expiry) CAS
> min-spacing gate (the SOLE chain budget under the 2026-07-16 two-bucket
> model), and every Dhan spot + expiry-list fire passes both the
> rolling-1000ms spot window (≤ `spot_window_cap` per sliding second) and
> the combined cap-5 spot+expiry rolling-second ring (the 2026-07-16
> binding cadence-lane Data-API pacing), all in the MONOTONIC domain, or
> defers; the deterministic replay proptest
> (`crates/core/tests/cadence_zero_429_replay.rs`) drives 64-cycle days through
> skew/jitter/failure/restart permutations — INCLUDING every (Dhan shape
> rung × concurrency tier × Groww shape) transition — and asserts zero
> per-(underlying, expiry) spacing violations, never
> more than `spot_window_cap` spot fires (nor 5 combined spot+expiry Dhan
> fires) in ANY rolling 1000ms window,
> zero nominal-slot denials, exactly 1 decision per (lane, cycle),
> exactly-once snapshot publication per successful chain fetch, and a
> non-vacuous 64-full-cycle activity floor. The 'no DECIDED outcome past the
> lane cutoff' invariant is enforced RUNNER-SIDE (TRH-R2-1 truth-sync,
> 2026-07-15): the finalize guard routes through the pure, unit-pinned
> `decision::may_decide_at_completion` (boundary tests in decision.rs) and
> the call site + early return are source-scan-ratcheted by
> `cadence_composition_contract_guard.rs::test_cadence_finalize_pins_never_a_late_decided_guard`
> — the replay proptest's matching assert exercises its own honest MIRROR of
> that guard (labeled as such in the sim), never the runner code path itself.
> NOT claimed:
> that the BROKER never 429s — a shared-budget co-tenant (BruteX) or a
> broker-side tightening can still produce one, which is typed
> `rate_limited`, arms the ladder (the sole arming class), and gets ONE
> bounded in-cycle retry through the gates — never a storm; that dry-run
> emits decisions — the `DryRunLoggingExecutor` returns Empty by design and
> every dry-run minute honest-skips; that the wall-clock targets are exact —
> `late_wake`/`boundary_skipped` are measured and coalesced, never hidden. The
> real broker executors (and the dated rule-file re-authorization for their
> decision-path fires) land in a LATER PR — this PR ships timing machinery
> only, no REST caller. Expiry resolution (2026-07-15) is day-locked +
> TASK-respawn-proof (process-global store keyed on the IST trading day via
> `trading_calendar::ist_offset()`; the read facade is DAY-CHECKED — E1,
> 2026-07-15 — so a failed morning re-resolution serves None, never a stale
> prior-day winner); its policy math is pure + property-
> tested (BANKNIFTY month-last never flat-min; expiry-day holds through
> close then rolls; garbage/empty lists fail closed to None). NOT claimed:
> PROCESS-restart-proofness — the in-memory day lock re-resolves on a
> mid-session crash-boot (the E2 residual + flagged persisted-lock
> follow-up in §0). NOT claimed:
> that the vendor lists are correct — a wrong upstream list resolves to a
> wrong-but-loud date (the disagreement arm catches cross-broker splits;
> a same-wrong-both-sides list is invisible by construction)."

## §5. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `Cadence0*` variant)
- `crates/core/src/cadence/` (any file)
- `crates/app/src/cadence_boot.rs`
- `crates/common/src/config.rs` (`CadenceConfig`)
- `config/base.toml` `[cadence]`
- Any file containing `CADENCE-01`, `CADENCE-02`, `CADENCE-03`,
  `Cadence01LaneDegraded`, `Cadence02DecisionSkipped`,
  `Cadence03SchedulerDegraded`, `MinSpacingGate`, `DhanGates`,
  `CadenceExecutor`, `spawn_supervised_cadence_runner`, `tv_cadence_`,
  `DayLockedExpiryStore`, `global_dhan_gates`, `global_expiry_store`,
  `resolve_policy_expiry`, or `QueueDelay`
