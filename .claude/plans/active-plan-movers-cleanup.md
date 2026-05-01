# Implementation Plan: Drop dead movers_22tf + rename movers_unified → movers

**Status:** VERIFIED
**Date:** 2026-05-01
**Approved by:** Parthiban (operator confirmed "go ahead dude")
**Branch:** `claude/error-summary-writer-5qfy0`

## Context

Operator's QuestDB tree currently shows three flavours of movers tables:

1. **Dead `movers_{1s..1h}` (25 tables)** — created by `movers_22tf_persistence` on every boot via DDL even though the writer pipeline has been disabled behind `TICKVAULT_MOVERS_22TF_PIPELINE_ENABLED=1` (default off). Tables are empty. Dead code.
2. **Live `movers_unified_1s` base + 24 mat views (`movers_unified_{10s..1h}`)** — actively populated by `spawn_movers_unified_pipeline` (Wave 5 Item 25/27). Bhavcopy cross-check at 16:30 IST reads from `movers_unified_1s` (Wave 5 Item 26 L2). Architecture is correct; the `_unified_` suffix is clutter.
3. **Legacy `stock_movers`/`option_movers`/`top_movers`** — load-bearing for `/api/top-movers` and `depth_20_dynamic_subscriber` (DEPTH-DYN-01/03). NOT touched by this plan.

Operator's stated intent: `ticks` + `candles_1s` + `movers_1s` are the only non-view base tables; everything else lives as a materialized view.

## Plan Items

### Phase 1 — Delete dead movers_22tf

- [x] **1.1 Delete the 11 source files**
  - Files: `crates/storage/src/movers_22tf_persistence.rs`, `crates/storage/src/movers_22tf_writer.rs`, `crates/core/src/pipeline/movers_22tf_{boot,scheduler,supervisor,tracker,writer_state}.rs`, `crates/core/benches/movers_22tf_primitives.rs`, `crates/core/tests/{chaos_movers_22tf_throughput,dhat_movers_22tf_primitives}.rs`, `crates/storage/tests/movers_22tf_integration_guard.rs`, `.claude/rules/project/movers-22tf-error-codes.md`

- [x] **1.2 Remove ErrorCode variants**
  - Files: `crates/common/src/error_code.rs`
  - Variants removed: `Movers22Tf01WriterIlpFailed`, `Movers22Tf02SchedulerPanic`, `Movers22Tf03UniverseDrift`
  - Tests: `error_code_rule_file_crossref.rs` symmetric — variants gone + rule file gone

- [x] **1.3 Wire out boot-side references**
  - Files: `crates/app/src/main.rs` (delete movers_22tf DDL call line 1951, env-var gate block 2014–2046, `boot_movers_22tf_pipeline` fn 8715+), `crates/storage/src/lib.rs`, `crates/core/src/pipeline/mod.rs`, `crates/core/src/pipeline/tick_processor.rs` (delete dual-feed block 1287–1316), `crates/core/Cargo.toml` (delete bench entry), `crates/common/src/mover_types.rs` (scrub doc-comment refs only — types stay)

- [x] **1.4 Wire out infrastructure references**
  - Files: `.claude/hooks/banned-pattern-scanner.sh`, `scripts/doctor.sh`, `deploy/docker/grafana/provisioning/alerting/alerts.yml`

- [x] **1.5 Add one-shot DROP TABLE IF EXISTS for the 25 dead tables**
  - Files: `crates/storage/src/movers_persistence.rs` (add `drop_dead_movers_22tf_tables` fn), `crates/app/src/main.rs` (call from boot DDL block before unified DDL)
  - Tests: unit test asserting the constant array length == 25

### Phase 2 — Rename movers_unified_* → movers_*

- [x] **2.1 Rename DDL/INSERT/SELECT identifiers**
  - Files: `crates/storage/src/movers_unified_persistence.rs`, `crates/storage/src/movers_unified_writer.rs`, `crates/storage/src/movers_unified_query.rs`
  - File-name + struct-name kept as-is to minimise risk
  - Constant rename: `QUESTDB_TABLE_MOVERS_UNIFIED_1S` → `QUESTDB_TABLE_MOVERS_1S`
  - Tests: ratchet `test_movers_base_table_named_movers_1s`

- [x] **2.2 Add boot-time DROP migration for old movers_unified_* tables/views**
  - Files: `crates/storage/src/movers_unified_persistence.rs::ensure_movers_unified_tables_and_views`
  - Prepend 24× `DROP MATERIALIZED VIEW IF EXISTS movers_unified_*` + `DROP TABLE IF EXISTS movers_unified_1s` BEFORE the new CREATEs
  - Tests: unit test asserting both DROP and CREATE statements present in the DDL block

- [x] **2.3 Update downstream consumers**
  - Files: `crates/app/src/bhavcopy_pipeline.rs` (constant import + 2 doc-comment refs), Grafana dashboards under `deploy/docker/grafana/dashboards/*.json` (search/replace `movers_unified_*`), `deploy/docker/grafana/provisioning/alerting/alerts.yml`

- [x] **2.4 Update plan/rule docs**
  - Files: search-and-replace `movers_unified_1s` → `movers_1s` in `.claude/plans/active-plan-wave-5-indices-only.md` and any matching rule file
  - Historical plans (`.claude/plans/active-plan-movers-22tf-redesign-v2.md`, `v2-architecture.md` etc.) left as historical record

## Order-of-operations critical detail

`movers_1s` is BOTH a name in the dead-22tf list AND the new base-table name. Boot DDL order MUST be:

1. **DROP** dead `movers_{1s..1h}` (25 tables) — Item 1.5
2. **DROP** old `movers_unified_*` (1 + 24) — Item 2.2
3. **CREATE** new `movers_1s` + 24 mat views — Item 2.1

Steps 1 + 2 must complete before step 3, otherwise the new `movers_1s` CREATE collides with the dead 22tf `movers_1s`.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot on clean DB | 25 dead DROPs no-op; new `movers_1s` + 24 mat views created |
| 2 | Boot on existing DB with old `movers_unified_*` | DROPs remove old; new `movers_*` replaces them. Movers data is intra-day reset — no historical loss |
| 3 | Boot on existing DB with 25 dead `movers_*tf` tables | DROPs remove them; new `movers_1s` does NOT clash (DROP runs BEFORE CREATE) |
| 4 | `cargo test --workspace` | All tests pass incl. ErrorCode cross-ref + DEDUP segment guard + dashboard guard |
| 5 | Pre-push gates | banned-pattern, plan-verify, fmt, clippy clean |

## Verification

```bash
cargo check --workspace
cargo test -p tickvault-common --lib
cargo test -p tickvault-storage
cargo test -p tickvault-core --lib
cargo test -p tickvault-app --lib
bash .claude/hooks/banned-pattern-scanner.sh
bash .claude/hooks/plan-verify.sh
python3 -c "import yaml; yaml.safe_load(open('deploy/docker/grafana/provisioning/alerting/alerts.yml'))"
```

## Per-Wave Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows apply.

## Files NOT touched

- `crates/storage/src/movers_persistence.rs` (legacy stock/option/top_movers — stays)
- `crates/core/src/instrument/depth_20_dynamic_subscriber.rs` (only doc comment references 22tf)
- Historical `.claude/plans/active-plan-movers-22tf-redesign{,v2}.md`, `v2-*.md` (historical record)
