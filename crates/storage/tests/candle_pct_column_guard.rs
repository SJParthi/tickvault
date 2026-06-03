//! Concern-C regression ratchet — `close_pct_from_prev_day` MUST stay wired
//! end-to-end across the candle persistence chain.
//!
//! Background (active-plan.md "PR-4 HOTFIX", Concern C): the Engine-B candle
//! rewrite SILENTLY DROPPED the `*_pct_from_prev_day` columns from the candle
//! tables. The operator expected visible % changes; they were gone. PR-4b
//! (#860) re-added `close_pct_from_prev_day` (operator decision 2026-05-28:
//! only `close_pct` is persisted — spot instruments have no OI and indices
//! have no volume, so `oi_pct` / `volume_pct` stay dropped).
//!
//! That regression slipped past CI because NO source-scan / unit test pinned
//! the column to the DDL or the ILP write — the only tests were inside the
//! pure pct-computation module. This file is the missing ratchet: it scans
//! the storage `src/` and fails the build if `close_pct_from_prev_day` is
//! removed from ANY of the four links in the persist chain:
//!
//!   1. CREATE TABLE DDL          — `shadow_persistence.rs`
//!   2. ALTER self-heal           — `shadow_persistence.rs`
//!   3. ILP append column write   — `shadow_candle_writer.rs`
//!   4. seal row struct field     — `shadow_seal_columns.rs`
//!   5. spill record (zero-loss)  — `seal_spill.rs`
//!
//! See: `.claude/rules/project/observability-architecture.md` (schema
//! self-heal at boot) and `.claude/rules/project/wave-6-error-codes.md`
//! (AGGREGATOR-SEAL-01 — seal-time ILP write).

#![cfg(test)]

use std::path::PathBuf;

const PCT_COLUMN: &str = "close_pct_from_prev_day";

fn storage_src(file: &str) -> (PathBuf, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "candle pct-column guard: cannot read {} — has the persist chain \
             been renamed/moved? Update this guard alongside the rename. ({e})",
            path.display()
        )
    });
    (path, content)
}

/// Strip line comments + doc comments so the guard pins the LIVE code, not a
/// comment that merely mentions the column name. Keeps the test honest: a
/// future refactor that leaves the column only in a `//` comment must fail.
fn code_only(content: &str) -> String {
    content
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("//!") && !t.starts_with("///")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// 1. CREATE TABLE DDL must declare the column as DOUBLE
// ============================================================================

#[test]
fn ddl_create_table_declares_close_pct_double_column() {
    let (path, content) = storage_src("shadow_persistence.rs");
    let code = code_only(&content);
    assert!(
        code.contains(PCT_COLUMN) && code.contains("DOUBLE"),
        "{}: candle CREATE TABLE DDL must declare `{PCT_COLUMN} DOUBLE`. \
         Concern-C regression: the Engine-B rewrite dropped the pct columns \
         once already. Re-add it to the `create_ddl` format string.",
        path.display()
    );
    // Pin the exact column-in-DDL token so a stray mention elsewhere can't
    // satisfy the check above.
    assert!(
        code.contains(&format!("{PCT_COLUMN}     DOUBLE"))
            || code.contains(&format!("{PCT_COLUMN} DOUBLE")),
        "{}: `{PCT_COLUMN}` must appear as a `DOUBLE` column in the candle \
         CREATE TABLE body.",
        path.display()
    );
}

// ============================================================================
// 2. ALTER ADD COLUMN IF NOT EXISTS self-heal must cover the column
// ============================================================================

#[test]
fn ddl_self_heal_alter_adds_close_pct_column() {
    let (path, content) = storage_src("shadow_persistence.rs");
    let code = code_only(&content);
    assert!(
        code.contains("ALTER TABLE")
            && code.contains("ADD COLUMN IF NOT EXISTS")
            && code.contains(PCT_COLUMN),
        "{}: candle tables created under the pre-2026-05-28 10-column schema \
         must auto-migrate via `ALTER TABLE {{table}} ADD COLUMN IF NOT EXISTS \
         {PCT_COLUMN} DOUBLE;`. Without this, an upgraded deployment keeps the \
         old schema and the % column stays empty forever (schema self-heal — \
         observability-architecture.md).",
        path.display()
    );
}

// ============================================================================
// 3. ILP append_seal must actually WRITE the column to QuestDB
// ============================================================================

#[test]
fn ilp_append_seal_writes_close_pct_column() {
    let (path, content) = storage_src("shadow_candle_writer.rs");
    let code = code_only(&content);
    assert!(
        code.contains(&format!(".column_f64(\"{PCT_COLUMN}\"")),
        "{}: the ILP `append_seal` builder must emit \
         `.column_f64(\"{PCT_COLUMN}\", row.{PCT_COLUMN})`. A DDL column that \
         is never written stays NULL — the operator sees no % change. This is \
         the second half of the Concern-C contract (DDL + write).",
        path.display()
    );
}

// ============================================================================
// 4. The seal row struct must carry the field (DDL ↔ struct ↔ write parity)
// ============================================================================

#[test]
fn seal_row_struct_carries_close_pct_field() {
    let (path, content) = storage_src("shadow_seal_columns.rs");
    let code = code_only(&content);
    assert!(
        code.contains(&format!("pub {PCT_COLUMN}: f64")),
        "{}: the persisted seal-row struct must carry `pub {PCT_COLUMN}: f64` \
         so the seal-time computed value reaches the ILP writer unchanged.",
        path.display()
    );
}

// ============================================================================
// 5. Spill record must preserve the field (zero-loss recovery contract)
// ============================================================================

#[test]
fn spill_record_preserves_close_pct_field() {
    let (path, content) = storage_src("seal_spill.rs");
    let code = code_only(&content);
    assert!(
        code.contains(PCT_COLUMN),
        "{}: the on-disk spill record must carry `{PCT_COLUMN}` so a seal that \
         overflows the ring → spill → DLQ still re-persists its % change on \
         replay. Operator charter: zero loss inside the rescue envelope.",
        path.display()
    );
}

// ============================================================================
// Self-tests for the helper (so the guard itself can't silently no-op)
// ============================================================================

// ============================================================================
// 6. Operator request 2026-06-02 — `change_pct` + `open_gap_pct` columns must
//    stay wired end-to-end across the SAME persist chain (DDL + ALTER + ILP
//    write + seal-row struct + spill record). `change_pct` is DERIVED from
//    `close_pct_from_prev_day` at the seal-row extractor (no LiveCandleState
//    field), so its struct/spill presence is the row/serialized-seal field.
// ============================================================================

/// Assert a derived/stamped pct column is wired across DDL, ALTER, ILP write,
/// seal-row struct and spill record.
fn assert_pct_column_wired_end_to_end(col: &str) {
    let (sp_path, sp) = storage_src("shadow_persistence.rs");
    let sp = code_only(&sp);
    assert!(
        sp.contains(&format!("{col}                  DOUBLE"))
            || sp.contains(&format!("{col}                DOUBLE"))
            || sp.contains(&format!("{col} DOUBLE")),
        "{}: candle CREATE TABLE DDL must declare `{col} DOUBLE` (operator \
         request 2026-06-02).",
        sp_path.display()
    );
    assert!(
        sp.contains(&format!("ADD COLUMN IF NOT EXISTS {col} DOUBLE")),
        "{}: candle schema self-heal must `ALTER TABLE {{table}} ADD COLUMN IF \
         NOT EXISTS {col} DOUBLE;` so upgraded deployments backfill the column.",
        sp_path.display()
    );

    let (w_path, w) = storage_src("shadow_candle_writer.rs");
    let w = code_only(&w);
    assert!(
        w.contains(&format!(".column_f64(\"{col}\"")),
        "{}: the ILP `append_seal` builder must emit `.column_f64(\"{col}\", \
         row.{col})` — a DDL column never written stays NULL.",
        w_path.display()
    );

    let (r_path, r) = storage_src("shadow_seal_columns.rs");
    let r = code_only(&r);
    assert!(
        r.contains(&format!("pub {col}: f64")),
        "{}: the persisted seal-row struct must carry `pub {col}: f64`.",
        r_path.display()
    );

    let (s_path, s) = storage_src("seal_spill.rs");
    let s = code_only(&s);
    assert!(
        s.contains(&format!("pub {col}: f64")),
        "{}: the on-disk spill record must carry `pub {col}: f64` so an \
         overflowed seal still re-persists the % on replay (zero-loss).",
        s_path.display()
    );
}

#[test]
fn change_pct_column_wired_end_to_end() {
    assert_pct_column_wired_end_to_end("change_pct");
}

#[test]
fn open_gap_pct_column_wired_end_to_end() {
    assert_pct_column_wired_end_to_end("open_gap_pct");
}

#[test]
fn self_test_code_only_strips_comments_keeps_code() {
    let sample = "// close_pct_from_prev_day in a comment\nlet x = close_pct_from_prev_day;";
    let stripped = code_only(sample);
    assert!(!stripped.contains("in a comment"));
    assert!(stripped.contains("let x = close_pct_from_prev_day;"));
}
