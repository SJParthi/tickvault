# Implementation Plan: PR-A — Persist Groww instruments into the shared lifecycle + constituency tables (feed='groww')

**Status:** VERIFIED
**Date:** 2026-06-28
**Approved by:** Parthiban (operator task brief 2026-06-28)
**Branch:** `claude/pr-a-groww-shared-master` (off origin/main 5c6477f7 = PR #1229)
**Changed crates:** `core`, `storage` (reused), `common`, `app`

> Authority chain obeyed: `daily-universe-scope-expansion-2026-05-27.md` (the lifecycle
> master is the system-of-record, NEVER deleted) · `security-id-uniqueness.md` (I-P1-11
> composite `(security_id, exchange_segment)`, extended in-key with `feed`) ·
> `data-integrity.md` "feed-in-key EVERYWHERE" (operator override 2026-06-28) ·
> `groww-second-feed-scope-2026-06-19.md` (SAME shared tables tagged `feed='groww'`, no
> `groww_*` master table) · `live-feed-purity.md` (this is master/constituency metadata,
> NOT the `ticks` table — no synthesized ticks anywhere).

## Design

PR #1229 already put `feed` in the DEDUP key + DDL + row struct of the two shared master
tables and stamps `feed='dhan'` on the Dhan-side row builders. Verified live on this branch:

- `instrument_lifecycle` — DEDUP `ts, security_id, exchange_segment, feed`
  (`instrument_lifecycle_persistence.rs:129`); DDL `feed SYMBOL` (:412); row field
  `InstrumentLifecycleRow.feed: &'a str` (:365); Dhan builders stamp `LIFECYCLE_FEED_DHAN`
  (`apply_reconcile_plan.rs:363` audit, `:413` upsert).
- `index_constituency` — DEDUP `ts, index_name, security_id, exchange_segment, feed`
  (`index_constituency_persistence.rs:69`); DDL `feed SYMBOL` (:152); row field
  `IndexConstituencyRow.feed: &'a str` (:117); feature-gated `daily_universe_fetcher`
  (:37); Dhan builder stamps `INDEX_CONSTITUENCY_FEED_DHAN` (`index_constituency_boot.rs:180`).

PR-A adds the **Groww-side writer** that fills the `feed='groww'` rows, reusing the existing
bulk ILP append fns — no new table, no new DEDUP key, no new ILP code.

Dependency direction (verified): `core` depends on `storage` (`core/Cargo.toml:10`), storage
depends only on `common`. So the new writer can live in `crates/core/.../groww/` and call
`tickvault_storage::instrument_lifecycle_persistence::*` directly.

**WatchEntry hot-path verdict (verified):** `WatchEntry`/`GrowwWatchSet` are referenced ONLY
in `instruments.rs` (def + tests) and plan docs (`grep WatchEntry|GrowwWatchSet` → 2 files).
The live tick path is the off-process Python sidecar reading the JSON watch file; the Rust
per-tick path never touches `WatchEntry`. The module header already declares it "PURE CORE …
daily watch build". **Verdict: WatchEntry is COLD-PATH only → add `Option<String>` fields**
(`isin`, `symbol_name`, `index_name`) populated through the resolvers, so the lifecycle +
constituency rows have `symbol_name`/`isin`/`index_name`. Cold-path daily build → these
allocations are acceptable (documented at the field site). No per-tick allocation introduced.

**Index security_id derivation (verified):** `stable_index_security_id(groww_symbol)`
(`instruments.rs:290`) already exists — FNV-1a(64) of the globally-unique `groww_symbol`,
forced into `[2^62, 2^63)` so it can never collide with a numeric stock token. `extract_index_entries`
already stamps `WatchEntry.security_id` with it. **Reuse the value already on `WatchEntry.security_id`** —
no new derivation. Stocks: `WatchEntry.security_id` is the numeric Groww `exchange_token`.

**Segment label (zero-alloc &'static str):** `groww_segment_label(&WatchEntry) -> &'static str`:
`IndexValue → "IDX_I"`; `Ltp` on exchange `"NSE"` → `"NSE_EQ"`, `"BSE"` → `"BSE_EQ"`;
FNO (segment `"FNO"`) on NSE → `"NSE_FNO"`, on BSE → `"BSE_FNO"`. `WatchEntry.exchange`/`.segment`
are `String` ("NSE"/"BSE", "CASH"/"FNO") — matched exhaustively with a documented fallthrough
(today the watch set is only IndexValue + Ltp-CASH; FNO arms are forward-cover for a future
F&O watch set; an unknown exchange falls back to NSE_EQ with a `warn!` once, never panics).

**Feature-gating of constituency write:** `index_constituency_persistence` is gated under
`daily_universe_fetcher`; `instrument_lifecycle_persistence` is always-on. So in the writer:
the lifecycle build/write is always compiled; the constituency build/write is
`#[cfg(feature = "daily_universe_fetcher")]`, mirroring `index_constituency_boot.rs`. Core's
`daily_universe_fetcher` feature is made to **forward** to `tickvault-storage/daily_universe_fetcher`
(core/Cargo.toml) so the gated storage module is compiled when core's feature is on.

**Dry-run handling:** the Groww lane has no dry-run-universe flag (verified: no `dry_run` in
`activate_groww_lane` scope). `persist_groww_instruments` accepts `dry_run: bool` for API
symmetry + rule §27 isolation contract; the call site passes `false`. When `dry_run=true`,
rows are built and the would-write counts logged, but NO append runs (isolation; rows never
leak into the live universe). Rows always carry `dry_run` so a future dry-run path reads only
`WHERE dry_run=false`.

**Persist call site (verified):** the `Ok(set)` arm of `build_and_write_groww_watch` is
`groww_activation.rs:331` (NOT :272 — that is the auth-smoke arm). `questdb: QuestDbConfig`,
`watch_date: String`, `set: GrowwWatchSet` are all in scope. The call is fire-and-forget
(`tokio::spawn`) so it never blocks lane activation; failure logs `error!(code=GROWW-MASTER-01)`
+ increments `tv_groww_master_persist_errors_total` and returns (degrade-safe, never aborts).

## Edge Cases

- Empty watch set → builders return empty Vec; bulk append fns already early-return `Ok(())`
  on empty slices (`instrument_lifecycle_persistence.rs:715`, `index_constituency_persistence.rs:274`).
- Stock with no ISIN/symbol retained → `Option` is `None` → row uses the empty-string sentinel
  (`""`), which the ILP writer skips (→ NULL) for optional SYMBOLs; `feed` is never empty.
- Index entry → `instrument_type = INDEX`, `exchange_segment = IDX_I`, spot sentinels
  (lot_size=0, tick_size=0.0, expiry=0, strike=0.0, option_type="", underlying=0).
- Stock entry → `instrument_type = EQUITY`, `exchange_segment = NSE_EQ`, same spot sentinels.
- BSE SENSEX index → exchange "BSE", kind IndexValue → label still `IDX_I` (index segment),
  matching the Dhan convention that all index values live in IDX_I.
- `daily_universe_fetcher` OFF → constituency build/write is compiled out; lifecycle still runs.
- Same instrument id present for Dhan AND Groww → distinct `feed` → DEDUP keeps both rows.

## Failure Modes

- QuestDB ILP unreachable → `append_*_rows` returns `Err` → writer logs `error!(code=GROWW-MASTER-01)`,
  increments the error counter, returns. Groww activation + the live feed are UNAFFECTED
  (best-effort cold-path forensic write; Severity::Medium).
- Bad `watch_date` (non-`YYYY-MM-DD`) reaching the writer → the date→nanos helper returns 0;
  the call site already validates `watch_date` (`groww_activation.rs:227`) so this is
  defense-in-depth, never a panic.
- Builder receives a WatchEntry with an unrecognized exchange → segment label falls back to
  `NSE_EQ` + logs `warn!` (never panics; no silent mis-segment because it is logged).

## Test Plan

Unit (in `shared_master_writer.rs`):
- `test_groww_segment_label_*` — every (kind, exchange, segment) permutation → exact &'static str.
- `test_build_groww_lifecycle_rows_*` — feed=="groww" on every row; index→INDEX/IDX_I,
  stock→EQUITY/NSE_EQ; spot sentinels; symbol_name/isin populated when present, empty when None.
- `test_build_groww_constituency_rows_*` (feature-gated) — feed=="groww", index_name/symbol/isin/
  security_id/segment correct.
- `test_regression_dhan_and_groww_same_id_distinct_under_composite_key` — pure in-memory proof
  that `(security_id, exchange_segment, feed)` keeps both a Dhan and a Groww row (key tuple test).
- `test_no_row_has_empty_feed` — builder always stamps a non-empty feed.

Guard/ratchet (existing, must stay green):
- `dedup_segment_meta_guard::every_persisted_table_dedup_key_must_include_feed` (no new key added).
- `error_code_rule_file_crossref` + `error_code` self-tests (new variant + rule file).
- `error_code_tag_guard` (the `error!` site carries `code = …code_str()`).

## Rollback

- Pure-additive: a new core module + a new ErrorCode variant + a new rule file + one
  fire-and-forget call + `Option` fields on a cold-path struct. Revert the single commit to
  fully remove. The Dhan path is untouched (already stamps `feed='dhan'`), so reverting cannot
  break Dhan persistence. The Groww writer is degrade-safe, so even a latent bug only loses the
  best-effort Groww forensic rows, never a tick or an order.

## Observability

- `error!(code = ErrorCode::GrowwMaster01PersistFailed.code_str(), …)` on any persist failure →
  5-sink fan-out → Telegram (Severity::Medium).
- `tv_groww_master_persist_errors_total{stage}` counter on each failure stage (lifecycle / constituency).
- `info!` success line with `lifecycle_rows` / `constituency_rows` / `dry_run` counts (positive signal).
- New runbook `.claude/rules/project/groww-shared-master-error-codes.md` (GROWW-MASTER-01).

---

## Plan Items

- [x] Item 1 — NEW `shared_master_writer.rs`: segment label, lifecycle + constituency row
      builders, `persist_groww_instruments` (fire-and-forget, degrade-safe, dry-run).
  - Files: crates/core/src/feed/groww/shared_master_writer.rs, crates/core/src/feed/groww/mod.rs
  - Tests: test_groww_segment_label_idx_i, test_groww_segment_label_nse_eq,
    test_groww_segment_label_bse_eq, test_groww_segment_label_nse_fno,
    test_groww_segment_label_bse_fno, test_build_groww_lifecycle_rows_index_is_index_idx_i,
    test_build_groww_lifecycle_rows_stock_is_equity_nse_eq,
    test_build_groww_lifecycle_rows_spot_sentinels, test_build_groww_lifecycle_rows_feed_is_groww,
    test_no_row_has_empty_feed,
    test_regression_dhan_and_groww_same_id_distinct_under_composite_key

- [x] Item 2 — retain `isin`/`symbol_name`/`index_name` through the resolvers onto cold-path
      `WatchEntry`.
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_resolve_stock_entries_retains_isin_and_symbol,
    test_extract_index_entries_retains_index_name

- [x] Item 3 — feature forwarding so core's `daily_universe_fetcher` compiles storage's gated module.
  - Files: crates/core/Cargo.toml

- [x] Item 4 — wire `persist_groww_instruments` into the `Ok(set)` arm (fire-and-forget).
  - Files: crates/app/src/groww_activation.rs
  - Tests: test_build_groww_constituency_rows_feed_is_groww (feature-gated, in writer)

- [x] Item 5 — NEW ErrorCode `GrowwMaster01PersistFailed` (GROWW-MASTER-01) + rule file.
  - Files: crates/common/src/error_code.rs, .claude/rules/project/groww-shared-master-error-codes.md
  - Tests: error_code self-tests (test_all_variants_have_unique_code_str, …),
    error_code_rule_file_crossref::every_error_code_variant_appears_in_a_rule_file

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww watch set built, QuestDB healthy, daily_universe_fetcher ON | lifecycle + constituency rows written, all feed='groww' |
| 2 | Same scenario, daily_universe_fetcher OFF | only lifecycle rows written; constituency build compiled out |
| 3 | QuestDB ILP down | error!(GROWW-MASTER-01) + counter; activation + live feed unaffected |
| 4 | dry_run=true | rows built, counts logged, NO append |
| 5 | Dhan + Groww both have id=27 NSE_EQ | both rows kept (feed differs in DEDUP key) |
| 6 | Stock with no retained ISIN | row written with isin sentinel "" (→ NULL), feed='groww' non-empty |

## Per-Item Guarantee Matrix (15-row + 7-row, per per-wave-guarantee-matrix.md)

### 15-row "100% everything"

| Demand | Proof for PR-A |
|---|---|
| 100% code coverage | unit tests for every new pure fn (label + 2 builders); builders are pure → fully covered |
| 100% audit coverage | reuses the existing `instrument_lifecycle` + `index_constituency` audit/master tables (DEDUP incl. feed) |
| 100% testing coverage | smoke + happy + error (None isin) + edge (empty set) + dedup (#20) + serialization (row→ILP via existing builders) |
| 100% code checks | banned-pattern (zero-alloc &'static str labels), pub-fn-test, pub-fn-wiring (call site in groww_activation), plan-verify |
| 100% code performance | cold-path daily build (NOT hot path) — no per-tick alloc; segment label is &'static str (zero-alloc) |
| 100% monitoring | `tv_groww_master_persist_errors_total` counter + info success line |
| 100% logging | error!(code=GROWW-MASTER-01) on failure; info! on success; never logs secrets |
| 100% alerting | Severity::Medium → Telegram via the 5-sink error pipeline |
| 100% security | no secrets touched; feed labels are &'static str; ILP strings sanitized by reused builders |
| 100% security hardening | no new external input; date→nanos validated; no panic paths |
| 100% bug fixing | adversarial 3-agent review BEFORE + AFTER (hot-path, security, hostile) |
| 100% scenarios covering | 6 scenarios above + dedup regression test |
| 100% functionalities covering | every new pub fn has a test + a call site (persist_groww_instruments wired) |
| 100% code review | self-review + the 3-agent pass on the diff |
| 100% extreme check | ratchet: dedup_segment_meta_guard + error_code cross-ref + tag-guard fail the build on regression |

### 7-row "Resilience"

| Demand | Honest envelope for PR-A |
|---|---|
| Zero ticks lost | N/A — this is master/constituency metadata, NOT the ticks path; introduces no tick-drop path |
| WS never disconnects | N/A — no WS code touched |
| Never slow/locked/hanged | fire-and-forget cold-path spawn; never blocks activation or the hot path |
| QuestDB never fails | ABSORB: persist failure is degrade-safe (error! + counter + return), next boot re-runs (idempotent DEDUP UPSERT) |
| O(1) latency | segment label is O(1) &'static str; per-constituent build is O(1); whole build is cold-path O(n) over the daily set (flagged O(n), honestly — not a per-tick path) |
| Uniqueness + dedup | composite `(security_id, exchange_segment, feed)` — reuses the existing in-key DEDUP; Dhan + Groww rows coexist |
| Real-time proof | counter + error code + info success line; regression test proves both feeds' rows survive |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the Groww master/constituency
writer reuses the existing in-key (`feed`) DEDUP UPSERT tables (no new key, meta-guard unchanged);
it is fire-and-forget + degrade-safe so a QuestDB outage only loses best-effort Groww forensic rows
(never a tick, never an order, never Groww activation), and the next idempotent boot re-runs. This is
PLUMBING that POPULATES the second feed's master rows — it adds no hot-path allocation and touches no
WS/order/tick path.
