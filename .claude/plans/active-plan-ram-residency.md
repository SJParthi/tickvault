# Implementation Plan: RAM Residency Store (spot month-deep rings + chain day ring)

**Status:** APPROVED
**Date:** 2026-07-16
**Approved by:** Parthiban (operator directive 2026-07-16, verbatim quotes below)

> **Operator authority (2026-07-16, verbatim):**
> 1. *"how can i believe you that you have all these already available in our in-memory
>    app RAM — especially for the current day and even in the future last one month data
>    should be entirely in memory app RAM, especially for trading decisions of entry and
>    exit"*
> 2. *"for only spots we will have minimum one month data because anyhow based on
>    underlying spots alone only trading decision will be entered or exited — but option
>    only for the current day"*
> 3. *"everything should be always available in our own questdb right — our entire one
>    month should be stored and fetched from questdb even before premarket"*

Crates touched: **crates/trading** (new `crates/trading/src/in_mem/spot_bar_store.rs`),
**crates/core** (new `crates/core/src/pipeline/chain_day_store.rs`), **crates/common**
(`crates/common/src/config.rs` — new `MarketRamStoreConfig`;
`crates/common/src/error_code.rs` — new `RamStore01Degraded` / `RAMSTORE-01`),
**crates/app** (new `crates/app/src/market_ram_store_boot.rs`; hook edits to
`crates/app/src/rest_candle_fold.rs`, `crates/app/src/option_chain_1m_boot.rs`,
`crates/app/src/main.rs`; new test `crates/app/tests/ram_store_wiring_guard.rs`),
plus `config/base.toml`, `.claude/rules/project/ram-store-error-codes.md`, and
`.claude/triage/error-rules.yaml`.

## Design

PR-1 (`rest_candle_fold.rs`, #1609) made the 21 `candles_*` tables populate again from
the official `spot_1m_rest` REST bars and rehydrates the stored month at boot. This PR
(PR-2, stacked) answers the operator's "how can I BELIEVE it is in RAM" demand with two
process-RAM residency stores, both populated by the EXISTING data flows — no new hot
path, no new market-data fetch:

1. **Spot month-deep RAM store** — `SpotBarStore`
   (`crates/trading/src/in_mem/spot_bar_store.rs`; trading is the dependency-clean home:
   it owns `TfIndex`/`TF_COUNT` and sits below app in the flow common←core←trading←app).
   Per `(feed, security_id, exchange_segment)` slot × per `TfIndex` ring of SEALED bars
   (`RamBar`: bucket-open IST secs + O/H/L/C + volume = 48 B, `Copy`), ring capacity =
   `spot_days` (config, default 35) × session bars/day for that TF
   (`bars_per_day(tf) = ceil(22_500 / tf_secs)`, Σ over 21 TFs = 1,279 bars/day/slot →
   ~17.2 MB at 8 slots × 35 days — asserted by a test against a 40 MB ceiling).
   Rings are `VecDeque<RamBar>` pre-allocated at slot creation.
   - **WRITE path:** PR-1's fold task calls the store at its emit choke points —
     `emit_seals` (live seals + current-day refold re-emits → `upsert` by bucket ts:
     binary-search replace, push_back with front eviction, or bounded middle insert —
     refolds re-emit existing buckets, so upsert-by-ts is the idempotent form) and
     `refold_day` (boot catch-up + past-day repair refolds → `record_day_block`: the
     catch-up iterates days NEWEST→OLDEST, so an all-older day block PREPENDS in one
     O(day) pass — never an O(n²) front-insert storm; a repair refold of a day already
     resident upserts in place). Pre-market rehydration for spots = PR-1's existing
     boot catch-up, extended to also fill RAM — ZERO new QuestDB reads for spots.
   - **READ path:** per-slot `parking_lot::RwLock` (workspace dep; ~30 ns uncontended)
     over the slot's 21 rings + one store-level `RwLock` over the ≤8-entry slot vec.
     HONEST envelope: reads are guarded-lock + binary-search — O(#slots ≤ 8) scan +
     O(log ring) — NOT lock-free and NOT claimed O(1); reads are COLD today (no
     strategy consumer exists — §28 boundary; this is the read contract only, the
     chain_snapshot precedent).
   - **Accessors:** `latest_n`, `bar_at`, `depth_days`, `stats` (store-wide snapshot
     for the observability task).
2. **Chain day-deep RAM ring** — `ChainDayStore`
   (`crates/core/src/pipeline/chain_day_store.rs`; core owns the
   `ChainMoneynessSnapshot` row type it stores). The existing latest-minute
   `chain_snapshot` registry is UNTOUCHED (its consumers keep their contract). The day
   store maps, per `(feed, ChainUnderlying)` slot (6 slots), minute-open IST nanos →
   `Arc<ChainMoneynessSnapshot>` (reusing the existing published row type — strike/leg/
   ltp/moneyness; oi/volume/greeks stay in the `option_chain_1m` audit table, honestly
   documented as NOT RAM-resident), CURRENT IST day only, cleared on day roll
   (epoch-day derived from the minute ts; a stale older-day publish is dropped +
   counted, never a clear). Bounded: ≤405 minutes/slot map cap + a hard
   `chain_row_cap` rows/minute (config, default 1_000; over-cap snapshots are
   truncated LOUDLY — counted + coded warn). Worst-case estimate ≈ 55 MB (375 × 6 ×
   1_000 × 24 B + overhead) — asserted by a test against a 120 MB ceiling.
   - **Publish site:** inside the SHARED `publish_chain_moneyness_snapshot` helper
     (`option_chain_1m_boot.rs`), immediately after the existing `publish_chain_snapshot`
     — the single choke point BOTH chain legs (Dhan `option_chain_1m_boot` + Groww
     `groww_option_chain_1m_boot`) already call (adjustment vs the task sketch,
     documented: one hook site covering both legs beats two duplicated hooks; the
     wiring guard pins both legs' calls INTO the helper plus the helper's hook).
   - **Boot rehydration:** today's `option_chain_1m` rows via bounded hardened `/exec`
     reads (PR-1's read discipline: micros WHERE window, explicit LIMIT tripwire,
     streamed 8 MiB cap, redirect-none client, timeouts) — CHUNKED per
     (feed, underlying, 30-min session window) so no single response can approach the
     cap (≤ ~78 bounded queries worst case, cold boot path, honestly flagged
     O(windows × underlyings)); rehydrated minutes insert ONLY-IF-ABSENT so a live
     publish always wins.
3. **Observability:** gauges `tv_ram_store_spot_bars_resident{feed}`,
   `tv_ram_store_spot_days_depth{feed}` (minimum depth across the feed's slots — the
   guaranteed depth, never the best case), `tv_ram_store_chain_minutes_resident{feed}`,
   `tv_ram_store_estimated_bytes`; dense heartbeat `tv_ram_store_heartbeat_total` (one
   increment per 60 s stats tick — flatline = dead task); counters
   `tv_ram_store_dropped_total{reason}` + `tv_ram_store_errors_total{stage}` +
   `tv_ram_store_rehydrate_minutes_total{feed}`. New ErrorCode `RamStore01Degraded`
   (`RAMSTORE-01`, Severity::High, auto-triage-safe, LOG-SINK-ONLY — delivery boundary
   documented in the rule file) with stage taxonomy
   `install` / `rehydrate_query` / `rehydrate_parse` / `rehydrate_truncated` /
   `chain_truncated` / `day_drop` / `task_respawn`. Runbook:
   `.claude/rules/project/ram-store-error-codes.md` (+ triage escalate rule — both
   crossref directions stay green).
4. **Config:** `[market_ram_store]` → `MarketRamStoreConfig { enabled (serde default
   FALSE — fail-safe), spot_days (default 35, validated 1..=370), chain_row_cap
   (default 1_000, validated 1..=10_000) }`; `config/base.toml` opts in with
   `enabled = true`, `spot_days = 35`, `chain_row_cap = 1000`. A `spot_days` below
   `rest_candle_fold.catchup_days` is legal (the ring simply holds less than catch-up
   offers) — noted with a boot log line, never a hard error. Disabled = honest boot
   log line + zero installs + every hook a no-op (the fold/publish paths check the
   `OnceLock` globals).
5. **Wiring order (main.rs):** the stores install BEFORE the fold task spawns (so the
   boot catch-up populates RAM) — pinned by the wiring guard; the chain rehydrate +
   stats tasks spawn in the same gated block.

## Edge Cases

- Refold re-emits (current-day repair) re-deliver existing bucket ts values → ring
  upsert-by-ts replaces in place (never a duplicate bar, never unordered growth).
- Boot catch-up day order is NEWEST→OLDEST → per-day block PREPEND keeps rings sorted
  without O(n²) inserts; a block older than the ring's window is dropped at the
  capacity edge (correct eviction — oldest beyond `spot_days` never displaces newer).
- A middle-insert (a bucket never emitted before, older than the ring tail) is bounded
  by the ring length and structurally rare — handled, counted via stats, never a panic.
- Ring at capacity: push_back evicts front (oldest); push_front of an over-window bar
  drops the incoming bar.
- Chain day roll: first snapshot of a NEWER day clears the slot map; an OLDER-day
  (stale) publish is dropped + counted (`day_drop`) — never clears the live day.
- Chain boot sentinel (`minute_ts == 0`) is never recorded.
- Over-cap chain snapshot (> `chain_row_cap` rows) is truncated loudly (kept prefix,
  counted, coded warn once per (feed, underlying, day)).
- Rehydrate LIMIT hit → that window is skipped LOUDLY (`rehydrate_truncated`), never a
  partially-trusted window; rehydrated minutes never overwrite live-published minutes.
- Disabled config → no global installs; all hooks no-op; byte-identical behavior.
- Mid-session restart: spots re-fill from PR-1's catch-up; chain re-fills from the
  bounded rehydrate — the morning is back in RAM within the boot cold path.

## Failure Modes

- QuestDB unreachable during chain rehydrate → per-window coded degrade
  (`rehydrate_query`), remaining windows still attempted; the day store simply starts
  shallow and live publishes fill forward. Never blocks boot; never touches the fold.
- Stats/rehydrate task death → supervised respawn (house `classify_join_exit` pattern,
  `task_respawn` stage; release builds abort per `panic = "abort"` — honest envelope,
  the TICK-FLUSH-01 precedent).
- Store lock poisoning: parking_lot locks do not poison (trading); the core store uses
  std RwLock with `PoisonError::into_inner` (house pattern) — never a panic path.
- RAM depth is bounded by CAPTURED history: `spot_1m_rest` holds ~1-2 days today, so
  the rings reach month-deep organically (~mid-Aug) — the depth gauges make the honest
  fill level visible; no fabricated depth, ever (Rule 11).
- Seal-channel drops do NOT desync RAM: the RAM hook records the fold OUTPUT
  independently of the seal channel outcome; QuestDB remains the durable truth and the
  15:40 tf-consistency verifier remains the DB-side proof.

## Test Plan

- trading `spot_bar_store.rs` unit tests: ring append/upsert-by-ts/wraparound
  (eviction), prepend-day ordering (newest→oldest catch-up shape), capacity/memory
  estimate assertion (< 40 MB spots at 8 slots × 35 days), `latest_n` / `bar_at` /
  `depth_days` / `stats`, install/get global pins.
- app rehydration-equals-live-fold equality test: a ring populated catch-up-style
  (today block + older-day prepends) is IDENTICAL to a ring built from live per-seal
  upserts over the same bars.
- core `chain_day_store.rs` unit tests: record/read roundtrip, day-roll clears, stale
  older-day drop, row-cap loud truncation, rehydrate-never-overwrites-live, memory
  estimate assertion (< 120 MB worst case), install/get global pins.
- common config pins: default-off, spot_days/chain_row_cap bounds validate.
- `crates/app/tests/ram_store_wiring_guard.rs` (PR-1 guard style — call-site anchoring
  + production-region split): fold emit sites call the RAM hooks; the shared chain
  publish helper records to the day store after `publish_chain_snapshot`; BOTH chain
  legs call the shared helper; main.rs installs the stores BEFORE the fold spawn and
  spawns rehydrate + stats in the gated block.
- ErrorCode crossref (both directions) + triage full-coverage + prefix-pattern tests
  stay green (RAMSTORE- prefix added).

## Rollback

Config rollback: `[market_ram_store] enabled = false` (or delete the section — serde
default OFF) restores byte-identical behavior: no globals installed, every hook a
checked no-op, zero new tasks. Code rollback: revert the PR — the fold module hooks are
additive single-call sites; no schema, no table, no retention change (this PR touches
NO QuestDB DDL). No live-feed-purity implications: the stores READ fold output and
chain publishes; they never write any table.

## Observability

- Gauges: `tv_ram_store_spot_bars_resident{feed}`, `tv_ram_store_spot_days_depth{feed}`,
  `tv_ram_store_chain_minutes_resident{feed}`, `tv_ram_store_estimated_bytes` —
  refreshed by the 60 s stats task; the operator's "is the month actually in RAM?"
  question becomes a gauge read.
- Dense heartbeat: `tv_ram_store_heartbeat_total` (per stats tick — alarm-ready,
  metrics-local today per the delivery boundary).
- Counters: `tv_ram_store_dropped_total{reason}` (stale_day / over_window /
  chain_row_cap / sentinel), `tv_ram_store_errors_total{stage}`,
  `tv_ram_store_rehydrate_minutes_total{feed}`, `tv_ram_store_task_respawn_total{reason}`.
- Every degrade is `error!`/`warn!` with `code = ErrorCode::RamStore01Degraded.code_str()`
  (tag-guard discipline). LOG-SINK-ONLY delivery (no CloudWatch entry) — documented in
  the rule file with the paging drift guard untouched.

## Plan Items

- [x] Item 0 — This plan file + PR-1 plan archived (post-merge archival, rule 7)
  - Files: .claude/plans/active-plan-ram-residency.md, .claude/plans/archive/2026-07-16-rest-candle-derivation.md
  - Tests: (docs only)
- [x] Item 1 — Config `MarketRamStoreConfig` (serde default OFF) + validate + base.toml opt-in
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_market_ram_store_config_default_off, test_market_ram_store_config_validate_bounds
- [x] Item 2 — ErrorCode RAMSTORE-01 + rule file + triage rule + crossref prefix
  - Files: crates/common/src/error_code.rs, .claude/rules/project/ram-store-error-codes.md, .claude/triage/error-rules.yaml
  - Tests: error_code_rule_file_crossref, triage_rules_full_coverage_guard, test_code_str_follows_expected_prefix_pattern
- [x] Item 3 — `SpotBarStore` (rings + upsert + day-block + reads + stats + global)
  - Files: crates/trading/src/in_mem/spot_bar_store.rs, crates/trading/src/in_mem/mod.rs
  - Tests: test_spot_bar_store_append_sealed_and_bar_at_roundtrip, test_spot_bar_store_upsert_by_ts_replaces_in_place, test_spot_bar_store_wraparound_evicts_oldest, test_spot_bar_store_record_day_block_prepends_newest_to_oldest, test_spot_bar_store_latest_n_returns_newest_first, test_spot_bar_store_depth_days_counts_distinct_days, test_spot_bar_store_stats_and_estimated_bytes, test_estimated_capacity_bytes_under_40mb_envelope, test_bars_per_day_session_math, test_install_spot_bar_store_first_wins
- [x] Item 4 — `ChainDayStore` (minute map + day roll + caps + rehydrate insert + stats + global)
  - Files: crates/core/src/pipeline/chain_day_store.rs, crates/core/src/pipeline/mod.rs
  - Tests: test_chain_day_store_record_live_and_minute_snapshot_roundtrip, test_chain_day_store_day_roll_clears_previous_day, test_chain_day_store_stale_older_day_publish_dropped, test_chain_day_store_row_cap_truncates_loudly, test_chain_day_store_record_rehydrated_never_overwrites_live, test_chain_day_store_latest_minutes_and_minutes_resident, test_chain_day_store_stats_estimated_bytes_under_120mb, test_install_chain_day_store_first_wins, test_chain_day_store_sentinel_never_recorded
- [x] Item 5 — Fold emit hooks (emit_seals upsert + refold_day day-block) + chain publish hook
  - Files: crates/app/src/rest_candle_fold.rs, crates/app/src/option_chain_1m_boot.rs
  - Tests: test_ram_hook_catchup_equals_live_fold_ring, ram_store_wiring_guard (hook pins)
- [x] Item 6 — Boot module: install + chain rehydrate (bounded windows) + stats/heartbeat task + main.rs gated wiring
  - Files: crates/app/src/market_ram_store_boot.rs, crates/app/src/lib.rs, crates/app/src/main.rs
  - Tests: test_chain_rehydrate_sql_shape, test_parse_chain_rehydrate_rows_and_truncation_tripwire, test_build_minute_snapshots_groups_rows_per_minute, test_rehydrate_window_starts_cover_session, ram_store_wiring_guard (install-before-fold + spawn pins)
- [x] Item 7 — Wiring ratchet test
  - Files: crates/app/tests/ram_store_wiring_guard.rs
  - Tests: the guard itself (install-order, hook, both-legs, rehydrate pins)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot with 20 days of spot_1m_rest history, store enabled | catch-up fills the rings newest→oldest; depth gauges read 20 days |
| 2 | Live minute seals at 09:16:01 | M1 RamBar upserted in the same emit pass; QuestDB seal unchanged |
| 3 | Current-day repair refold re-emits buckets | ring upsert replaces in place — no duplicates, order intact |
| 4 | Mid-session restart at 13:00 | spots re-fill via catch-up; chain rehydrate returns the morning's minutes; live publishes win over rehydrated duplicates |
| 5 | Chain leg publishes a 1,200-row snapshot with cap 1,000 | truncated to 1,000 LOUDLY (counter + coded warn); minute still resident |
| 6 | IST day rolls (next session's first chain publish) | previous day's chain map cleared; spot rings retain the month |
| 7 | Store disabled | zero installs, hooks no-op, byte-identical to PR-1 behavior |
| 8 | QuestDB down at boot | chain rehydrate degrades loudly per window; live fills forward; fold unaffected |

## Per-Item Guarantee Matrix (15-row + 7-row, per `.claude/rules/project/per-wave-guarantee-matrix.md`)

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | store cores + config + rehydrate parse fully unit-tested; `quality/crate-coverage-thresholds.toml` ratcheted floors | post-merge llvm-cov | tests listed per item above |
| 100% audit coverage | RAM mirrors DEDUP-keyed `candles_*` / `option_chain_1m` rows; every degrade coded RAMSTORE-01 in errors.jsonl | `mcp__tickvault-logs__questdb_sql` | QuestDB stays the durable truth |
| 100% testing coverage | 22-category standard scoped to trading/core/common/app; boundary + equality + wraparound tests | `cargo test --workspace` green | Test Plan section |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + pre-commit gates | pre-push mandatory | all gates run before push |
| 100% code performance | writes are cold-path (per-minute fold emits / per-minute chain publish); reads cold (no consumer, §28); no hot-path involvement — honestly NOT lock-free, documented | n/a (not hot path) | N/A — cold path (no DHAT/Criterion per spec) |
| 100% monitoring | 4 gauges + heartbeat + drop/error counters + coded logs | `mcp__tickvault-logs__run_doctor` | Observability section |
| 100% logging | every degrade path `error!`/`warn!` with `code = RAMSTORE-01` (tag-guard) | errors.jsonl hourly | tag-guard green |
| 100% alerting | log-sink-only by design (honest delivery boundary — no false paging claim); the depth gauges are the operator's positive signal | paging drift guard green | documented in rule file |
| 100% security | local QuestDB /exec reads only, bounded + redirect-none; no secrets, no URLs with tokens; underlying symbols from the pinned ChainUnderlying set only | `cargo audit` | rehydrate SQL scoping tests |
| 100% security hardening | explicit LIMIT tripwires + 8 MiB streamed cap + timeouts + windowed reads | post-deploy | constants pinned in module |
| 100% bugs fixing | adversarial review by the parent session before the PR opens (task contract) | pre-PR | parent-session review |
| 100% scenarios covering | Scenarios table (8) + Edge Cases each mapped to a test or documented residual | scoped tests | Test Plan |
| 100% functionalities covering | every new pub fn has a call site + named test (pub-fn guards) | pre-push gates 6+11 | guards green |
| 100% code review | parent session hostile review on the diff | per-PR | task contract |
| 100% extreme check | ram_store_wiring_guard + memory-envelope assertions + config pins fail the build on regression | every commit | Item 7 |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | NO tick path touched; RAM mirrors fold output AFTER persist-confirmed bars; a lost RAM entry re-fills from QuestDB at next boot — never data loss | hooks sit on the fold emit path only |
| WS never disconnects | no WebSocket involvement (REST-only runtime) | n/a |
| Never slow/locked/hanged | cold-path writes (≤ dozens/min); bounded rehydrate windows with LIMIT + caps + timeouts; per-slot short critical sections | constants + wiring guard |
| QuestDB never fails | RAM store is independent of QuestDB health at runtime; rehydrate degrades loudly per window and live fills forward | Failure Modes section |
| O(1) latency | upsert is O(log ring) + amortized O(1) push; day-block prepend O(day); reads O(#slots ≤ 8) + O(log ring) — honestly flagged NOT lock-free/N-bounded, cold | store structure + docs |
| Uniqueness + dedup | ring upsert keyed by bucket ts per (feed, sid, segment, tf) — mirrors the candles DEDUP key; chain minutes keyed (feed, underlying, minute) | upsert/equality tests |
| Real-time proof | depth/resident gauges refreshed every 60 s + dense heartbeat; the 15:40 tf-consistency verifier remains the DB-side exact-match proof | Observability section |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the RAM stores
mirror exactly what the fold emits and the chain legs publish (equality-tested
catch-up-vs-live); bounded memory (asserted < 40 MB spots + < 120 MB chain worst case);
every degrade coded RAMSTORE-01. NOT claimed: month-deep RAM before the underlying
`spot_1m_rest` history exists (~1-2 days today, month-deep ~mid-Aug organically — the
depth gauges show the honest fill level); lock-free reads (guarded RwLock, documented);
any strategy consumer (§28 boundary — read contract only); chain oi/volume/greeks in
RAM (they live in the `option_chain_1m` audit table). Beyond the envelope, QuestDB
remains the durable truth and a restart rehydrates from it.
