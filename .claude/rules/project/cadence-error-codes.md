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
  structural floor, monotonic domain).
- **Dhan spots** (operator spec addition 2026-07-15) fire as 4 single-symbol
  calls grouped by the ADAPTIVE CONCURRENCY LADDER — step 0: all 4
  SIMULTANEOUS at T+3000; degraded steps split 3+1 / 2+2 / fully sequential
  across 1000ms-spaced group anchors (both brokers' candle endpoints are
  single-symbol-per-request, so step 0 is 4 parallel calls, never one batched
  HTTP request). Every fire passes the spot ROLLING-1000ms-WINDOW gate (≤
  `spot_window_cap`, default 4, hard-bounded 1..=5 — Dhan's 5/sec cap; the
  shared 3 rps `dhan_data_api_limiter` still smooths a 4-simultaneous burst
  to ~3/sec — the window gate is the STRUCTURAL ceiling, the limiter
  defense-in-depth). Degrade one step after 2 CONSECUTIVE spot-dirty
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
| `cross_fill` | the lane's OWN fetch path exhausted (every own fire completed/failed, retries spent) without the cell; the OTHER lane's fresh same-minute data filled it (provenance stamped `CrossSource`). The fallback rungs run only on own-path exhaustion or at the cutoff — never preempting a still-scheduled own fire (design §5 resolution order) |
| `chain_embedded_spot` | third-rung provenance: the chain response's own embedded underlying spot filled the cell (own path exhausted first, as above) |
| `moneyness_unknown` | ≥1 underlying's fold classified Unknown (spot unusable / rows unclassifiable / registry snapshot refused by the decide-time guard: unconfirmed publish, wrong minute, stale, or the boot sentinel) |
| `expiry_unresolved` | ≥1 chain request was stamped `expiry_yyyymmdd = None` — the ExpiryResolver seam (2026-07-15) is unresolved for the (broker, underlying); the scheduler NEVER guesses an expiry (the executor impl may fall back to its warmup expiry). ALWAYS present in dry-run (the day-1 `StubExpiryResolver` is unresolved by design; the real resolution boot phase is a separate follow-up increment) |
| `ladder_exhausted` | the failure ladder hit its max rung (5) — edge-latched ONCE per episode, re-armed by a clean cycle |

**Dry-run note (honest — not an incident):** with the
`DryRunLoggingExecutor` every fire returns Empty, so EVERY enabled dry-run
cycle also emits ONE coalesced CADENCE-01 per lane whose stages are the
empty classes (`chain_empty,spot_empty` on Dhan; `chain_empty,spot_empty,
groww_fallback` on Groww). That is the same honest end-to-end timing
signal as the §2 dry-run skip flood — NOT broker degradation; a real
broker problem shows as `fetch_failed` / `rate_limited` instead.

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
3. Dry-run note: with the `DryRunLoggingExecutor` EVERY cycle skips (`cutoff` —
   Empty results by design) AND emits the §1 dry-run CADENCE-01 empty-stage
   line per lane. Both are the honest dry-run signal that the timing
   machinery runs end-to-end, not an incident.

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
> zero nominal-slot denials, exactly 1 decision per (lane, cycle), no DECIDED
> outcome past the lane cutoff, exactly-once snapshot publication per
> successful chain fetch, and a non-vacuous 64-full-cycle activity floor.
> NOT claimed:
> that the BROKER never 429s — a shared-budget co-tenant (BruteX) or a
> broker-side tightening can still produce one, which is typed
> `rate_limited`, arms the ladder, and is never blind-retried; that dry-run
> emits decisions — the `DryRunLoggingExecutor` returns Empty by design and
> every dry-run minute honest-skips; that the wall-clock targets are exact —
> `late_wake`/`boundary_skipped` are measured and coalesced, never hidden. The
> real broker executors (and the dated rule-file re-authorization for their
> decision-path fires) land in a LATER PR — this PR ships timing machinery
> only, no REST caller."

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
  `CadenceExecutor`, `spawn_supervised_cadence_runner`, or `tv_cadence_`
