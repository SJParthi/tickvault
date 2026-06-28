> **⚠ OVERRIDE 2026-06-28 — `feed` IN-KEY ON EVERY PERSISTED TABLE (SUPERSEDES the 2026-06-23 "masters = label-only" decision below).**
>
> Operator override 2026-06-28: every persisted QuestDB table's DEDUP UPSERT key
> MUST include `feed`, accepting the per-feed-duplicate-universe consequence
> (each feed gets its OWN row for the same logical key on the previously-shared
> master/audit tables). This REVERSES the 2026-06-23 "Label-only, NOT in key"
> verified decision for the 5 master/audit tables.
>
> **Implemented (PR `claude/feed-everywhere-in-key`):** `feed` moved INTO the
> DEDUP key + CREATE DDL + writer + brownfield self-heal
> (ALTER ADD COLUMN → backfill NULL→'dhan' → DEDUP ENABLE) for:
> `instrument_lifecycle`, `instrument_lifecycle_audit`, `instrument_fetch_audit`,
> `index_constituency`, `tick_conservation_audit`. (`cross_verify_1m_audit` has no
> live storage module; `ws_event_audit` + `feed_parity_1m_audit` were already
> feed-keyed.)
>
> **Honest envelope:** this is FORWARD-COMPATIBLE PLUMBING ONLY — all writers
> stamp `'dhan'` today; no Groww master-writer exists, so no duplicate rows
> appear until that SEPARATE follow-up lands. The brownfield NULL→'dhan' backfill
> closes the migration overlap window. Ratchet: the NEW
> `every_persisted_table_dedup_key_must_include_feed` meta-guard (empty
> allowlist) in `dedup_segment_meta_guard.rs` fails the build if any persisted
> key drops `feed`.
>
> The 2026-06-23 contents below are RETAINED for audit only; their "Label-only"
> verdict for the master/audit tables is NO LONGER the effective contract.

---

# Implementation Plan: C — per-feed `feed` identity across all tables

**Status:** VERIFIED (operator AskUserQuestion 2026-06-23: master tables = "Label-only, NOT in key") — SUPERSEDED by the 2026-06-28 OVERRIDE above for the 5 master/audit tables.
**Date:** 2026-06-23
**Branch:** `claude/feed-identity-all-tables` (off origin/main).
**Operator choice:** AskUserQuestion 2026-06-23 → "All tables, in DEDUP keys (full)".

## Grounded reality (Explore survey, cite-heavy)

The CORE market-data tables are ALREADY fully per-feed partitioned:

| Table | `feed` col | `feed` in DEDUP | writer sets it | status |
|---|---|---|---|---|
| `ticks` | yes | yes (`security_id, segment, capture_seq, feed`) | Dhan+Groww | ✅ DONE |
| `candles_1m..1d` (21) | yes | yes (`ts, security_id, segment, feed`) | shared seal writer (`seal.feed`) | ✅ DONE |

Remaining tables have `feed` as an **additive ALTER column** (present in DDL) but
NOT in their DEDUP key and NOT stamped by the writer:

| Table | per-instrument market data? | shared across feeds? | recommended `feed` treatment |
|---|---|---|---|
| `prev_day_ohlcv` | YES (daily candle/SID) | NO — each feed has its own | **DEDUP key + writer stamps** |
| `tick_conservation_audit` | per-RUN (not per-instrument) | single COMBINED cross-feed reconciliation | label-only / NO key change (no per-feed value exists) |
| `ws_event_audit` | per-connection | connections are feed-specific | **DEDUP key + writer** (self-describing) |
| `cross_verify_1m_audit` | Dhan live-vs-Dhan-REST | Dhan-only by design | column-only (always `dhan`); Groww parity is its OWN table per lock §32 |
| `instrument_lifecycle` / `instrument_lifecycle_audit` | universe master | **YES — same instruments both feeds** | **NO feed in key** (would duplicate the universe) — column-only/N-A, feature-gated |
| `index_constituency` | universe map | YES — shared | **NO feed in key** — column-only/N-A, feature-gated |
| `instrument_fetch_audit` | per-fetch (Dhan CSV) | Dhan fetch only | N/A |

### The honest correctness flag (why literal "every DEDUP key" is wrong)

The instrument **universe is identical regardless of feed** — both Dhan and Groww
subscribe NIFTY=13, BANKNIFTY=25, etc. Putting `feed` into the DEDUP key of the
universe/master tables (`instrument_lifecycle`, `index_constituency`,
`instrument_fetch_audit`) would **duplicate the entire instrument master per feed**
— wrong data model, SEBI-master corruption, and it breaks the I-P1-11
`(security_id, exchange_segment)` single-universe contract. So "feed in the key"
must apply to **per-feed market-data / per-feed-event** tables only, NOT the shared
universe master. `cross_verify_1m_audit` stays Dhan-only (Groww has its own audit
table, operator lock §32) so `feed` there is column-only.

## Plan Items

Feed-in-key applies ONLY where each row is genuinely ONE feed's record. Two
tables qualify; the rest get label-only or stay as-is (honest refinement, same
principle the operator approved for the master tables).

- [x] **C1 — `prev_day_ohlcv`: feed into DEDUP key + writer** ✅ DONE
  - `prev_day_ohlcv_persistence.rs` (DEDUP `ts, security_id, segment, feed`; Row `feed`; builder stamps it; `DEDUP ENABLE` self-heal for existing tables), `crates/app/src/prev_day_ohlcv_boot.rs` (passes `Feed::Dhan`)
- [x] **C2 — `ws_event_audit`: feed into DEDUP key + writer** (WS connections ARE per-feed: Dhan-WS vs Groww-WS)
  - `crates/common/src/ws_event_types.rs` (Row `feed`), `crates/storage/src/ws_event_audit_persistence.rs` (DEDUP const + builder + `DEDUP ENABLE` self-heal), emit sites pass `Feed::Dhan`
- [x] **C3 — `cross_verify_1m_audit`: stamp `feed` LABEL only** (NOT key — Dhan-only audit; Groww has its own table per lock §32)
  - `crates/storage/src/cross_verify_1m_audit_persistence.rs` + its boot caller
- [x] **C4 — meta-guard + docs**: extend the dedup meta-guard so the per-feed *market-data* keys (ticks, candles, prev_day_ohlcv, ws_event) MUST contain `feed`; update `data-integrity.md` + relevant rule files.

**Label-only / NOT in key (per-feed value is meaningless or absent):**
- `tick_conservation_audit` — a SINGLE combined cross-feed reconciliation (global WAL-frame + processor counters); no per-feed value exists. Additive `feed` column stays; NO key change, NO writer change.
- `cross_verify_1m_audit` — Dhan-only audit (Groww has its own table). `feed` label-only.

**Explicitly OUT (shared universe master — feed in key would duplicate the universe):**
`instrument_lifecycle`, `instrument_lifecycle_audit`, `index_constituency`,
`instrument_fetch_audit` — operator-confirmed label-only / NO key change.

## Design
Mirror the proven `ticks`/`candles` pattern: a `Feed` enum `&'static str` (already
`tickvault_common::feed::Feed`) stamped by the writer, added to the DEDUP key so two
feeds' rows never collide. O(1), zero-alloc (`&'static str`), no hot-path change
(prev_day/audit are cold paths; ws_event is per-event).

## Edge Cases
- Old rows already on disk without `feed` → QuestDB SYMBOL default is null; the
  DEDUP-key flip `DEDUP ENABLE UPSERT KEYS(...)` re-applies in place (same in-place
  fail-safe used for the ticks key flip — never drops a populated table).
- Groww writers must stamp `Feed::Groww`; Dhan stamps `Feed::Dhan`.

## Failure Modes
- DEDUP-key meta-guard (`dedup_segment_meta_guard.rs`) + the exact-string
  `test_dedup_key_*` tests will trip on each key change — update them in lockstep.
- A populated table must NOT be dropped on the key flip (SEBI) — reuse the
  `*_table_is_populated`-gated in-place re-key path.

## Test Plan
- Per-crate: `cargo test -p tickvault-storage -p tickvault-common -p tickvault-core -p tickvault-app`
- Update + run the exact-DEDUP-string tests for each changed table.
- `cargo clippy --workspace -- -D warnings` + `cargo fmt --check`.
- 3-agent adversarial review (hot-path + security + hostile) on the diff (charter §E).

## Rollback
- Single feature branch; `git revert`. DEDUP-key flips are idempotent + in-place;
  reverting the const + re-running boot restores the prior key.

## Observability
- No new metric needed (feed is a label on existing rows). The per-feed rows make
  `feed`-grouped QuestDB queries possible (Dhan-vs-Groww comparison) — the operator's goal.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan-only (Groww off) | every row `feed='dhan'`; identical to today |
| 2 | Both feeds on | prev_day/candles/ticks keep BOTH feeds' rows per (ts,sid,seg) |
| 3 | universe master | ONE row per (sid,segment) regardless of feed (NOT duplicated) |
