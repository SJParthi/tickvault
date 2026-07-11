# Implementation Plan: Human-readable QuestDB console views (`ticks_named` + `candles_named`)

**Status:** VERIFIED
**Date:** 2026-07-08
**Approved by:** Parthiban (operator) — ultracode directive 2026-07-08 (human-readable QuestDB views)

> Per-item guarantee matrix: see `.claude/rules/project/per-wave-guarantee-matrix.md` (cross-reference).
> Changed crates: **tickvault-storage** (`crates/storage/src/console_views.rs`,
> `crates/storage/src/lib.rs`, `crates/storage/src/shadow_persistence.rs` comment)
> + **tickvault-app** (`crates/app/src/main.rs` boot wiring +
> `crates/app/src/groww_activation.rs` Groww-lane wiring, review round 1).

## Design

Two plain (non-materialized) QuestDB views, created-or-converged at every
feed-enabled boot via `CREATE OR REPLACE VIEW` (review round 1 — every boot
converges the deployed definition to the code, closing the stale-definition
false-OK; probe-Verified on the pinned QuestDB 9.3.5):
`ticks_named` (over `ticks`) and `candles_named` (over `candles_1m`),
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

Boot wiring — THREE call sites so every feed-enabled boot mode is
covered (review round 1): (1) the fast crash-recovery arm + (2) the slow
`start_dhan_lane` arm in `crates/app/src/main.rs`, each a sequential
`.await` immediately AFTER the base-table ensure `tokio::join!`
completes; (3) `crates/app/src/groww_activation.rs::activate_groww_lane`
after its own base-table ensures — closing the Groww-only-boot gap
(`feeds.dhan_enabled = false`, the scale-test / groww-only lab mode)
where both Dhan-gated sites were unreachable. `ticks` + `candles_1m`
exist before CREATE VIEW validates its column references at every site;
double execution on dual-feed boots is harmless (convergent DDL).

Join correctness — HONEST ENVELOPE (review round 2, corrected here in
review round 3): the lifecycle master's designated `ts` is a pinned
epoch-0 constant, so **WHILE its DEDUP is live** the master collapses
to exactly ONE row per `(security_id, exchange_segment, feed)` and the
join cannot multiply rows in that state. That precondition is
CONDITIONAL — two documented degraded windows (the lifecycle
HTTP-CLIENT-01 auto-create window and a partial feed self-heal boot)
leave persistent same-key duplicates that a later `DEDUP ENABLE` does
NOT retro-collapse; with N lifecycle rows for one key, every matching
view row appears exactly N times. Deliberately NO `LATEST ON` collapse
(un-probed inside a VIEW on 9.3.5; silently collapsing would hide the
master-table corruption) — the runbook ships the operator detector
query (QuestDB subquery + WHERE form; QuestDB does not support HAVING)
instead.

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
- **Groww-only boot** (`feeds.dhan_enabled = false` — the documented
  groww-only / scale-test mode): covered by the dedicated
  `activate_groww_lane` call site (review round 1) — previously both
  call sites were Dhan-gated and this mode never created the views
  while `ticks`/`candles_1m` filled with `feed='groww'` rows.
- **Both feeds disabled**: no base tables are ensured and no views are
  created — nothing streams in that mode, so there is nothing to name
  (documented in the runbook TL;DR; honest-envelope wording).

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
- **View definition drift**: CLOSED (review round 1) — `CREATE OR
  REPLACE VIEW` converges the deployed definition to the code on every
  boot (probe-Verified on 9.3.5: ddl OK + definition actually
  replaced), so a future DDL edit self-heals with zero manual prod
  steps. The prior `IF NOT EXISTS` form 2xx-no-op'd on a changed
  definition (stale-definition false-OK, audit Rule 11) and required a
  manual prod `DROP VIEW` (aws-budget.md rule 8 violation); it is now
  ratchet-banned.

## Test Plan

The 10 ratchets in `console_views.rs::tests` (the 8 below plus the
per-builder single-statement pins
`test_ticks_named_view_ddl_is_single_terminated_statement` +
`test_candles_named_view_ddl_is_single_terminated_statement`):
1. `test_view_name_constants_stable`
2. `test_both_ddls_use_create_or_replace_view` (bare CREATE VIEW is
   non-idempotent = boot-error class; `IF NOT EXISTS` 2xx-no-ops on a
   changed definition = stale-definition false-OK class — both banned)
3. `test_both_ddls_left_join_on_composite_feed_key` (incl. no non-LEFT
   ` JOIN ` occurrence)
4. `test_ddls_filter_dry_run_inside_subquery` (placement pin: filter
   index < `) il` index)
5. `test_ddls_never_select_star`
6. `test_ddls_select_identity_columns_first_from_correct_bases`
7. `test_duplicate_dim_honest_envelope_ratchet` (review rounds 2+3:
   pins the runbook detector query in QuestDB's subquery + WHERE form,
   bans the QuestDB-unsupported `HAVING count()` detector form, and
   scans the runbook + module + this plan — active or archived — for
   the banned unconditional join-safety phrase; requires the
   WHILE-DEDUP-is-live envelope wording in runbook + plan)
8. `#[cfg(feature = "daily_universe_fetcher")]
   test_lifecycle_dim_matches_persistence_const`

Plus: `cargo test -p tickvault-storage` (default features) AND
`cargo test -p tickvault-storage --features daily_universe_fetcher`;
`cargo clippy -p tickvault-storage -p tickvault-app -- -D warnings`;
`cargo check -p tickvault-app`. One live smoke via `make questdb` /
`mcp questdb_sql` before merge — the multi-key `ON a=b AND c=d` view
form is probe-class-Verified for JOINs generally but this exact
statement is Assumed until smoked. The smoke must ALSO execute the
runbook's duplicate-dim DETECTOR query (review round 3: it is
docs-Verified against QuestDB's documentation — HAVING unsupported,
subquery + WHERE is the documented equivalent — but Assumed pending
that run; QuestDB was unreachable, connection refused, from the
review-round-3 session).

## Rollback

`DROP VIEW IF EXISTS ticks_named; DROP VIEW IF EXISTS candles_named;`
plus revert the three `ensure_named_views` call sites (two Dhan paths
in `crates/app/src/main.rs` + `crates/app/src/groww_activation.rs`).
Views are stateless read-only projections — zero data risk, no
migration, no writer impact. A future definition change is NOT a
rollback and needs NO manual step — `CREATE OR REPLACE VIEW` converges
the deployed definition on the next boot (documented in the runbook).

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
    test_both_ddls_use_create_or_replace_view,
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
- [x] Review round 1 fixes: `CREATE OR REPLACE VIEW` convergence
      (stale-definition false-OK closed) + Groww-lane call site
      (Groww-only-boot coverage gap closed) + doc honesty
  - Files: crates/storage/src/console_views.rs,
    crates/app/src/groww_activation.rs, crates/app/src/main.rs,
    docs/runbooks/questdb-console-queries.md
  - Tests: test_both_ddls_use_create_or_replace_view
- [x] Review round 2 fixes: honest duplicate-dim envelope — module doc
      rewritten to the conditional WHILE-DEDUP-is-live wording (the two
      degraded windows: lifecycle HTTP-CLIENT-01 auto-create +
      partial feed self-heal), runbook duplicate-dim section +
      failure-mode row + operator detector query, new build-failing
      honest-envelope ratchet
  - Files: crates/storage/src/console_views.rs,
    docs/runbooks/questdb-console-queries.md
  - Tests: test_duplicate_dim_honest_envelope_ratchet
- [x] Review round 3 fixes: detector query rewritten to QuestDB's
      documented subquery + WHERE form at all three sites (QuestDB does
      not support standard SQL HAVING — the shipped detector would have
      errored verbatim in the console); plan Design + Honest-100%
      wording corrected to the conditional envelope; ratchet extended
      to pin the new detector fragments, ban the broken
      `HAVING count()` form, and scan this plan (active or archived)
      for the banned unconditional phrase; Test Plan / ratchet counts
      corrected (10 tests, one feature-gated)
  - Files: crates/storage/src/console_views.rs,
    docs/runbooks/questdb-console-queries.md,
    .claude/plans/active-plan-questdb-named-views.md
  - Tests: test_duplicate_dim_honest_envelope_ratchet

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Fresh DB boot, lifecycle empty | Views created; NULL symbol_name rows until reconcile — diagnostic, not loss |
| 2 | Re-boot with views present | `OR REPLACE` converges the definition to the code (no-op when unchanged); info! per view |
| 3 | reqwest client build fails | HTTP-CLIENT-01 error! + counter, graceful return, next boot self-heals |
| 4 | Base table missing (degraded boot) | warn! non-2xx per view; retries next boot |
| 5 | Dhan + Groww rows for same instrument | Distinct rows per feed; no cross-feed mislabel |
| 6 | Operator needs SAMPLE BY | Use base tables (view carries no designated ts) — runbook documents |
| 7 | Groww-only boot (`dhan_enabled=false`) | Views created by the `activate_groww_lane` call site after its base-table ensures |
| 8 | Future DDL edit deployed | Next boot's `OR REPLACE` converges every provisioned DB — no manual DROP, no stale definition |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage:
convergent `CREATE OR REPLACE VIEW` (probe-Verified on QuestDB 9.3.5 —
ddl OK + definition actually replaced; empty-table-safe), every
feed-enabled boot mode wired (two Dhan paths + the Groww activation),
LEFT-join over the pinned-ts lifecycle master (exactly ONE dimension
row per key WHILE DEDUP is live; the two documented degraded windows —
the lifecycle HTTP-CLIENT-01 auto-create window and a partial feed
self-heal — N-fold-multiply view rows; detector query in the runbook),
fail-soft HTTP-CLIENT-01 degrade with next-boot self-heal, 10
build-failing ratchets (one feature-gated). NOT claimed:
O(1) — the views are O(join) cold-path console tooling (honestly O(N)
at SELECT time, zero hot-path impact); NOT claimed: exact-statement
server acceptance until the pre-merge live smoke against `make questdb`
passes (the multi-key ON view form is Assumed pending that one run, and
the duplicate-dim DETECTOR query is docs-Verified against QuestDB's own
documentation — HAVING unsupported, subquery + WHERE is the documented
form — but Assumed pending the same live smoke; QuestDB was unreachable
from the review-round-3 session).
