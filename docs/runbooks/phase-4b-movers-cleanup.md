# Phase 4b Operator Runbook — `stock_movers` + `option_movers` Cleanup

> **Status:** Active runbook. Execute step-by-step ONLY after Phase 4a verification (24h v1↔v2 dual-path soak per active-plan §6 row 4) clears successfully.
>
> **Authority chain:** active-plan-29-tf-and-movers-deletion.md §16 Phase 4b row.

## Why this runbook exists

Phase 3 + Phase 4a shipped the in-RAM 29-TF cascade and the dormant `/api/movers/v2` endpoint. Phase 4b retires the legacy `stock_movers` + `option_movers` QuestDB tables and the now-dead writer code that targeted them.

The writers were already wired to `None` in `crates/app/src/main.rs:2728` per the 2026-05-03 audit. Production has not been writing to those tables since that commit. Phase 4b is the cleanup: drop the tables, delete the dead writer code, retire the 3 movers ErrorCodes.

## Pre-conditions (MUST all be true before starting)

- [ ] Phase 3 verified — 14 trading days of green RAM≡DB parity per `make parity-soak` plus operator-run live parity reports
- [ ] Phase 4a verified — operator flipped `config.api.movers_v2_enabled = true`, ran 24h dual-path comparison of `/api/movers` (v1, QuestDB) vs `/api/movers/v2` (RAM), zero mismatches
- [ ] Phase 4a verification report committed to `docs/operator/phase-4a-soak-YYYY-MM-DD.md`

If ANY box is unchecked, STOP. Do not proceed.

## Step 1 — Drop the legacy tables

Connect to the running QuestDB instance (postgres-wire on port 8812 by default) using the `tickvault` user. Run:

```sql
-- Phase 4b table drop. The writer code in production was already
-- inert (None per main.rs:2728). This drop reclaims disk + removes
-- the SEBI-audit-irrelevant tables.
DROP TABLE IF EXISTS stock_movers;
DROP TABLE IF EXISTS option_movers;
```

**Verification:**

```sql
-- Both queries MUST return zero rows.
SELECT count(*) FROM tables() WHERE name = 'stock_movers';
SELECT count(*) FROM tables() WHERE name = 'option_movers';
```

If either returns 1, the drop failed — investigate. Possible causes: another process holding a lock; QuestDB running out of disk during the drop. Retry after addressing.

## Step 2 — Wait 30 trading days

Per active-plan §6 row 4, the post-deploy clean window is 30 trading days. During this window:

- [ ] No alerts on `tv_movers_persist_errors_total` (ratchet — it should never fire because writers are already dormant)
- [ ] No alerts on `MOVERS-01` / `MOVERS-02` / `MOVERS-03` ErrorCodes
- [ ] Operator runs the `make parity-soak` framework-validation harness daily; zero regressions

If ANY alert fires, STOP. Do NOT proceed to Step 3. Investigate, fix, restart the 30-day clock.

## Step 3 — Delete the dead writer code (separate PR)

Once Step 2's 30-day window is clean:

1. Open a PR titled `feat(storage,core): Phase 4b cleanup — delete legacy movers writers + retire ErrorCodes`
2. Delete `crates/storage/src/movers_persistence.rs` (all of `StockMoversWriter`, `OptionMoversWriter`, ~1,700 LoC)
3. Remove the `Option<StockMoversWriter>` / `Option<OptionMoversWriter>` parameters from `run_tick_processor` in `crates/core/src/pipeline/tick_processor.rs`
4. Remove the call sites in `main.rs` that bind these to `None`
5. Retire the 3 movers ErrorCodes from `crates/common/src/error_code.rs`:
   - `Movers01StockPersistFailed`
   - `Movers02OptionPersistFailed`
   - `Movers03PreopenPersistFailed`
6. Remove the corresponding entries from any rule-file documentation (cross-ref test will catch stragglers)
7. Update `quality/coverage-thresholds.toml` if any movers-specific entries exist

## Step 4 — Archive the active plan

Once Step 3's PR merges:

1. Move `.claude/plans/active-plan-29-tf-and-movers-deletion.md` to `.claude/plans/archive/2026-MM-DD-29-tf-movers-deletion.md`
2. Update `CLAUDE.md` `## CURRENT CONTEXT` if it referenced the active plan
3. Tag the merge commit `phase-4b-complete-YYYY-MM-DD`

## Rollback procedure (if needed)

If anything in Step 1 or Step 2 fails catastrophically:

1. The dropped tables are GONE — there is no undo. Restore from S3 cold-storage snapshots if any rows were SEBI-relevant.
2. Re-create the tables via the original DDL in `scripts/questdb-init.sh` (look for the `stock_movers` + `option_movers` `CREATE TABLE` blocks before they were removed).
3. Set `config.api.movers_v2_enabled = false` to reactivate v1 path. Restart app.
4. Investigate what caused the failure before retrying Phase 4b.

The `stock_movers` / `option_movers` tables were NOT SEBI-relevant — they held derived movers snapshots, not original tick / order data. SEBI retention applies to `ticks` (5y hot) + `orders` (forever) + `historical_candles` (5y cold), none of which Phase 4b touches.

## Why this is a runbook, not an automated PR

The drop is destructive and irreversible. Automating it via a Rust migration would mean a code-side mistake auto-deletes data. Keeping the SQL in operator hands means the human reviews the actual statements being run against production QuestDB before pressing enter. Per CLAUDE.md "Executing actions with care" and "Destructive operations require user confirmation" — `DROP TABLE` is exactly the operation that should NOT be code-driven.
