# Implementation Plan: Human-readable QuestDB console views (`ticks_named` + `candles_named`)

**Status:** VERIFIED
**Date:** 2026-07-08
**Approved by:** Parthiban (operator) — ultracode directive 2026-07-08 (human-readable QuestDB views)

> Per-item guarantee matrix: see `.claude/rules/project/per-wave-guarantee-matrix.md` (cross-reference).
> Changed crates: **tickvault-storage** (`crates/storage/src/console_views.rs`,
> `crates/storage/src/lib.rs`, `crates/storage/src/shadow_persistence.rs` comment)
> + **tickvault-app** (`crates/app/src/main.rs` boot wiring).

## Design

Two plain (non-materialized) QuestDB views, created idempotently at every
boot: `ticks_named` (over `ticks`) and `candles_named` (over `candles_1m`),
each LEFT-joined against a dimension subquery of `instrument_lifecycle`
(`SELECT security_id, exchange_segment, feed, symbol_name, display_name,
instrument_type ... WHERE dry_run = false`) on the composite predicate
`security_id + segment/exchange_segment + feed` (I-P1-11 + feed-in-key).
Explicit column lists (never `SELECT *`), identity-first column order.

New module `crates/storage/src/console_views.rs` (NOT feature-gated) with
pure DDL builders `ticks_named_view_ddl()` / `candles_named_view_ddl()`
sharing a private `lifecycle_dim_subquery()`, and fail-soft
`ensure_named_views(&QuestDbConfig)` following the house `ensure_*` norm
(unit return, never Result, never panic) with the exact
`shadow_persistence` HTTP-CLIENT-01 degrade arm (static site label
`named_views_ensure`). FEATURE-GATE FIX: the internal call to
`ensure_instrument_lifecycle_table` is
`#[cfg(feature = "daily_universe_fetcher")]`-gated because that entire
persistence module is `#![cfg]`-gated — the DDL builders compile
feature-free, with the hardcoded `instrument_lifecycle` mirror pinned by a
feature-gated const-equality ratchet.

Boot wiring: BOTH boot paths in `crates/app/src/main.rs` (fast
crash-recovery arm + slow `start_dhan_lane` arm), a sequential `.await`
immediately AFTER the base-table ensure `tokio::join!` completes so
`ticks` + `candles_1m` exist before CREATE VIEW validates its column
references.

Join correctness: the lifecycle master's designated `ts` is a pinned
epoch-0 constant, so DEDUP collapses to exactly ONE row per
`(security_id, exchange_segment, feed)` — the join can never multiply
rows; no LATEST ON needed.

**O-honesty:** the views are cold-path analyst tooling — O(join) at
SELECT time, **honestly O(N)**, never claimed O(1). Zero hot-path
impact: no writer/indicator/strategy/risk code reads the views; the
RAM-first SELECT ban is untouched; boot cost is a handful of idempotent
10s-timeout-bounded HTTP GETs.

## Edge Cases

- **Empty/partial lifecycle master** (fresh DB, pre-reconcile boot):
  LEFT JOIN yields NULL `symbol_name` rows — a diagnostic ("streaming
  instrument absent from the lifecycle master", audit Rule 11), never a
  dropped row. Probe-verified empty-table-safe.
- **Dry-run rows** (§27 isolation): filtered `dry_run = false` INSIDE
  the dimension subquery so LEFT semantics are preserved.
- **Groww bit-62 synthetic index ids**: exist only under
  `feed = 'groww'`; the `feed` predicate keeps them from ever matching a
  Dhan row (and vice versa).
- **Feature-off builds** (`daily_universe_fetcher` disabled): the
  lifecycle table never exists → the CREATE VIEW warn-fails harmlessly
  each boot (those builds never write lifecycle rows anyway); the DDL
  builders still compile.
- **Expired instruments**: NO `lifecycle_state = 'active'` filter —
  historical ticks/candles of expired instruments must still resolve
  names; the pinned-ts single-row invariant already prevents duplicates.
- **Legacy candle sweep collision**: verified the
  `drop_legacy_candle_objects` sweep lists can never match `*_named`;
  a guard comment was added at the sweep constants.

## Failure Modes

- **reqwest client build failure** → HTTP-CLIENT-01 degrade arm:
  `error!` with `code = ErrorCode::HttpClient01BuildFailed.code_str()` +
  `tv_http_client_build_failed_total{site="named_views_ensure"}` +
  graceful return. Views skipped this boot; next boot re-runs
  (idempotent; read-only projections — no data path affected, no
  duplicate-row window). Rule-file §1 row added.
- **DDL non-2xx** (e.g. base table missing on a degraded boot) →
  `warn!` with status + 200-char body prefix; self-heals next boot.
  Both views attempted independently — one failing never blocks the
  other.
- **QuestDB down / transport error** → `error!` per view; boot never
  blocks (fail-soft house norm).
- **View definition drift**: `CREATE VIEW IF NOT EXISTS` is not
  drop-and-recreate — a definition change requires a documented one-time
  manual `DROP VIEW` + reboot (runbook).

## Test Plan

The 9 pure DDL-string ratchets in `console_views.rs::tests` (the 7 below
plus the per-builder single-statement pins
`test_ticks_named_view_ddl_is_single_terminated_statement` +
`test_candles_named_view_ddl_is_single_terminated_statement`):
1. `test_view_name_constants_stable`
2. `test_both_ddls_use_create_view_if_not_exists` (bare CREATE VIEW is
   non-idempotent = boot-regression class)
3. `test_both_ddls_left_join_on_composite_feed_key` (incl. no non-LEFT
   ` JOIN ` occurrence)
4. `test_ddls_filter_dry_run_inside_subquery` (placement pin: filter
   index < `) il` index)
5. `test_ddls_never_select_star`
6. `test_ddls_select_identity_columns_first_from_correct_bases`
7. `#[cfg(feature = "daily_universe_fetcher")]
   test_lifecycle_dim_matches_persistence_const`

Plus: `cargo test -p tickvault-storage` (default features) AND
`cargo test -p tickvault-storage --features daily_universe_fetcher`;
`cargo clippy -p tickvault-storage -p tickvault-app -- -D warnings`;
`cargo check -p tickvault-app`. One live smoke via `make questdb` /
`mcp questdb_sql` before merge — the multi-key `ON a=b AND c=d` view
form is probe-class-Verified for JOINs generally but this exact
statement is Assumed until smoked.

## Rollback

`DROP VIEW IF EXISTS ticks_named; DROP VIEW IF EXISTS candles_named;`
plus revert the two `ensure_named_views` call sites in
`crates/app/src/main.rs`. Views are stateless read-only projections —
zero data risk, no migration, no writer impact. For a future definition
change (not a rollback): manual `DROP VIEW <name>` then reboot — the
next boot recreates from the new DDL (documented in the runbook).

## Observability

- `info!(view, "named console view ready")` per view on 2xx;
  `warn!(view, %status, body, "named view DDL non-2xx — retries next
  boot")`; `error!(view, ?err, "named view DDL request failed")` —
  mirrors the `shadow_persistence::run_ddl` level ladder exactly
  (meta-guard-safe: no persist/flush/drain phrasing at warn level).
- `tv_http_client_build_failed_total{site="named_views_ensure"}` static
  counter on the HTTP-CLIENT-01 degrade arm, with the paired typed
  `error!` naming the degrade consequence.
- Rule-file: `.claude/rules/project/http-client-error-codes.md` §1
  gains the `named_views_ensure` degrade row (its §2 trigger fires on
  the counter string).
- Runbook: `docs/runbooks/questdb-console-queries.md` — copy-paste
  queries, NULL-name semantics, SAMPLE BY caveat, fallback JOIN SQL,
  introspection commands, definition-change procedure.

## Plan Items

- [x] New module `crates/storage/src/console_views.rs` — pure DDL
      builders + fail-soft `ensure_named_views` + HTTP-CLIENT-01 degrade
      arm + feature-gated lifecycle ensure
  - Files: crates/storage/src/console_views.rs, crates/storage/src/lib.rs
  - Tests: test_view_name_constants_stable,
    test_both_ddls_use_create_view_if_not_exists,
    test_both_ddls_left_join_on_composite_feed_key,
    test_ddls_filter_dry_run_inside_subquery,
    test_ddls_never_select_star,
    test_ddls_select_identity_columns_first_from_correct_bases,
    test_lifecycle_dim_matches_persistence_const
- [x] Boot wiring in BOTH paths (fast crash-recovery arm + slow
      `start_dhan_lane` arm), sequential after the base-table ensure join
  - Files: crates/app/src/main.rs
  - Tests: cargo check -p tickvault-app (wiring satisfies
    pub-fn-wiring-guard — two call sites)
- [x] Legacy-sweep exclusion guard comment at the sweep constants
  - Files: crates/storage/src/shadow_persistence.rs
  - Tests: n/a (comment only)
- [x] Rule-file degrade row + runbook
  - Files: .claude/rules/project/http-client-error-codes.md,
    docs/runbooks/questdb-console-queries.md
  - Tests: n/a (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fresh DB boot, lifecycle empty | Views created; NULL symbol_name rows until reconcile — diagnostic, not loss |
| 2 | Re-boot with views present | `IF NOT EXISTS` no-op; info! per view |
| 3 | reqwest client build fails | HTTP-CLIENT-01 error! + counter, graceful return, next boot self-heals |
| 4 | Base table missing (degraded boot) | warn! non-2xx per view; retries next boot |
| 5 | Dhan + Groww rows for same instrument | Distinct rows per feed; no cross-feed mislabel |
| 6 | Operator needs SAMPLE BY | Use base tables (view carries no designated ts) — runbook documents |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage:
idempotent `CREATE VIEW IF NOT EXISTS` (probe-Verified on QuestDB 9.3.5,
empty-table-safe), LEFT-join over the pinned-ts single-row lifecycle
master (join can never multiply rows), fail-soft HTTP-CLIENT-01 degrade
with next-boot self-heal, 7 build-failing DDL ratchets. NOT claimed:
O(1) — the views are O(join) cold-path console tooling (honestly O(N)
at SELECT time, zero hot-path impact); NOT claimed: exact-statement
server acceptance until the pre-merge live smoke against `make questdb`
passes (the multi-key ON view form is Assumed pending that one run).
