# Implementation Plan: Cadence native retry hedge (Dhan open-minute empties + REST latency)

**Status:** APPROVED
**Date:** 2026-07-20
**Approved by:** Parthiban (operator standing pre-authorization — "every plan file any session writes for work I have ordered (treat its Status as APPROVED by me the moment it exists)". Build directive, coordinator-relayed 2026-07-20, verbatim intent: "instead of immediately cross-filling from Groww, retry the missing broker's pull until the 4th second; if still not available, THEN use the cross-fill." Scope-absorb: the late history re-pull deliverable of the dead session cse_016srEF2BdW3Vn93UnsNJXeK folds into this lane.)

**Crates touched:** `crates/common` (constants.rs, error_code.rs), `crates/core` (cadence runner.rs / assembly.rs / executor.rs), `crates/app` (dhan_cadence_executor.rs, groww cadence executors, cadence_boot wiring). Rule file: `.claude/rules/project/cadence-error-codes.md`.

## Design

**The problem (measured 2026-07-20):** Dhan REST cycles run p50 1077 ms / p90 1152 / p99-max 4019 (n=88) vs Groww p50 308 / p90 505 / max 1798 (~3.5x). Dhan served the 09:15 + 09:16 IST open minutes as HTTP-2xx EMPTY (zero candles); both were resolved by cross-fill ~+19.4 s after close. There is NO per-leg REST latency instrumentation (only whole-cycle per-lane latency_ms), so the latency floor cause is undeterminable today.

**The hedge (the operator's directive):** when a lane's volley (fired ~T+1.0 s after minute close) resolves a leg as 2xx-EMPTY (~T+1.3 s), the runner no longer cross-fills immediately. Instead it runs BOTH arms in parallel:

1. **Native micro-retries** of ONLY the missing legs/SIDs at pinned instants ~T+2.0 s, ~T+3.0 s, final ~T+3.8 s after minute close (max 3 attempts per leg per minute). Each retry re-asks the SAME broker via the SAME executor leg fn. Pacing law: every retry attempt passes through the cadence gate's `try_acquire(now_monotonic_ms)` / `try_acquire_chain` with a FRESH timestamp (gate.rs:68/:325) — cadence legs remain mechanically BANNED from the legacy shared `shared_dhan_data_api_limiter()` (the ban at dhan_cadence_executor.rs:9 + crates/core/src/cadence/executor.rs:23,:220-225, pinned by dhan_data_api_limiter_wiring_guard.rs, is UNTOUCHED). A gate Busy verdict SKIPS that rung (no spin, no reschedule). ANY 429 from the broker ABORTS all remaining retries for that lane's minute immediately — never a storm.
2. **Cross-fill source prep continues unchanged** — the existing cross-fill arm (runner.rs ~L2774-2785: `cross_fill_freshness_floor_ms` -> `cross_fill_from`) stays armed the whole window.

**Deadline arbitration at T+4.0 s:** if a native retry returned rows before the deadline, the native rows win (preferred) and the slot resolves `native_late_retry`; otherwise the already-prepared cross-fill fires with zero extra wait and the slot resolves `cross_fill`. A leg that was never empty resolves `native_first_try`. Partial-SID recovery: recovered SIDs are native; still-missing SIDs cross-fill at the deadline.

**Resolution enum (the CLOSED cross-session contract):** `CadenceResolution { NativeFirstTry, NativeLateRetry, CrossFill }` with `as_str()` emitting exactly `native_first_try` | `native_late_retry` | `cross_fill`. A groww-fallback-launched leg records `cross_fill` with stage "groww_fallback". The sibling session's `cross_fill_audit` appender is called at the two resolution call sites ONCE its DDL + writer fn signature lands (their table, their writer; append-only, best-effort, never touches the decision path). Until then the tokens ride the coalesced CADENCE-01 stage flags + metrics labels (real call sites — no dead code).

**Pinned constants (crates/common/src/constants.rs, all `pub const`, ratchet-tested):**
- `CADENCE_NATIVE_RETRY_OFFSETS_MS: [i64; 3] = [2_000, 3_000, 3_800]` (from minute close)
- `CADENCE_NATIVE_RETRY_MAX_ATTEMPTS: usize = 3`
- `CADENCE_DECISION_DEADLINE_MS: i64 = 4_000` (formalizes the previously comment-only ~T+4 s deadline; wired where the runner arbitrates, honoring the existing gate/expiry logic at runner.rs:855)
- `CADENCE_HISTORY_REPULL_OFFSETS_MS: [i64; 2] = [30_000, 60_000]`

**Background history re-pull (scope-absorb):** when a slot resolves `cross_fill`, a bounded BACKGROUND task re-asks the degraded broker for that minute at T+30 s and T+60 s (2 attempts, pinned above), upserting that broker's OWN rows via the EXISTING persistence paths + DEDUP keys (idempotent re-append). History-repair ONLY: the task receives NO handle to assembly/decision state, writes NO cross_fill_audit rows, and never touches the decision path — enforced by a NEW build-failing isolation ratchet (source-scan test: the re-pull module must not reference the assembly/decision surface). Paced through the same cadence gate; 429 aborts the remaining attempt.

**Per-leg fetch-duration instrumentation (closes the verdict-2 gap):** each executor leg (Dhan spot/chain/expirylist; Groww spot/chain) measures its own request wall time; the per-lane cycle log gains per-leg `*_fetch_ms` fields, so option-1 hygiene (keepalive/pre-warm) becomes measurable. Client micro-hygiene rides along: the ONE shared cadence reqwest client (built once at dhan_cadence_executor.rs:213) gains explicit `.pool_idle_timeout(Duration::from_secs(120))` + `.tcp_keepalive(Duration::from_secs(30))` — additive insurance so pooled connections survive the ~60 s inter-minute idle; NO pre-warm is built (speculative per verdict 3; the pre-warm budget-accounting tension is a PR-body research note, not code).

**Kill switch:** `[cadence] native_retry_enabled` (serde default TRUE — the operator ordered the hedge ON; base.toml carries it explicitly) and `[cadence] history_repull_enabled` (serde default TRUE). Flipping either false restores the pre-PR shape (immediate cross-fill; no background re-pull).

**CADENCE-04:** new `ErrorCode::Cadence04NativeRetryExhausted` ("CADENCE-04", Medium, auto-triage-safe YES, log-sink-only — NO Telegram, the Dhan noise-lock 4-item family is untouched) — fires once per lane/minute when the bounded native retries exhaust and the slot resolves via cross-fill. Registered per the full checklist: enum after `Cadence03SchedulerDegraded,` (~error_code.rs:1248), code_str arm (~:1473), severity arm (~:1790 region), runbook_path (~:1812 -> cadence-error-codes.md), auto-triage (~:2066), catalog + variant-count bump (~:2364), unit tests mirroring the c1/c2/c3 tests (~:2822-2837), crossref literal `CADENCE-04` + verbatim `Cadence04NativeRetryExhausted` in cadence-error-codes.md.

**Honest envelope (mandatory, verdict-4):** on 2026-07-20 evidence, ZERO Dhan-native recoveries were observed and both open-minute empties resolved via cross-fill at ~+19.4 s — the retry hedge would NOT have saved that day's opens. The hedge is a bounded opportunity window for sub-4 s vendor lag, NOT a guarantee; cross-fill remains the correctness floor and is unchanged. The degraded ~4 s cycles were Dhan-only (4018/4019 ms vs healthy p50 1078 ms); Groww had zero degraded cycles.

## Plan Items

- [ ] Item 1 — Pinned constants + config gates: retry offsets/attempts, decision deadline, re-pull offsets in `crates/common/src/constants.rs`; `[cadence] native_retry_enabled` + `history_repull_enabled` in `CadenceConfig` (`crates/common/src/config.rs`) + `config/base.toml`.
  - Files: crates/common/src/constants.rs, crates/common/src/config.rs, config/base.toml
  - Tests: test_cadence_native_retry_constants_pinned, test_cadence_config_retry_flags_default_on
- [ ] Item 2 — Resolution enum + hedged retry state machine in the runner: `CadenceResolution` with `as_str()` (three verbatim tokens), retry scheduling of only-missing legs at the pinned offsets, deadline arbitration (native-preferred), partial-SID handling, flags/stage strings extended (the coalesced CADENCE-01 struct ~runner.rs:1226-1281; the 200-empty arm ~:2527; the cross-fill arm ~:2774).
  - Files: crates/core/src/cadence/runner.rs, crates/core/src/cadence/assembly.rs
  - Tests: test_cadence_resolution_tokens_verbatim, test_retry_schedule_offsets_from_close, test_deadline_native_beats_cross_fill, test_deadline_cross_fill_when_retries_exhaust, test_partial_sid_recovery_mixed_resolution
- [ ] Item 3 — Retry pacing + 429-abort in the executors: retries reuse the SAME leg fns, pass through gate `try_acquire`/`try_acquire_chain` with fresh timestamps; Busy skips the rung; any 429 aborts the lane's remaining retries; the shared-limiter ban untouched.
  - Files: crates/core/src/cadence/executor.rs, crates/app/src/dhan_cadence_executor.rs
  - Tests: test_retry_paces_through_gate_fresh_timestamp, test_gate_busy_skips_rung_no_spin, test_429_aborts_remaining_retries, dhan_data_api_limiter_wiring_guard (unchanged, re-run)
- [ ] Item 4 — Per-leg fetch-duration instrumentation + client micro-hygiene: per-leg `*_fetch_ms` on the cycle log for BOTH lanes; `.pool_idle_timeout(120s)` + `.tcp_keepalive(30s)` on the shared cadence client.
  - Files: crates/app/src/dhan_cadence_executor.rs, crates/app/src/groww_cadence_executors.rs (actual Groww executor file per tree), crates/core/src/cadence/runner.rs
  - Tests: test_per_leg_fetch_ms_populated, test_cadence_client_pool_settings_pinned
- [ ] Item 5 — Background history re-pull (T+30/T+60) + HARD isolation ratchet: new module (proposal `crates/core/src/cadence/history_repull.rs` or app-side twin per tree), 2 paced attempts, own-broker rows via existing DEDUP-keyed persistence, no assembly/decision references, no cross_fill_audit rows.
  - Files: crates/core/src/cadence/history_repull.rs (NEW), crates/core/src/cadence/runner.rs, crates/app cadence wiring
  - Tests: test_history_repull_schedule_pinned, test_history_repull_dedup_idempotent, cadence_history_repull_isolation_guard (NEW build-failing source-scan ratchet)
- [ ] Item 6 — CADENCE-04 registration + rule/runbook edits: full error_code.rs checklist + `.claude/rules/project/cadence-error-codes.md` gains the CADENCE-04 section (verbatim variant name + code literal) + honest-envelope wording.
  - Files: crates/common/src/error_code.rs, .claude/rules/project/cadence-error-codes.md
  - Tests: error_code_rule_file_crossref, error_code_tag_guard, test_cadence04_code_str, test_cadence04_severity_medium, test_cadence04_runbook_path
- [ ] Item 7 — cross_fill_audit seam readiness: resolution tokens emitted at the two resolution events (retry-recovered + deadline-cross-fill) via logs/metrics now; the sibling's shared appender is wired at exactly these two call sites when their DDL + writer signature lands (this item completes in this PR as the token emission; the appender call is the sibling-coordinated follow-up commit).
  - Files: crates/core/src/cadence/runner.rs
  - Tests: test_resolution_emitted_on_both_events

## Edge Cases

- 429 mid-retry: immediate abort of that lane's remaining rungs; deadline cross-fill proceeds; never a second ask that minute.
- Gate Busy at a rung instant: rung skipped entirely (no spin/reschedule); later rungs fire on their own schedule.
- Native rows land between the last rung (~T+3.8 s) and the deadline (T+4.0 s): native wins (`native_late_retry`).
- Partial recovery (some SIDs only): recovered SIDs native; the rest cross-fill; flags reflect both.
- BOTH lanes degraded the same minute (no fresh cross source): existing empty-minute / groww-fallback behavior unchanged; retries still attempted; cross-fill freshness floor still enforced.
- Ladder interaction: retry attempts are NOT new cycles — the ladder's degrade-not-reshape arms (ladder.rs:281/:288/:407) and rung0_reentry_cap (:50) must see the same cycle counts as today.
- Groww-fallback lane: retries apply to the native broker's leg only; fallback launch (~runner.rs:2021-2075) untouched.
- Session tail (15:29 minute): re-pull offsets may cross 15:30 close — bounded 2 attempts, harmless, runner supervision unchanged.
- Shutdown mid-retry: tasks die with the runner (existing respawn/backoff supervision).
- Background re-pull racing the broker's own later backfill: DEDUP-idempotent upsert — duplicate appends collapse.
- `native_retry_enabled = false`: byte-equivalent to today's immediate-cross-fill shape (regression-pinned).

## Failure Modes

- Retry storm: structurally impossible — hard cap 3 attempts/leg/minute + gate pacing + immediate 429-abort; cap ratchet-tested.
- Decision-path contamination by the background re-pull: HARD isolation — the module holds no assembly/decision handles; the NEW build-failing source-scan ratchet (`cadence_history_repull_isolation_guard`) fails the build on any assembly-type reference.
- Audit-append failure (when wired): best-effort, never touches the decision path (the sibling's contract; their writer owns persistence failure handling).
- CADENCE-04 drift: crossref + tag + catalog-count guards fail the build on any unregistered surface.
- Client-hygiene regression: settings are additive builder calls on the ONE existing shared client (built at dhan_cadence_executor.rs:213, fallible — HTTP-CLIENT-01 degrade class unchanged).
- Instrumentation overhead: cadence is a cold per-minute REST path — two `Instant::now()` per leg; zero hot-path involvement; no allocation added on any tick path.
- Deadline mis-arbitration (double-resolve): single resolution point per slot per minute; unit tests pin exactly-one-resolution across all arms.

## Test Plan

Block-scoped per `.claude/rules/project/testing-scope.md`: `cargo test -p tickvault-common -p tickvault-core -p tickvault-app` (crates/common is touched -> escalate to `cargo test --workspace` before push). Categories from the 22: unit (schedule math, enum tokens, arbitration, 429-abort, gate pacing, partial-SID), integration (runner cycle with injected empty-then-recovered executor fake; cross-fill-at-deadline path), ratchets (constants pinned, isolation guard, limiter-ban guard re-run, error-code crossref/tag/catalog), config (serde default + base.toml flags), regression (kill-switch-off byte-equivalence). All new pub fns carry call sites + tests (pub-fn wiring + test guards). Full pre-push gate battery + `bash .claude/hooks/plan-verify.sh` before declaring done.

## Rollback

Two independent kill switches restore the pre-PR shape without a revert: `[cadence] native_retry_enabled = false` (immediate cross-fill at first empty, exactly today's arm) and `[cadence] history_repull_enabled = false` (no background re-pull). Full rollback = revert the squash-merge commit; no schema/DDL ships in this PR (the cross_fill_audit table is the sibling session's PR), no data migration, stored rows unaffected (re-pull rows are DEDUP-idempotent appends of the broker's own data).

## Observability

- Per-leg `*_fetch_ms` fields on the per-lane cycle log (both lanes) — closes the verdict-2 instrumentation gap.
- Counters: `tv_cadence_native_retry_total{lane, outcome}` (outcome: recovered | exhausted | aborted_429 | gate_busy_skip), `tv_cadence_history_repull_total{lane, outcome}` (recovered | empty | error | aborted_429). Existing `tv_cadence_cross_fill_total{direction}` unchanged.
- CADENCE-04 coded `error!` (log-sink-only, Medium) once per lane/minute on retry exhaustion -> cross-fill; the coalesced CADENCE-01 wrap-up gains the resolution token + retry_attempts fields.
- Resolution tokens (`native_first_try` | `native_late_retry` | `cross_fill`) ride the stage flags now and the sibling's cross_fill_audit rows when wired — giving the operator a per-minute forensic record.
- NO new Telegram pages (Dhan noise-lock 4-item family untouched; CADENCE-04 is log-sink-only by contract).

## Per-Item Guarantee Matrix

Cross-reference per `.claude/rules/project/per-wave-guarantee-matrix.md`: every item above carries the 15-row "100% everything" matrix + the 7-row resilience matrix from that file, instantiated as follows — coverage: new units + ratchets listed per item (coverage delta >= 0); audit: cross_fill_audit rows are the sibling's table (this PR emits tokens; no new table here — no new DEDUP surface); monitoring/logging/alerting: the Observability section's counters + coded CADENCE-04 (log-sink-only by design — alert row N/A per the noise-lock, documented); performance: cold path only, no hot-path allocation (DHAT N/A — not a hot path; documented); security: no new input surface, no secrets, reqwest builder flags only; bugs/review: adversarial hostile review, 2 consecutive clean rounds before push; scenarios: the Edge Cases section enumerated + tested; functionality: every new pub fn has call site + test; extreme check: the isolation ratchet + constants ratchet + limiter-ban guard fail the build on regression. Resilience 7-row: zero ticks lost — N/A (REST cadence lane, no tick path); WS — N/A; never slow/locked — bounded retries, gate-paced, deadline-capped; QuestDB — unchanged persistence paths, DEDUP-idempotent; O(1) — per-slot constant-bounded work (<= 3 retries + 2 re-pulls, pinned); uniqueness/dedup — existing composite DEDUP keys reused verbatim; real-time proof — per-leg instrumentation + counters + resolution tokens.

**Honest 100% claim:** 100% inside the tested envelope, with ratcheted regression coverage: <= 3 gate-paced native retries per leg per minute with immediate 429-abort (constants ratchet-pinned); deadline arbitration at T+4.0 s with the pre-existing cross-fill as the unchanged correctness floor; the background re-pull bounded to 2 DEDUP-idempotent attempts, HARD-isolated from decisions by a build-failing ratchet. NOT claimed: that retries recover today's observed vendor lag (2026-07-20 evidence: zero native recoveries, both open empties cross-filled ~+19.4 s — the hedge is an opportunity window, not a guarantee); NOT claimed: any latency improvement from client hygiene (unmeasured until the per-leg instrumentation lands — that is why it lands).


## 2026-07-20 Amendment — #1690-supersession corrections (MANDATORY; adopted before items 6/2+3)

> Source: coordinator + the PR #1690 owner (PR #1690 CLOSED as superseded by #1693, comment
> 5023436400; branch `claude/cadence-t4s-retry` @ e8c48cc2 retained as a read-only reference).
> Adversarially re-verified; the external verifier holds the same list. Where this amendment
> conflicts with the plan body above, THIS AMENDMENT WINS.

### A. Error code: CADENCE-05, not CADENCE-04 (collision)
Main already carries `Cadence04AuditWriteFailed => "CADENCE-04"` (landed with #1688;
`crates/common/src/error_code.rs:1263` / `:1490`). The plan body's
`Cadence04NativeRetryExhausted` is RENAMED **`Cadence05RecoveryDegraded` => "CADENCE-05"**.
Item 6 registers CADENCE-05 through the full D3 checklist (enum, code_str, severity,
runbook_path, auto-triage, catalog, unit tests) + a triage-yaml entry. The error-code count on
main is **166** (do not copy #1690's stale 165) — item 6 bumps the count assert to 167.

### B. History re-pull offsets: [30_000, 50_000] (NOT 60_000)
T+60s lands exactly ON the next minute boundary and collides with the next cycle's volley burst
(#1690 audit §0d H5). `CADENCE_HISTORY_REPULL_OFFSETS_MS = [30_000, 50_000]` + a pinned
`assert!(offsets.iter().all(|&o| o < 60_000))` boundary assert + an in-code rationale comment.
(Item 1 was in flight with 60_000; corrected by the follow-up commit before the merge-forward.
The config gates `[cadence] native_retry_enabled` / `history_repull_enabled` landed with item 1,
default ON, serde-defaulted.)

### C. Retry eligibility (Malformed NEVER retried)
| Leg outcome at volley | In-window native retry? | Cross-fill eligible? |
|---|---|---|
| 2xx EMPTY (zero candles) | YES (spot ladder; chain see D) | YES |
| 2xx MALFORMED (schema break) | **NEVER** (excluded from retry AND from L3 fallback) | per existing rules |
| HTTP 429 | NO — immediate ladder abort; the existing shape ladder owns it | YES |
| Transport error / timeout | NO — the existing per-leg escalation owns it | YES |

(#1690 pinned this class via `test_late_retry_eligibility_set_pinned`; main now carries #1689's
Malformed-vs-Empty split — the eligibility gate keys on the split enum, never on "no rows".)

### D. Spot-only retry grid; chain = single slot at arbitration
The [T+2.0, T+3.0, T+3.8]s rung grid is **SPOT-ONLY**. Chain legs: the volley consumes the
per-(underlying, expiry) >=3s CAS gate slot at ~T+1.0-1.3s, so the earliest legal chain re-fire
is ~T+4.0-4.3s — at/after the decision deadline. Chain legs therefore get ZERO sub-4s rungs; at
T+4.0s arbitration an empty chain leg resolves via the prepared cross-fill, and AT MOST ONE
gate-paced native chain attempt may fire at/after arbitration whose result is HISTORY REPAIR
ONLY (never the decision). Quote-1's "measure precisely when Dhan serves it" intent is served
for chains by the T+30/50s re-pull + the `resolved_at_ms_after_close` audit column.

### E. Operator quotes (verbatim, typos included — the authority for this plan)
Quote 1 (2026-07-20): "not 4 seconds later dude see if it can't be pulled at the very first second means then second third fourth sequentially without hitting the rate limit right dude see in this case we will easily get to know whether because of our today issues maybe it could have taken one or two or three or fourth second right because if the data is not yet formed till 4th second then our only idea is to find the precisely when til l4 th seockd if the eat is kto deliverable means then we can use the cross reference right dude"

Quote 2 (2026-07-20): "See do you udnerstand my point as what I'm asking bro as such see if spot alone doesn't return the data then only that shoudl be retried toll 4 seconds right dude am I right bro do you understand what I'm askign dude see this shoudl be the same even with option chain right dude If everything can be paralleled by spinning everything in its own parallelism spinning sessions do it as the parallel process dude"

Second clause (2026-07-20, relayed): "retry native until the 4th second, then cross-fill; backfill never feeds live decisions"

### F. Pre-warm: DEFERRED (dated note)
A real HTTP pre-warm against api.dhan.co is a new REST call class — it needs a fresh dated KEEP
row in `no-rest-except-live-feed-2026-06-27.md` §3 FIRST (verifier M13). NOT in this PR. Only
connection-level warmth ships now: `.pool_idle_timeout(120s)` + `.tcp_keepalive(30s)` (item 4).

### G. Non-blocking re-pull gate law (H4/H5)
EVERY attempt (rung, arbitration slot, T+30/50 re-pull) passes `DhanGates::try_acquire_spot` /
`try_acquire_chain` with a fresh monotonic timestamp BEFORE dispatch; budget proofs cite the
DhanGates ring, NOT the retired 3rps limiter. The T+30/50 re-pull runs OUTSIDE the next cycle's
burst window; when the gate refuses, the attempt is SKIPPED + counted (`gate_skipped`) — nominal
traffic ALWAYS wins; a re-pull 429 never arms the shape ladder or rate_limited streaks.

### H. Test-signature budget
The hedge disturbs exact-fire-count expectations in `cadence_fallback_proof`, `runner_dry_run`,
and `zero_429_replay` suites. Update them deliberately (reference shapes: the closed branch
`claude/cadence-t4s-retry` @ e8c48cc2 — reference only, no unadjudicated code copying).

### I. Decision isolation (unchanged, re-affirmed)
Cross-filled + re-pulled rows are record-completeness only — never trading-decision inputs; the
build-failing `cadence_history_repull_isolation_guard` (item 5) + the ratcheted
`may_decide_at_completion` cutoff (TRH-R2-1) stay. `cross_fill_audit` ROWS are owned by
`claude/cross-fill-audit`; this PR lands the seam only.
