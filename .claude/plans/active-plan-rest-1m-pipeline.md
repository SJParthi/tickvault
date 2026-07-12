# Implementation Plan: Per-Minute REST Spot-1m + Option-Chain Pipeline

**Status:** APPROVED
**Date:** 2026-07-12
**Approved by:** Parthiban (operator) — 2026-07-12 directive relayed verbatim via the coordinator session (quote recorded in `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §8)

> **Authority chain:** `no-rest-except-live-feed-2026-06-27.md` §8 (the scheduled-pull
> KEEP-class authorization) > this plan > defaults. Companion rule edits landed in the
> SAME PR as this plan (PR-1, docs-only): the §8 section + two §3 KEEP rows in the
> REST lock, the `⚠ REBUILT 2026-07-12` banner in `.claude/rules/dhan/option-chain.md`,
> the dated notes in `option-chain-cross-verify-error-codes.md` /
> `historical-candles-cross-verify.md`, and rule 12 in `live-feed-purity.md`.
> Code lands in PR-2 (spot) and PR-3 (chain) per the Plan Items below.

## Design

Two sequenced per-minute REST pulls during NSE market hours (09:15–15:30 IST),
implemented across `tickvault-app` (schedulers, boot wiring), `tickvault-storage`
(QuestDB persistence + DDL), and `tickvault-common` (constants, config structs,
error codes):

1. **Spot-1m leg (PR-2, default ON):** at each minute close + ~1s (minute close
   epsilon constant in `crates/common/src/constants.rs`), fetch that minute's
   official 1m OHLCV for the 3 IDX_I spot indices — NIFTY=13, BANKNIFTY=25,
   SENSEX=51 — via Dhan REST `POST /v2/charts/intraday` (interval `"1"` STRING,
   columnar parallel arrays, UTC epoch secs +19800 → IST per
   `.claude/rules/dhan/historical-data.md`). Rows land in a NEW `spot_1m_rest`
   QuestDB table (PARTITION BY DAY, DEDUP UPSERT KEYS `(ts, security_id,
   exchange_segment, feed)` per the I-P1-11 feed-in-key mandate,
   `feed='dhan_rest'`), registered in the partition manager's
   DAY_PARTITIONED_TABLES. Modules: `crates/app/src/spot_1m_rest_boot.rs`
   (scheduler + fetcher, market-hours-gated, trading-day-gated) +
   `crates/storage/src/spot_1m_rest_persistence.rs` (ensure-DDL + ILP-over-HTTP
   writer with per-flush server ACK — the 2026-07-05 fire-and-forget lesson).
2. **Option-chain leg (PR-3, config-gated DEFAULT-OFF):** sequenced immediately
   after the spot leg each minute, pull the current-expiry option chain for the
   same 3 underlyings via `POST /v2/optionchain` (+ a day-start
   `POST /v2/optionchain/expirylist` warmup — expiry NEVER guessed), honoring the
   1-unique-request-per-3s limit (3 DISTINCT underlyings concurrent inside the
   window is allowed per `.claude/rules/dhan/option-chain.md` rule 4), PascalCase
   request fields, `client-id` header, decimal-string strike keys, `Option<>`
   CE/PE. Rows land in a NEW `option_chain_1m` table (PARTITION BY DAY, DEDUP
   UPSERT KEYS `(ts, underlying_security_id, exchange_segment, expiry,
   strike, option_type, feed)`). Modules: `crates/app/src/option_chain_1m_boot.rs`
   + `crates/storage/src/option_chain_1m_persistence.rs`. The chain leg stays
   DEFAULT-OFF pending a first-live-boot entitlement probe (one `expirylist`
   call, outcome reported via Telegram) — the sandbox cannot probe the
   entitlement (dhan.co is proxy-blocked and no access-token is reachable here).

Rate budget: 3 intraday calls + ≤3 chain calls per minute ≈ ≤6 REST calls/min —
trivially inside the Data-API 5/sec + 100K/day budget; DH-904 gets the standard
10→20→40→80s backoff then skip-this-minute (never blocks the next minute).
Both legs are COLD-PATH tokio tasks: no hot-path involvement, no `ticks` /
`candles_*` / `historical_candles` writes ever (live-feed-purity rule 12), no
new WebSocket (2-WS lock untouched), no order path.

## Edge Cases

- **Minute-boundary skew:** the fetch fires at close+1s; Dhan may not have the
  minute published yet — the response's `timestamp` array is filtered to the
  target minute; an absent minute is retried once next cycle (DEDUP makes the
  re-write idempotent) and counted, never fabricated.
- **toDate non-inclusive + over-delivery:** the intraday request window is
  `[minute_open, next_minute_open]`; extra rows Dhan over-delivers are filtered
  by exact minute match before persist.
- **Session boundaries:** first minute (09:15) fires at ~09:16:01 IST; the last
  session minute (15:29) fires at ~15:30:01; NO pull outside [09:15, 15:30) —
  pre-open and post-close are gated by the canonical session gate + trading-day
  calendar (weekends/holidays: zero calls).
- **Expiry-day rollover (chain):** the day-start `expirylist` warmup pins the
  day's current expiry; the expiry NEVER changes intraday (the never-roll house
  precedent); next trading day's warmup advances it.
- **Deep-OTM one-sided strikes:** CE or PE `None` per option-chain.md rule 7 —
  the absent side is skipped, never a deserialization panic.
- **Mid-day restart:** schedulers are stateless per minute; DEDUP UPSERT keys
  make any overlap idempotent; no backfill of missed minutes (the 15:31
  cross-verify + `historical_candles` chain owns full-day parity — this
  pipeline is a capture, not a backfill).
- **Token expiry mid-minute (807/DH-901):** the shared TokenManager handle is
  read per request; a 401-class failure skips the minute and relies on the
  existing renewal machinery — never a mint from this path.
- **Clock skew:** BOOT-03 bounds host skew ≤2s; the +1s epsilon plus the exact
  minute filter absorbs it.

## Failure Modes

- **SPOT1M-01 — spot minute fetch failed** (REST non-2xx / send-leg / parse /
  empty minute after retry): `error!` with `code =` field, counter
  `tv_spot_1m_rest_errors_total{stage}`, skip the minute — the live WS candle
  chain is completely unaffected. Severity::High on a sustained run.
- **SPOT1M-02 — spot_1m_rest persist failed** (ILP-HTTP reject / QuestDB down):
  loud `error!` per the error-level meta-guard (never `warn!`), counted; row
  dropped for the minute (capture table, not the durability floor — the WAL/
  ticks chain is the zero-loss guarantor).
- **CHAIN-01 — option-chain fetch failed**; **CHAIN-02 — DH-904 ladder
  exhausted** (skip cycle); **CHAIN-03 — response parse failure** (schema
  drift); **CHAIN-04 — expirylist warmup failed** (chain leg stays dormant for
  the day, loud page — fail-closed, never a guessed expiry).
- All six codes are NEW `ErrorCode` variants documented in
  `.claude/rules/project/rest-1m-pipeline-error-codes.md`, which lands WITH the
  code PRs (PR-2 adds SPOT1M-01/02, PR-3 adds CHAIN-01..04) so the cross-ref +
  tag-guard tests stay green per-PR. The RETIRED `OptionChain01..08` variants
  are NOT revived.
- **Entitlement absent (chain):** the first-live-boot probe reports the
  verdict via Telegram; the leg stays OFF — no per-minute 401 storm.
- **Scheduler task death:** both schedulers run under the supervised-respawn
  house pattern (WS-GAP-05 / DISK-WATCHER-01 family) with a respawn counter —
  never a silent death.

## Test Plan

- **PR-2 (spot):** unit tests for the pure minute-window builder + response
  filter + IST conversion (+19800) + columnar-array length validation; ensure-DDL
  shape tests pinning the DEDUP key `(ts, security_id, exchange_segment, feed)`
  + PARTITION BY DAY (the `dedup_segment_meta_guard.rs` contract); a
  partition-manager registration test; scheduler gate tests (in-session /
  pre-open / post-close / holiday); wiring ratchet pinning the main.rs spawn
  site; error-path tests for SPOT1M-01/02 emit shapes (tag-guard compliant).
  Scoped run: `cargo test -p tickvault-app -p tickvault-storage`; the
  `tickvault-common` constants/config additions escalate per testing-scope.md.
- **PR-3 (chain):** unit tests for PascalCase request serde
  (`UnderlyingScrip` INT), decimal-strike-key parse (f64, never integer-key),
  `Option<>` CE/PE absence, the 1-per-3s pacing math, expirylist warmup
  fail-closed behavior, DEFAULT-OFF config gate (`#[serde(default)]` = false),
  and the DEDUP key shape for `option_chain_1m`; CHAIN-01..04 emit-shape tests.
- Both PRs: `cargo fmt --check`, clippy, banned-pattern scanner, pub-fn-test +
  pub-fn-wiring guards, the full pre-push gate battery, and the CI All Green
  fan-in before merge (merge-gate-lock-2026-07-04.md).

## Rollback

- **Chain leg:** config toggle OFF (it is the default) — zero behavior change;
  a full revert is a clean `git revert` of PR-3 (isolated modules, no shared
  code paths touched).
- **Spot leg:** `git revert` of PR-2 removes the scheduler + persistence
  modules; the `spot_1m_rest` table remains as inert data (QuestDB tables are
  never dropped by code — operator DROP if desired). No other subsystem reads
  the table in these PRs, so revert is consumer-free.
- **Rule files:** the §8 authorization and companion notes are dated additive
  sections — rollback of the docs is a revert of PR-1; the REMOVE-row
  annotations revert with it.
- No migration, no config-default change for existing keys, no hot-path edit —
  rollback risk is minimal by construction.

## Observability

- Counters: `tv_spot_1m_rest_fetched_total`, `tv_spot_1m_rest_errors_total{stage}`,
  `tv_option_chain_1m_fetched_total`, `tv_option_chain_1m_errors_total{stage}`,
  supervisor respawn counters for both schedulers (static labels only).
- Logs: every failure is `error!` with `code = ErrorCode::X.code_str()`
  (tag-guard); persist failures at `error!` never `warn!` (error-level
  meta-guard); one coalesced line per failing minute — never per-row spam.
- Telegram: the chain entitlement-probe verdict (once, at first enabled boot);
  sustained-failure pages ride the errors.jsonl → CloudWatch log-filter chain
  documented in `rest-1m-pipeline-error-codes.md` (delivery boundary stated
  honestly there — no false "pages immediately" claim before an
  `error_code_alerts` map entry exists).
- Tables ARE the audit surface: `spot_1m_rest` + `option_chain_1m` are
  queryable via `mcp__tickvault-logs__questdb_sql` with DEDUP-keyed forensic
  rows; both registered for DAY partition retention.

## Plan Items

- [x] PR-1 (docs-only, MERGED as #1486): rule-file authorization set + this plan file
  - Files: .claude/rules/project/no-rest-except-live-feed-2026-06-27.md, .claude/rules/dhan/option-chain.md, .claude/rules/project/option-chain-cross-verify-error-codes.md, .claude/rules/project/historical-candles-cross-verify.md, .claude/rules/project/live-feed-purity.md, .claude/plans/active-plan-rest-1m-pipeline.md
  - Tests: N/A — docs-only (plan-gate + per-item-guarantee-check hooks are the verification)
- [x] PR-2 (spot leg, `tickvault-app` + `tickvault-storage` + `tickvault-common`): per-minute spot-1m fetcher + `spot_1m_rest` table + SPOT1M-01/02 codes + rest-1m-pipeline-error-codes.md
  - Files: crates/app/src/spot_1m_rest_boot.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/app/tests/spot_1m_rest_wiring_guard.rs, crates/storage/src/spot_1m_rest_persistence.rs, crates/storage/src/partition_manager.rs, crates/common/src/constants.rs, crates/common/src/config.rs, crates/common/src/error_code.rs, crates/core/src/notification/events.rs, config/base.toml, .claude/rules/project/rest-1m-pipeline-error-codes.md, .claude/triage/error-rules.yaml
  - Tests: test_minute_window_strings_open_plus_60s, test_parse_intraday_columnar_for_minute_utc_to_ist_and_filter, test_select_minute_candle_exact_match_only, test_minute_open_ist_nanos_matches_ist_as_epoch_convention, test_spot_1m_rest_dedup_key_ts_first_segment_and_feed_in_key, test_next_fire_past_window_returns_none, test_day_is_over_non_trading_day_or_past_window, test_fire_walk_covers_exactly_375_minutes, test_day_partitioned_tables_not_empty, ratchet_spot1m_spawn_is_config_gated_inside_post_market_seam, ratchet_spot1m_boot_module_is_not_a_stub
  - As-built deltas vs the original promises (noted in the PR body): the six
    promised test names were placeholders — the real suite above supersedes
    them 1:1 by concern (window builder / minute filter / +19800 / DEDUP key /
    session+trading-day gates / partition registration) plus the 2026-07-12
    adversarial-review additions (same-second re-fire, per-SID minute budget,
    missed-boundary accounting, persist-gated edge, discard_pending, body
    cap). `feed` is stamped `'dhan'` with a separate `source='rest_intraday'`
    SYMBOL label (NOT the originally-sketched `'dhan_rest'`) so the `feed`
    domain stays the canonical `dhan`/`groww` set from `data-integrity.md`;
    error counters landed as `tv_spot1m_fetch_total{outcome}` +
    `tv_spot1m_persist_errors_total{stage}` (not `tv_spot_1m_rest_*`).
- [ ] PR-3 (chain leg, `tickvault-app` + `tickvault-storage` + `tickvault-common`): per-minute option-chain fetcher (DEFAULT-OFF) + `option_chain_1m` table + CHAIN-01..04 codes + entitlement probe
  - Files: crates/app/src/option_chain_1m_boot.rs, crates/app/src/main.rs, crates/storage/src/option_chain_1m_persistence.rs, crates/common/src/constants.rs, crates/common/src/config.rs, crates/common/src/error_code.rs, .claude/rules/project/rest-1m-pipeline-error-codes.md
  - Tests: test_option_chain_request_pascal_case_underlying_scrip_int, test_option_chain_strike_keys_parse_decimal_strings, test_option_chain_ce_pe_optional_absent_side, test_option_chain_pacing_one_unique_per_3s, test_expirylist_warmup_fail_closed_keeps_leg_dormant, test_option_chain_1m_default_off_config_gate

## Per-Item Guarantee Matrix

Every plan item above carries the **15-row "100% everything" matrix** and the
**7-row Resilience Demand Matrix** by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical rows apply
verbatim; enforced by `.claude/hooks/per-item-guarantee-check.sh` +
`make wave-guarantee-check`). Item-specific instantiation highlights:

| Demand (from the 15-row matrix) | This plan's proof artefact |
|---|---|
| 100% code coverage | PR-2/PR-3 unit + emit-shape tests; ratcheted per-crate floors unchanged (`quality/crate-coverage-thresholds.toml`) |
| 100% audit coverage | `spot_1m_rest` + `option_chain_1m` DEDUP UPSERT KEYS tables (segment + feed in key) |
| 100% logging / alerting | tag-guard `code =` fields; error-level meta-guard; runbook `rest-1m-pipeline-error-codes.md` |
| 100% code checks | banned-pattern + pub-fn guards + plan-gate + the 12 pre-push gates on every code PR |
| 100% extreme check | DEDUP-key + partition-registration + gate ratchet tests fail the build on regression |

7-row resilience instantiation: **Zero ticks lost** — no new tick-drop path
(cold-path REST only; ring→spill→DLQ untouched); **WS never disconnects** —
no WebSocket touched (2-WS lock intact); **Never slow/locked/hanged** — zero
hot-path involvement (no DHAT/Criterion delta; scheduler is a once-a-minute
tokio task); **QuestDB never fails** — ensure-DDL self-heal pattern +
ILP-over-HTTP ACK; **O(1) latency** — hot path untouched; the per-minute fetch
is honestly O(underlyings)=O(3), flagged, cold; **Uniqueness + dedup** —
composite keys include `exchange_segment` + `feed` per I-P1-11; **Real-time
proof** — counters + coded errors + queryable tables.

Honest 100% claim (mandatory wording): 100% inside the tested envelope, with
ratcheted regression coverage: session-gated per-minute pulls for exactly 3
IDX_I SIDs; DEDUP-idempotent re-fetch; DH-904 backoff 10→20→40→80s then
skip-minute; chain leg DEFAULT-OFF pending the first-live-boot entitlement
probe; the live WS capture chain (ring → NDJSON spill → DLQ,
`TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`) is
untouched. Beyond the envelope, a failed minute is a counted, coded, visible
gap in a capture table — never silent, never a hole in the zero-loss tick
chain. NOT claimed: that Dhan publishes every minute within 1s (the retry +
counter measure it); that the option-chain entitlement exists (the probe
answers it); Dhan-side data correctness (the 15:31 cross-verify chain owns
parity).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Healthy minute in-session | 3 spot rows persisted ≤ ~2s after minute close; chain rows if enabled |
| 2 | Dhan minute not yet published at +1s | one retry next cycle; DEDUP absorbs; counted |
| 3 | DH-904 rate limit | 10→20→40→80s ladder, then skip the minute (CHAIN-02 / SPOT1M-01) |
| 4 | QuestDB down | SPOT1M-02 loud error + counter; live tick chain unaffected |
| 5 | No option-chain entitlement | probe fails at first enabled boot → Telegram verdict, leg dormant (CHAIN-04 class) |
| 6 | Weekend / NSE holiday | zero REST calls (calendar gate) |
| 7 | Mid-day restart | stateless resume at next minute; overlapping writes DEDUP-idempotent |
| 8 | Expiry day | warmup pins current expiry at day start; never rolls intraday |
