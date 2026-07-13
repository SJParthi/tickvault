# Implementation Plan: Groww per-minute REST pipeline — spot 1m + option chain + per-contract 1m

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator) — directive relayed verbatim via the coordinator session, 2026-07-13 (quotes preserved in this plan + the rule amendments: `groww-second-feed-scope-2026-06-19.md` §38, `no-rest-except-live-feed-2026-06-27.md` §9).

## The verbatim operator authorization (2026-07-13 — preserve exactly, typos included)

**Quote 1 (the directive):**
> "can we implement the same Groww one min fetch which is precisely very similar to the same Dhan — REST api pull ohlcv entirely and even then instantly option chain api also... for Groww live feed and now we planned to add this live REST which is very similar to Dhan. That's it."

**Quote 2 (latency visibility):**
> "always clearly note within a second — or within how many seconds precisely — we are fetching this live real OHLCV, along with the option chain API."

**Context 3 (verbatim intent, not a quote — labeled as such):** the purpose is backtest↔live parity of the operator's fill model. BruteX backtests on Groww 1-minute candles with a worst-case fill rule: signal on the current candle → fill at the NEXT minute's worst-case high/low, for BOTH the underlying AND the option leg. Live must recreate exactly that combo from the same data source; therefore per-contract 1m candles for selected option contracts are in scope (the chain endpoint serves strike discovery/liquidity), and the close-to-data latency measurement is load-bearing (the signal→next-minute-fill window depends on it).

## Crates touched

`tickvault-common` (`crates/common/` — constants, config sections, ErrorCode decisions), `tickvault-app` (`crates/app/` — the per-minute schedulers + scorecard surfacing), `tickvault-storage` (`crates/storage/` — persistence writers, the new per-contract table), plus a one-line visibility change in `tickvault-core` (`crates/core/` — expose `stable_index_security_id`, see PR-2).

## Plan Items

- [x] PR-1 (this PR) — rule amendments + this plan + plan archiving (docs-only)
  - Files: .claude/rules/project/groww-second-feed-scope-2026-06-19.md (§33 dated notes + new §38), .claude/rules/project/no-rest-except-live-feed-2026-06-27.md (banner + §3 KEEP rows + §6 triggers + new §9), .claude/plans/active-plan-groww-rest-1m.md (NEW), .claude/plans/archive/2026-07-08-futidx-4.md + .claude/plans/archive/2026-07-10-futidx-allmonths.md (archived from active per plan-enforcement rule 7)
  - Tests: N/A — docs-only (plan-gate exempt class; no `crates/**/src` change)

- [ ] PR-2 — Groww spot-1m leg (per-minute scheduler mirroring `crates/app/src/spot_1m_rest_boot.rs`, with the #1499 lessons (#1499, pending merge) baked in from day one: day-granular request window + client-side minute filter — for Groww a `start_time`/`end_time` DAY window in `yyyy-MM-dd HH:mm:ss` (IST Assumed — Indian-exchange convention, confirm live), filtering for the target minute client-side; flush-confirmed persist watermark; one-minute-lookback backfill in every ladder outcome arm; one bounded 15:31 post-session sweep). SEQUENCING: PR-2 lands AFTER #1499 merges (or rebases onto it). Endpoint `GET https://api.groww.in/v1/historical/candles`, `candle_interval="1minute"`, `groww_symbol` identity (`NSE-NIFTY` / `NSE-BANKNIFTY` / `BSE-SENSEX`, segment `CASH`), `Authorization: Bearer <shared-minter token via the existing fetch_groww_access_token SSM read-only path>` + `x-api-version: 1.0`. Rows land in the EXISTING `spot_1m_rest` table tagged `feed='groww'` (the §2 same-tables + feed-in-DEDUP-key model; the key already carries `feed`), using the SAME `security_id` values the Groww live lane uses for these indices (`stable_index_security_id` — currently PRIVATE in crates/core/src/feed/groww/instruments.rs; PR-2 exposes it via pub(crate)/a public accessor to reuse the exact live-lane ids). Per-fetch close-to-data latency: per-row `close_to_data_ms` column + histogram `tv_groww_spot1m_close_to_data_ms`. Config: `[groww_spot_1m]` gated, serde default OFF, base.toml opts in.
  - Files: crates/app/src/groww_spot_1m_boot.rs (NEW), crates/app/src/main.rs, crates/common/src/constants.rs, crates/common/src/config.rs, crates/storage/src/spot_1m_rest_persistence.rs (feed='groww' row support), crates/core/src/feed/groww/instruments.rs (expose stable_index_security_id — visibility only), config/base.toml
  - Tests: unit tests on the pure scheduler/window/filter/backfill fns (the spot_1m_rest_boot pattern), a wiring guard test (spot_1m_rest_wiring_guard pattern), config-default-off test, ErrorCode cross-ref satisfied in-PR

- [ ] PR-3 — Groww option-chain leg: `GET /v1/option-chain/exchange/{exchange}/underlying/{underlying}?expiry_date=...`, 3 underlyings (NIFTY/BANKNIFTY NSE, SENSEX BSE; the `underlying` path param is the PLAIN symbol — NOT groww_symbol), CURRENT expiry resolved from the ALREADY-INGESTED daily Groww instruments CSV (no new expiry endpoint needed — zero rate cost), sequenced after the spot leg (watch-signal + fallback timer, the Dhan pattern), rows into the EXISTING `option_chain_1m` table `feed='groww'`, own min-gap rate limiter, latency histogram `tv_groww_chain1m_close_to_data_ms` + per-row latency column. Config `[groww_option_chain_1m]`, default OFF pending the first live probe (the endpoint is documented-available, unlike Dhan's entitlement question, but first-live-session verification still gates the default).
  - Files: crates/app/src/groww_option_chain_1m_boot.rs (NEW), crates/app/src/main.rs, crates/common/src/constants.rs, crates/common/src/config.rs, crates/storage/src/option_chain_1m_persistence.rs (feed='groww' row support), config/base.toml
  - Tests: pure parse tests on the strikes-map response (strike-string keys, `Option<>` CE/PE), expiry-from-CSV selection tests, sequencing tests, wiring guard, config-default-off test

- [ ] PR-4 — per-contract 1m candle leg (the fill-model leg): same `/v1/historical/candles` endpoint with `segment=FNO` and `groww_symbol` like `NSE-NIFTY-04Jan24-19200-CE`, for a BOUNDED selected set of active contracts (selection fed by the chain snapshot / instruments master; envelope cap on contracts per minute), NEW table (name proposal: `option_contract_1m_rest`) with `feed` in the DEDUP key + retention/partition-manager registration. Config-gated default OFF.
  - Files: crates/app/src/groww_contract_1m_boot.rs (NEW), crates/storage/src/option_contract_1m_rest_persistence.rs (NEW), crates/common/src/constants.rs, crates/common/src/config.rs, config/base.toml
  - Tests: DDL/DEDUP-key tests (dedup_segment_meta_guard compliance), selection-envelope cap tests, groww_symbol builder tests, wiring guard

- [ ] PR-5 — latency visibility surfacing (Quote 2): plain-English per-feed per-leg line on the 15:45 dual-feed scorecard / EOD digest ("Groww spot 1m: fetched median X.Xs / p99 Y.Ys after close; chain: median Z.Zs" — and the Dhan leg's line for parity), + dashboard gauge. Insertion point: `crates/app/src/feed_scoreboard_boot.rs` (summary struct ~:2428/:2496, `DualFeedDailyScorecard` variant + render arm in events.rs), following the PR-C keep-better/−1-sentinel honesty conventions.
  - Files: crates/app/src/feed_scoreboard_boot.rs, crates/core/src/notification/events.rs
  - Tests: scorecard-line render tests, keep-better guard tests, −1-sentinel honesty tests

## Design

Mirror the Dhan per-minute REST pipeline (spot_1m_rest_boot.rs + option_chain_1m_boot.rs + their persistence modules — the proven template incl. the #1499 hotfix lessons) onto Groww, as a cold-path scheduled fetcher family in `crates/app/`, persistence in `crates/storage/`, constants/config in `crates/common/`.

- **Rate budget:** Groww documents type-level limits — **Live Data 10/sec + 300/min** (Orders 10/s+250/min; Non Trading 20/s+500/min; NO per-day cap documented). The `/historical/*` and `/option-chain/*` buckets are UNNAMED in the docs table → conservatively ASSUMED Live Data. Our in-session load ≈ 3 spot + 3 chain + a bounded contract set ≈ **~6–12 requests/min** — far inside 10/sec + 300/min even on the shared bucket. Per-key vs per-token vs per-account limit scoping is officially UNSTATED (Unknown); both BruteX and TickVault present the ONE shared daily minter token, so the bucket is EFFECTIVELY shared for us either way (Assumed inference from the single shared token, not a documented fact). BruteX's bulk historical pulls are nightly/post-market — time-disjoint from our in-session per-minute pulls (Assumed; coordinate before relying on tighter headroom).
- **Capacity verdict (2026-07-13 dedicated docs-research session — 26 official pages + growwapi 1.5.0 wheel; cite the refreshed `docs/groww-ref/` pack for constants once its PR merges):**
  - Rate buckets are TYPE-LEVEL POOLED — exhausting one API throttles the whole type. Numbers re-confirmed: Live Data 10/s + 300/min; Orders 10/s + 250/min (changed 15→10 between Dec'25 and Mar'26 — the numbers CAN change, re-verify on the box); Non-Trading 20/s + 500/min; NO per-day cap officially.
  - 429 semantics: the only official guidance is "throttle the request rate" — NO documented Retry-After / ban / cooldown. The SDK has ZERO client-side throttling and infinite default timeouts, so pacing + timeouts are entirely OURS.
  - Minute-boundary pacing rule (binds PR-2/3/4): spread each boundary burst to ≤6 req/s against the shared 10/s ceiling (BruteX co-tenancy on the same token — its ~3 req/s sustained is an operator-relayed empirical anchor; its nightly timing is Assumed, not verified).
  - Worst-case load ~18 req/min (3 spot + 3 chain + ~12 contracts) ≈ 6% of the 300/min budget solo; ~66% if BruteX runs in-session — still inside.
  - Token: the ~06:00 IST daily expiry is OFFICIALLY documented (upgraded from assumption).
  - Historical 1m per-request range cap = 30 days — our day-granular windows are far inside.
- **Sequencing:** PR-2 lands AFTER Dhan hotfix PR #1499 merges (or rebases onto it) — the day-window + backfill + sweep patterns PR-2 mirrors live in that PR (pending merge at plan time).
- **Live-probe list (the PR-2/3/4 instrumentation MUST capture — mirrored in Observability):** (a) just-closed-minute freshness distribution per index (poll-until-present ladder latency); (b) in-progress-day serving confirmation; (c) which bucket `/v1/historical/*` and `/v1/option-chain/*` actually share (429 correlation vs live-data volume — undocumented, verified-by-omission); (d) per-endpoint response-time histograms + chain payload byte size / strike count per underlying; (e) every 429's timestamp / endpoint / body shape / Retry-After presence / recovery time; (f) minute-boundary burst tolerance (does 10-in-1s pass); (g) per-API-key vs per-account limit scoping.
- **UNVERIFIED-LIVE items (the first live session is the probe):** (1) just-closed-minute freshness — no documented availability delay for the sealing minute; NEVER assert "within a second", always show the MEASURED close-to-data number (Quote 2); (2) the V2 candle timestamp type (`yyyy-MM-dd HH:mm:ss` string vs epoch — parse defensively for both); (3) which rate bucket `/historical/*` + `/option-chain/*` count against (watch for 429 / `GrowwAPIRateLimitException` behavior).
- **Token:** shared-minter SSM READ-ONLY via the existing `fetch_groww_access_token` (`/tickvault/<env>/groww/access-token`) — NEVER mint, never cache past an auth failure, per `groww-shared-token-minter-2026-07-02.md`. Headers: `Authorization: Bearer <token>` + `x-api-version: 1.0`.
- **Identity:** the CANDLE legs (spot + contracts) take `groww_symbol` (NOT exchange_token, NOT bare trading symbol): indices `NSE-NIFTY` / `NSE-BANKNIFTY` / `BSE-SENSEX` segment `CASH`; contracts `NSE-NIFTY-04Jan24-19200-CE` segment `FNO`. The CHAIN endpoint's `underlying` path param is the PLAIN symbol (`NIFTY` / `BANKNIFTY` / `SENSEX`), NOT groww_symbol. Persisted `security_id` = the SAME ids the Groww live lane uses (`stable_index_security_id` for indices; `exchange_token` for contracts) so cross-source joins work; `feed='groww'` on every row, `feed` in every DEDUP key (I-P1-11 + feed-in-key).
- **Error codes:** REUSE the SPOT1M-01/02 + CHAIN-02/03 code strings with a `feed` field on the emit where the semantics match (same stage taxonomy, same 3-minute FailureEdge pager); NEW GROWW-specific variants ONLY where semantics diverge (e.g. there is NO Dhan-style entitlement probe — the Groww chain endpoint is documented-available, so no CHAIN-01 analog is expected). Final call lands in PR-2/PR-3 with any new variant's rule-file mention in the SAME PR (the `error_code_rule_file_crossref.rs` contract).
- **Out of scope:** GDF (parked — its own plan/trial per `gdf-third-feed-scope-2026-07-13.md`); any bulk/backtest history sweep (§33's ban otherwise stands — §38.1); volume classification in any verdict; strategy/order wiring (§28 boundary).

## Edge Cases

- Just-closed minute not yet sealed at first poll → bounded in-minute re-poll ladder (spot leg), day-window + client-side minute filter (#1499 pattern) so a late-sealing vendor is recovered by the next fire's one-minute backfill + the 15:31 sweep.
- Zero-trade minutes on thin option strikes: whether Groww emits gap-filled or ABSENT candles is Unknown — a missing contract minute is counted + reported, never fabricated.
- Timestamp format ambiguity (string vs epoch) → defensive dual parse, fail-loud on neither matching.
- Groww's ~06:00 IST daily token reset mid-warmup → SSM re-read on auth-class failure at ≥60s pacing (never mint).
- Expiry day: current expiry from the instruments CSV holds through the session (nearest ≥ today; never-roll — the index_futures precedent).
- Duplicate rows + 21-column header in the instruments CSV → parse by header name, dedup on `(exchange, exchange_token)` (already the ingested master's discipline).
- 429 / rate-limit: counted, never retried past the ladder; min-gap pacing on the chain/contract legs.
- Clock steps / suspend: `fire_is_fresh` staleness gate + missed-boundary accounting (the Dhan scheduler's H1/H2 invariants, reused).

## Failure Modes

- Fetch degraded (transport / non-2xx / empty / budget overrun) → per-minute coalesced coded log + the 3-consecutive-fully-failed-minutes edge-triggered HIGH Telegram page; persist-gated OK (a fetched-but-never-persisted minute is NOT ok).
- Persist failed (ensure-DDL / append / flush ACK reject) → discard-pending (poisoned-buffer defense), counters, feeds the failure edge; rows re-fetchable + DEDUP-idempotent.
- Task death → supervised respawn with bounded backoff (release `panic="abort"` honesty: respawn arms are unwind-build paths).
- Rate-limit storm (shared-bucket contention with BruteX) → 429 counter + the edge pager; never tighter polling in response.
- QuestDB down → the same fail-soft class as the Dhan legs: gaps in the REST tables only; live WS capture unaffected.
- Groww-only sessions: unlike the Dhan legs (Dhan-gated spawn), the Groww legs must spawn from the Groww lane — a Dhan-off session still runs them (design detail finalized in PR-2).

## Test Plan

- Pure-fn unit tests for every scheduler/window/filter/backfill/selection/parse primitive (the spot_1m_rest_boot test pattern); proptest on the response parsers (hostile bodies).
- Wiring guard tests per leg (spawn-site + no-TCP-ILP + config-gate pins — the existing `*_wiring_guard.rs` pattern in crates/app/tests/).
- DDL/DEDUP tests for the new `option_contract_1m_rest` table (dedup_segment_meta_guard compliance: ts first, segment + feed in key).
- Config default-OFF tests for all three `[groww_*]` sections.
- ErrorCode cross-ref + tag-guard tests pass in the same PR as any new variant.
- Scoped per `testing-scope.md`: `cargo test -p tickvault-app -p tickvault-storage`; `crates/common/` changes escalate to workspace.

## Rollback

- PR-1 (docs): `git revert` of the single docs commit restores the §33/§1 bans verbatim (dated-note pattern — no text was deleted).
- PR-2..PR-5: each leg is config-gated serde-default-OFF — flipping the base.toml key OFF restores byte-identical prior behavior with zero code change; full rollback = `git revert` of the leg's commits (new tables are additive; no schema mutation of existing tables beyond feed-tagged rows, which DEDUP-isolate).
- No feature interlocks: spot / chain / contract legs are independently disable-able; the Dhan legs are untouched.

## Observability

- Histograms: `tv_groww_spot1m_close_to_data_ms`, `tv_groww_chain1m_close_to_data_ms` (+ per-request fetch-duration histograms) — the Quote 2 mandate; per-row `close_to_data_ms` columns stamp the real delay including backfilled/swept rows (never sampled into the own-fire histogram — the #1499 semantics split).
- Counters per leg: fetch outcomes {ok, empty, error, rate_limited}, rows written/discarded, backfilled/sweep counters, task respawns {reason}, boundary-skipped.
- Coded `error!` with stage taxonomy + the edge-triggered HIGH Telegram pages (typed events) + Info recovery events; delivery boundary honest: log-sink-only for the codes, the typed Telegram is the operator page (the §3 rest-1m precedent).
- PR-5: plain-English daily latency lines on the 15:45 scorecard + EOD digest (per-feed, per-leg, median/p99 seconds after close) + dashboard gauge — the operator-visible answer to "within how many seconds precisely".
- Forensic ground truth: the `spot_1m_rest` / `option_chain_1m` / `option_contract_1m_rest` tables themselves (`questdb_sql` counts).
- **Live-probe instrumentation (mandatory in PR-2/3/4 — the Design live-probe list):** (a) just-closed-minute freshness distribution per index (poll-until-present ladder latency); (b) in-progress-day serving confirmation; (c) the actual rate-bucket sharing of `/v1/historical/*` + `/v1/option-chain/*` (429 correlation vs live-data volume); (d) per-endpoint response-time histograms + chain payload byte size / strike count per underlying; (e) every 429's timestamp / endpoint / body shape / Retry-After presence / recovery time; (f) minute-boundary burst tolerance (does 10-in-1s pass); (g) per-API-key vs per-account limit scoping.

## Per-Item Guarantee Matrix

The canonical 15-row + 7-row matrices from `per-wave-guarantee-matrix.md` apply to EVERY item in this plan; the project-specific fills:

### 15-row 100% Guarantee Matrix

| Demand | This plan's proof artefact |
|---|---|
| 100% code coverage | ratcheted per-crate floors (`quality/crate-coverage-thresholds.toml`); PR-2..5 add tests with every module |
| 100% audit coverage | rows in `spot_1m_rest` / `option_chain_1m` / `option_contract_1m_rest` with DEDUP UPSERT KEYS incl. `feed` |
| 100% testing coverage | unit + proptest + wiring-guard + config-gate tests per leg (22-category mapping declared per PR) |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan gates on every push |
| 100% code performance | cold-path only — NO hot-path change; no DHAT/Criterion delta claimed (N/A — flagged honestly) |
| 100% monitoring | histograms + counters + gauges per leg (Observability section) |
| 100% logging | every failure path `error!` with `code = ErrorCode::X.code_str()` + stage field |
| 100% alerting | edge-triggered typed HIGH Telegram at the 3-minute escalation edge + Info recovery |
| 100% security | token via SSM read-only `Secret` path; never logged, never in URLs; secret-scan gates |
| 100% security hardening | body caps + redirect Policy::none() + host pinning (the csv_downloader §18 / socket_token patterns) |
| 100% bugs fixing | adversarial 3-agent review before + after each code PR |
| 100% scenarios covering | Edge Cases + Failure Modes sections enumerate the failure classes; sweep/backfill close the vendor-late class |
| 100% functionalities covering | every new pub fn has a call site + test (pre-push gates 6+11) |
| 100% code review | 3-agent pass on each PR diff, before AND after impl |
| 100% extreme check | ratchet tests (wiring guards, DEDUP meta-guard, config-default pins) fail the build on regression |

### 7-row Resilience Demand Matrix

| Demand | This plan's honest envelope |
|---|---|
| Zero ticks lost | UNTOUCHED — this pipeline never writes `ticks`/`candles_*`; the WS capture chain is not modified |
| WS never disconnects | UNTOUCHED — no WS change; REST legs are cold-path siblings |
| Never slow/locked/hanged | cold-path scheduled tasks; every request timeout-bounded; per-leg hard budgets; zero hot-path allocation added |
| QuestDB never fails | ABSORB: discard-pending + DEDUP-idempotent re-fetch; gaps loud, never silent |
| O(1) latency | N/A on hot path (no hot-path change); per-fire work is bounded-constant (3 indices + capped contract set) — flagged honestly, not claimed O(1) globally |
| Uniqueness + dedup | `(ts, security_id, exchange_segment, feed)`-class DEDUP keys on all three tables (I-P1-11 + feed-in-key) |
| Real-time proof | close-to-data histograms + per-row latency columns + the daily scorecard line = the measured, never-asserted freshness number |

Honest claim wording: any "100% guarantee" in these PRs is qualified "100% inside the tested envelope, with ratcheted regression coverage" per operator-charter §F.

## Scenarios

| # | Scenario | Expected |
|---|---|---|
| 1 | 09:16:00 fire, Groww serves the 09:15 candle on first poll | row persisted, close-to-data latency measured and recorded (per-row column + histogram) — never pre-asserted |
| 2 | Vendor seals the minute late (> ladder) | own-fire counted failed (honest), next fire's backfill or 15:31 sweep repairs; real delay stamped per-row |
| 3 | 429 mid-session | counted, edge feeds, no tighter polling |
| 4 | Token reset at ~06:00 IST | SSM re-read ≥60s pacing; never mint |
| 5 | QuestDB outage window | discard-pending + loud gap; DEDUP-idempotent backfill after recovery |
| 6 | Chain leg first live session | verified live → operator flips `[groww_option_chain_1m] enabled = true` with a dated note |
