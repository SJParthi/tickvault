# Implementation Plan: feed in DEDUP key on every persisted QuestDB table

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** Parthiban (operator override 2026-06-28 — verbatim below)

> **Operator override (2026-06-28):** "feed in DEDUP key on every persisted
> QuestDB table." This SUPERSEDES the 2026-06-23 verified decision that the
> instrument-master / universe / single-cross-feed-reconciliation tables keep
> `feed` as a LABEL only (NOT a key). The operator explicitly accepts the
> per-feed-duplicate-universe consequence: each feed now gets its own row for
> the same logical key on these previously-shared tables. This is
> **forward-compatible plumbing** — ALL writers stamp `'dhan'` today (no Groww
> master-writer exists yet; that populate work is a SEPARATE follow-up, NOT
> this PR).

## Design

Add the `feed` SYMBOL column INTO the DEDUP UPSERT key of the 5 remaining
persisted QuestDB tables that today treat `feed` as a label-or-absent:

1. `instrument_lifecycle`
2. `instrument_lifecycle_audit`
3. `instrument_fetch_audit`
4. `index_constituency`
5. `tick_conservation_audit`

Each follows the PROVEN pattern already shipped for `candles_*`
(`shadow_persistence.rs:242-262`) and `prev_day_ohlcv`
(`prev_day_ohlcv_persistence.rs:66,200-264`):

- DEDUP_KEY constant gains `feed` (appended).
- CREATE-DDL gains `feed SYMBOL`.
- Self-heal sequence runs at boot, IN THIS ORDER (the load-bearing order):
  `ALTER TABLE ADD COLUMN IF NOT EXISTS feed SYMBOL`
  → `UPDATE … SET feed = 'dhan' WHERE feed IS NULL` (brownfield NULL backfill)
  → `ALTER TABLE … DEDUP ENABLE UPSERT KEYS(<new key incl feed>)`.
- Row struct gains a `feed: &'a str` field (owned mirrors gain `feed: String`).
- The builder/tuple/INSERT-column-list stamps `feed`.
- Producers stamp `tickvault_common::feed::Feed::Dhan.as_str()` ("dhan").

`feed` is a `&'static str` from the existing `Feed` enum — O(1), zero-alloc,
replay-stable (constant per writer), so the capture_seq/idempotency guarantees
are preserved.

NOTE: `cross_verify_1m_audit` is NOT changed (no live storage module exists;
its successor `feed_parity_1m_audit` is already feed-keyed). `ws_event_audit`
+ `feed_parity_1m_audit` already carry `feed` in-key.

## Edge Cases

- **Brownfield NULL-feed rows** (rows written under the OLD key before this
  change): without the `UPDATE … WHERE feed IS NULL` backfill, a new
  `feed='dhan'` row for the same business key is a DISTINCT key (NULL != 'dhan')
  → a DUPLICATE, not an UPSERT. The backfill stamps `'dhan'` on every legacy
  NULL row BEFORE re-enabling DEDUP, closing the overlap window. Idempotent
  (`WHERE feed IS NULL` matches nothing once backfilled).
- **`index_constituency` + `tick_conservation_audit`** already have the
  `ALTER ADD COLUMN feed` (as a NON-key label); this PR adds the backfill +
  DEDUP-ENABLE after it, and moves `feed` INTO the key.
- **`instrument_fetch_audit`** has NO ILP path + NO prior self-heal — the
  ALTER+backfill+DEDUP-ENABLE block is NEW for it.
- **Per-feed-duplicate universe** (operator-accepted): the lifecycle master,
  fetch audit, constituency map, and conservation audit will now keep one row
  per feed per logical key. Today only `'dhan'` is stamped, so no duplicates
  appear until a Groww writer lands.
- **Empty/optional SYMBOL skip** does NOT apply to `feed` — it is always
  stamped non-empty ('dhan'), and IS a DEDUP key, so it MUST be written every
  row (skipping it → NULL → cross-feed collapse).

## Failure Modes

- **DDL self-heal non-2xx / request fail** — logged (warn!/error! per the
  existing convention in each module); never blocks boot. Mirrors the existing
  prev_day_ohlcv + index_constituency feed-ALTER error handling.
- **DEDUP ENABLE on a populated table** — QuestDB re-enabling the same key is a
  no-op; never drops the table (SEBI retention). No populated-table DROP path
  is introduced.
- **Producer regression** (a writer forgets to stamp `feed`) — caught at
  compile time (the struct field is non-optional) + by the per-writer
  feed-stamp unit tests.

## Test Plan

- Per-writer feed-stamp unit test for each of the 5 tables (asserts the
  builder/tuple emits `feed`/'dhan').
- Updated exact-DEDUP-key tests (column counts: lifecycle 3→4, audit 5→6,
  fetch 4→5, constituency 4→5, conservation 2→3).
- Updated exact INSERT-column-count / tuple-field-count tests
  (lifecycle 24→25, audit 16→17, fetch 12→13, constituency-columns 9→10,
  conservation DDL +feed).
- NEW meta-guard `every_persisted_table_dedup_key_must_include_feed` in
  `dedup_segment_meta_guard.rs` — scans ALL DEDUP_KEY_* via
  `collect_dedup_key_declarations()`, asserts each contains `feed` as a whole
  token; allowlist EMPTY (`FEED_NOT_APPLICABLE_KEYS = &[]`).
- Regression test: two feeds' same `(security_id, segment)` produce DISTINCT
  rows (key-level — the DEDUP key strings for the two feeds differ).
- Scoped: `cargo test -p tickvault-storage -p tickvault-common -p tickvault-app
  --features daily_universe_fetcher` + `cargo test -p tickvault-storage --test
  dedup_segment_meta_guard` + `cargo fmt`.

## Rollback

`git revert` the single squash commit. The change is additive (a new column +
key migration); reverting removes the column from the CREATE DDL + key, and the
self-heal ALTER on an OLD binary simply never runs again. Already-written
`feed='dhan'` rows remain (harmless extra column). No data loss either
direction. No feature flag needed — the change is behind the existing
`daily_universe_fetcher` gate for 4 of the 5 tables (lifecycle ×2, fetch,
constituency); `tick_conservation_audit` is always-compiled.

## Observability

No new error codes / counters / Telegram events — this is a schema-key change.
The existing self-heal DDL error logging (warn!/error! with the existing
module-local messages) covers the migration failure path. The DEDUP-key
meta-guards (`per_feed_market_data_dedup_keys_must_include_feed` +
the NEW `every_persisted_table_dedup_key_must_include_feed`) are the ratchets
that fail the build if any persisted table's key drops `feed`.

## Per-Item Guarantee Matrix

Cross-references `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row +
7-row matrices) and `.claude/rules/project/zero-loss-guarantee-charter.md` §3.

### 15-row 100% Guarantee Matrix

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | new unit tests per writer + updated exact-key tests | post-merge llvm-cov | scoped `cargo test` green |
| 100% audit coverage | the audit tables themselves gain feed-in-key (forensic per-feed) | `questdb_sql` | DEDUP keys updated |
| 100% testing coverage | unit + exact-string + meta-guard + regression | `cargo test -p tickvault-storage` | declared above |
| 100% code checks | banned-pattern + pub-fn-test + plan-verify + fmt | pre-push | all gates green |
| 100% code performance | `feed` is `&'static str`, zero-alloc, O(1) — no hot path touched | N/A — cold-path persistence | no hot-path alloc added |
| 100% monitoring | existing self-heal DDL error logs | `tail_errors` | unchanged |
| 100% logging | existing warn!/error! on DDL failure | errors.jsonl | unchanged |
| 100% alerting | N/A — schema change, no new failure mode | — | N/A |
| 100% security | `feed` is a fixed `'dhan'` label (no injection surface) | — | N/A |
| 100% security hardening | sanitised SYMBOL write paths reused | — | unchanged |
| 100% bugs fixing | adversarial 3-agent review on diff | sequential agents | pre-merge |
| 100% scenarios covering | brownfield NULL backfill + cross-feed-distinct regression | meta-guard + regression test | declared |
| 100% functionalities covering | every new field stamped + tested | pre-push gates | per-writer test |
| 100% code review | hot-path → security → hostile (sequential) | per-PR | pre-merge |
| 100% extreme check | NEW meta-guard fails build if any key drops feed | `cargo test --test dedup_segment_meta_guard` | shipped |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | unchanged — `ticks` key already feed-keyed; this PR touches master/audit tables | no tick-drop path added |
| WS never disconnects | unchanged | no WS code touched |
| Never slow/locked/hanged | cold-path DDL + persistence only; no hot-path alloc | `feed` is `&'static str` |
| QuestDB never fails | self-heal ALTER+backfill+DEDUP-ENABLE preserved; no populated-table DROP | mirrors proven candles/prev_day pattern |
| O(1) latency | per-row feed stamp is an O(1) `&'static str` copy | no per-row allocation |
| Uniqueness + dedup | composite `(…, feed)` per the per-feed identity model (I-P1-11 EXTENDED) | DEDUP keys + meta-guard |
| Real-time proof | the 2 DEDUP meta-guards ratchet the contract | `cargo test` |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: every
persisted QuestDB table's DEDUP UPSERT key now includes `feed`, pinned by the
NEW `every_persisted_table_dedup_key_must_include_feed` meta-guard (empty
allowlist) + the existing `per_feed_market_data_dedup_keys_must_include_feed`
ratchet — removing `feed` from any persisted key fails the build. This is
PLUMBING ONLY: all writers stamp `'dhan'` today; no Groww master-writer exists,
so the per-feed-populate work is a SEPARATE follow-up. The brownfield NULL-feed
backfill closes the migration overlap window so a re-keyed table upserts in
place rather than duplicating legacy rows.

## Plan Items

- [x] Item 1 — `instrument_lifecycle`: feed in key + DDL + row + builder + tuple + INSERT-list + ALTER/backfill/DEDUP-ENABLE
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs, crates/app/src/apply_reconcile_plan.rs
  - Tests: test_feed_in_lifecycle_dedup_key, test_lifecycle_row_stamps_feed
- [x] Item 2 — `instrument_lifecycle_audit`: feed in key + DDL + row + builder + tuple + INSERT-list (shares ensure-fn file)
  - Files: crates/storage/src/instrument_lifecycle_persistence.rs, crates/app/src/apply_reconcile_plan.rs
  - Tests: test_feed_in_lifecycle_audit_dedup_key, test_lifecycle_audit_row_stamps_feed
- [x] Item 3 — `instrument_fetch_audit`: feed in key + DDL + row + tuple + INSERT-list + NEW ALTER/backfill/DEDUP-ENABLE
  - Files: crates/storage/src/instrument_fetch_audit_persistence.rs, crates/app/src/instr_fetch_audit_writer.rs
  - Tests: test_feed_in_fetch_audit_dedup_key, test_fetch_audit_row_stamps_feed
- [x] Item 4 — `index_constituency`: feed in key + DDL + columns-const + row + builder + backfill/DEDUP-ENABLE
  - Files: crates/storage/src/index_constituency_persistence.rs, crates/app/src/index_constituency_boot.rs
  - Tests: test_feed_in_index_constituency_dedup_key, test_index_constituency_row_stamps_feed
- [x] Item 5 — `tick_conservation_audit`: feed in key + DDL + row + append_row + backfill/DEDUP-ENABLE
  - Files: crates/storage/src/tick_conservation_audit_persistence.rs, crates/app/src/tick_conservation_boot.rs
  - Tests: test_feed_in_conservation_dedup_key, test_conservation_row_stamps_feed
- [x] Item 6 — NEW meta-guard + stale-comment update
  - Files: crates/storage/tests/dedup_segment_meta_guard.rs
  - Tests: every_persisted_table_dedup_key_must_include_feed
- [x] Item 7 — rule-file updates (OVERRIDE section + data-integrity clause + I-P1-11 note)
  - Files: .claude/plans/active-plan-feed-identity-all-tables.md, .claude/rules/project/data-integrity.md, .claude/rules/project/security-id-uniqueness.md

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Greenfield boot creates table | CREATE DDL already has `feed SYMBOL` + DEDUP key incl `feed` |
| 2 | Brownfield boot (old table, NULL feed rows) | ALTER adds col → backfill stamps 'dhan' → DEDUP ENABLE re-keys; legacy rows upsert in place |
| 3 | Dhan writer stamps a row | row.feed == "dhan"; tuple/ILP emits `feed='dhan'` |
| 4 | Future Groww writer (hypothetical) | same (sid,seg) but feed='groww' → DISTINCT key → both rows kept |
| 5 | Meta-guard run | all 10 DEDUP_KEY_* in storage src contain `feed`; empty allowlist passes |
