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

- **Dhan chains** pre-fire at T−5000 / T−2000 and post-fire at T+2000 (NIFTY /
  BANKNIFTY / SENSEX), every fire passing a per-underlying AND a global CAS
  min-spacing gate (≥3000 ms — the 1-unique-request-per-3s chain rule held as a
  structural floor, monotonic domain). CONSERVATIVE combined-denial behavior
  (R2, 2026-07-15): a chain fire deferred by the 1s COMBINED per-second budget
  re-fires only after the FULL 3s chain spacing — the already-acquired CAS
  spacing slots stay CONSUMED on a combined denial (the carried retry instant
  is the MAX of both constraints) — a deliberate conservative-safe trade (the
  slower re-fire can never violate the broker window), never a violation.
- **Dhan spots** (operator spec addition 2026-07-15) fire as 4 single-symbol
  calls grouped by the ADAPTIVE CONCURRENCY LADDER — step 0: all 4
  SIMULTANEOUS at T+3000; degraded steps split 3+1 / 2+2 / fully sequential
  across 1000ms-spaced group anchors (both brokers' candle endpoints are
  single-symbol-per-request, so step 0 is 4 parallel calls, never one batched
  HTTP request). Every fire passes the spot ROLLING-1000ms-WINDOW gate (≤
  `spot_window_cap`, default 4, hard-bounded 1..=5 — Dhan's 5/sec cap; the
  shared 3 rps `dhan_data_api_limiter` still smooths a 4-simultaneous burst
  to ~3/sec — the window gate is the STRUCTURAL ceiling, the limiter
  defense-in-depth). Since verifier L1 (2026-07-15) EVERY Dhan fire —
  chain, spot AND expiry-list — additionally records into ONE COMBINED
  rolling-1000ms window (cap 5, Dhan's Data-API per-second hard budget)
  inside `DhanGates`, checked-then-recorded atomically, so a chain fire +
  a full spot group (+ an expiry fire) can never jointly exceed 5 Dhan
  requests in any rolling second (the replay ledger asserts the COMBINED
  cap, not just the per-class floors). Degrade one step after 2 CONSECUTIVE spot-dirty
  (rate-limited) cycles; recover one step after 3 consecutive clean cycles
  (both config-keyed, Assumed pending operator confirm).
- **Groww** fires per its THREE-CHOICE fallback-shape ladder (operator
  verbatim 2026-07-15): choice 1 (default) = all 7 requests in parallel at
  T+0 (gate-free lane BY CONSTRUCTION — no Groww arm ever touches the
  gates); choice 2 = :01 all 3 chains / :02 ALL 4 spots (VIX included);
  choice 3 (last resort) = :01 chains / :02 core spots / :03 VIX alone.
  Verdict = last wave + the burst timeout, sequential fallback on failed
  legs; same 2-dirty/3-clean streak rules as the spot ladder. VIX absence
  NEVER blocks the Groww data-complete predicate (coordinator-confirmed
  2026-07-15 — VIX stays advisory).
- **Failure ladder** (rungs 0..=5) shifts the chain slots earlier −1000·rung on
  RateLimited / Timeout / Transport cycles; recovery steps back one rung per
  clean cycle; RateLimited is NEVER blind-retried.
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
  maximally far from the Dhan :55–:05 burst region and the Groww :00–:03
  waves, so a vendor-outage retry cadence can never phase-lock into the
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

**HONEST COMPOSITION WORDING (verifier F6, dated 2026-07-15 — re-located):**
the earlier "never queues" claim about the spot gate vs the shared
`dhan_data_api_limiter` holds at the DEFAULT composition only (window cap 4
against the limiter's 3 rps target). At the limiter's 2 rps FLOOR (a
429-storm step-down) a gate-authorized 4-spot group CAN briefly queue inside
the shared limiter — bounded (≤ ~1s of pacing spill; the limiter smooths, it
never rejects) and harmless (typed `QueueDelay`, non-arming — see the
COMPOSITION CONTRACT below). Stated plainly so the composition is never
mistaken for a violation.

Everything is COLD-PATH (a handful of scheduled fires per minute); the tick hot
path, the WS read loops, the aggregator, and trading are never touched. The
`[cadence]` config ships `enabled = false` — a disabled boot is byte-identical
to today.

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
| `rate_limited` | a broker 429 arrived DESPITE the gates — per-request (rare by construction; every occurrence is a gate-bug signal worth its own line), arms the ladder, NEVER blind-retried |
| `spot_empty` | a spot leg returned 2xx-without-data (the Dhan 200-empty saga class; EITHER lane — dry-run returns Empty on every fire by design, see the dry-run note below); does NOT arm the ladder |
| `chain_empty` | a chain leg returned 2xx-without-usable-data (EITHER lane); does NOT arm the ladder — kept distinct from `fetch_failed` so a 200-empty is never misread as a transport failure |
| `groww_fallback` | the Groww T+800 verdict found failed burst legs — the sequential fallback engaged |
| `cross_fill` | the lane's OWN fetch path exhausted (every own fire completed/failed, retries spent) without the cell; the OTHER lane's fresh same-minute data filled it (provenance stamped `CrossSource`). The freshness floor is LENDER-aware (CADENCE-XFILL-RUNG-1, 2026-07-15): the base T−5000 window widens to the Dhan lender's rung-shifted earliest chain pre-fire (`cross_fill_freshness_floor_ms`), so an anchor-ladder-shifted same-minute Dhan chain is never refused as stale while driving Dhan's own decision; the Groww lender keeps the base floor. The fallback rungs run only on own-path exhaustion or at the cutoff — never preempting a still-scheduled own fire (design §5 resolution order) |
| `chain_embedded_spot` | third-rung provenance: the chain response's own embedded underlying spot filled the cell (own path exhausted first, as above) |
| `moneyness_unknown` | ≥1 underlying's fold classified Unknown (spot unusable / rows unclassifiable / registry snapshot refused by the decide-time guard: unconfirmed publish, wrong minute, stale, or the boot sentinel) |
| `queue_delay` | a fetch was refused by the SHARED `dhan_data_api_limiter`'s queue deadline (SELF-INFLICTED pacing — our own defense-in-depth limiter, not the broker; F1(iii) 2026-07-15). Stage-tagged distinctly, NEVER folded into `fetch_failed`, NEVER arms any ladder |
| `expiry_unresolved` | TWO emission points share this stage: (a) the per-cycle coalesced flag — ≥1 chain request was stamped `expiry_yyyymmdd = None` (the day-locked store has no policy date yet; the scheduler NEVER guesses — the executor impl may fall back to its warmup expiry; ALWAYS present in dry-run, where every expiry-list fetch returns Empty); (b) the resolution loop's EDGE-LATCHED deadline page — ONE `error!` per (broker, underlying) per IST day the instant `expiry_deadline_secs_of_day_ist` (default 08:55) passes unresolved; the lanes run degraded meanwhile and the background retry continues at `expiry_retry_interval_ms` until session end (the deadline gates the PAGE, never the attempts, and a post-deadline BOOT requires ≥2 consecutive failed waves before the page — E4, 2026-07-15; R3, 2026-07-15: waves count REAL dispatched attempts per pair only — a disabled-lane or gate-deferred iteration never advances the threshold). FALLING EDGE (E3, 2026-07-15): a LATER successful resolution for a pair whose page HAD fired emits one coded recovery `info!` (`stage = "expiry_resolved_late"` on the same CADENCE-01 code — no new variant) + `tv_cadence_expiry_resolved_late_total{broker, underlying}`, at most once per pair per day (first write wins) |
| `expiry_disagreement` | both brokers resolved the day's policy expiry for one underlying and the dates DIFFER — **Dhan WINS for keying BOTH lanes** (exchange-sourced expirylist authority); edge-latched ONCE per (underlying, day); both raw dates ride the payload + the store's provenance view (`tv_cadence_expiry_disagreement_total{underlying}`) |
| `expiry_rate_limited` | an expiry-list fetch returned a broker 429 (verifier L2, 2026-07-15 — was `debug!`-only): one coded `warn!` per occurrence + `tv_cadence_expiry_rate_limited_total{broker}`; never blind-retried in-wave — the next `expiry_retry_interval_ms` wave re-attempts THROUGH the gates. Dhan expiry fires pass `DhanGates::try_acquire_expiry` (the L1 COMBINED 5-per-rolling-second budget + a 1-per-rolling-second expiry spacing) BEFORE dispatch, so a Dhan expiry 429 despite the gates is a gate-bug / shared-budget-co-tenant signal; a gate deferral skips the fire to the next wave (`tv_cadence_expiry_gate_deferred_total{broker}` — a deferral, never a violation). Groww expiry fires stay ungated by design (no Groww rate rule) |
| `ladder_exhausted` | the failure ladder hit its max rung (5) — edge-latched ONCE per episode, re-armed by a clean cycle |

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
4. A sustained `ladder_exhausted` (rung pinned at 5) means the broker cannot
   serve even maximally-early fires — the decision quality is already degraded
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
| `ladder_shift` | the failure ladder moved a rung (either direction — the `tv_cadence_ladder_shifts_total{direction}` counter carries which) |
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
4. `gate_deferred_nominal` — do NOT widen spacings; diff the configured chain
   offsets against `chain_min_spacing_ms` (config validation enforces pairwise
   gaps ≥ spacing at boot, so a live occurrence implies a runtime retry
   interleaving bug — file it).

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

1. **Route every Dhan call through the shared `dhan_data_api_limiter`**
   (`crates/app/src/dhan_data_api_limiter.rs` — the 3 rps self-tuning
   process-wide gate, `rest-1m-pipeline-error-codes.md` §2f). The scheduler's
   gates are the STRUCTURAL ceiling; the shared limiter is defense-in-depth.
2. **Record/consult the PROCESS-GLOBAL gate registry**
   (`gate::global_dhan_gates()` — one shared `DhanGates` per process, F1(ii)):
   an executor-side fast path or a second scheduler instance must share the
   SAME budget, or two innocent components jointly exceed Dhan's window. The
   per-underlying 3s chain gate is ALWAYS enforced; when the expiry is known
   the per-(underlying, expiry) stamp is recorded/consulted too — an
   expiry-LESS fire is strictly MORE conservative (subsumption, pinned by
   `test_cadence_gate_expiryless_fire_subsumes_expiry_stamp`).
3. **Type limiter-queue deadline misses as `CadenceFetchError::QueueDelay`**
   — NEVER `Timeout`. A queue delay is SELF-INFLICTED pacing (our own
   limiter), not a broker signal: it is stage-tagged distinctly
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

## §3d. Any-failure ladder arming — Assumed, amplitude-1 oscillation accepted (verifier F7, dated 2026-07-15)

The operator's verbatim rule arms the anchor ladder on ANY arming failure in
the cycle EVEN WHEN an in-cycle retry recovered the leg. Under a perfectly
alternating fail/clean minute pattern this yields a PERMANENT AMPLITUDE-1
rung oscillation (0 ↔ 1, one `ladder_shift` pair per two minutes) — accepted
as the CURRENT CONTRACT and pinned by
`test_ladder_any_failure_arming_amplitude_1_oscillation`; it can never walk
deeper without CONSECUTIVE failing cycles. Flagged **Assumed** pending
operator confirmation of a "recovered-in-cycle does not arm" refinement (the
flag also lives on `failure_arms_ladder`'s doc + the plan file).

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

## §4. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> zero-429 property is STRUCTURAL — every Dhan chain fire (primary, retry, any
> ladder rung) passes a pure CAS min-spacing gate and every Dhan spot fire
> passes the rolling-1000ms-window gate (≤ `spot_window_cap` per sliding
> second), both in the MONOTONIC domain, or defers; the deterministic replay
> proptest
> (`crates/core/tests/cadence_zero_429_replay.rs`) drives 64-cycle days through
> skew/jitter/failure/restart permutations — INCLUDING every concurrency-ladder
> step and shape transition — and asserts zero chain-spacing violations, never
> more than `spot_window_cap` spot fires in ANY rolling 1000ms window,
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
> `rate_limited`, arms the ladder, and is never blind-retried; that dry-run
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
