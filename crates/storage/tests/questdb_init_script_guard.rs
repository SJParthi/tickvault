//! questdb-init.sh ⟷ retired-table sweep consistency guard (Track A, 2026-07-18).
//!
//! `scripts/questdb-init.sh` runs on every `make docker-up`. Before the
//! 2026-07-18 prune it re-CREATED 13 tables the boot-time retired-object
//! sweep (`shadow_persistence::drop_legacy_candle_objects`) drops — so a
//! swept deployment resurrected retired tables (without their persistence
//! code) on the next docker-up. This build-failing guard pins:
//!
//! 1. NO retired table name (parsed from the live `RETIRED_QUESTDB_TABLES`
//!    source array) is ever a `CREATE TABLE` target in the script again.
//! 2. NO `movers_*` / `candles_1s` object is created by the script.
//! 3. Every `CREATE TABLE` target in the script is in the explicit KEEP
//!    allowlist (today: `ticks` only — live-table DDL is owned by the
//!    boot-reachable Rust ensure fns, see
//!    crates/app/tests/ensure_ddl_boot_wiring_guard.rs).
//! 4. The `ticks` CREATE carries the FINAL schema columns (`feed`,
//!    `capture_seq`) and the 5-key DEDUP
//!    `(ts, security_id, segment, capture_seq, feed)` — the pre-prune
//!    script shipped the stale 3-key form.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolvable")
}

fn init_script() -> String {
    let path = repo_root().join("scripts/questdb-init.sh");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Parse the string literals out of the `RETIRED_QUESTDB_TABLES` array in
/// the live shadow_persistence.rs source — so the guard tracks the drop
/// list automatically as it is extended.
fn retired_tables_from_source() -> Vec<String> {
    let src_path = repo_root().join("crates/storage/src/shadow_persistence.rs");
    let src = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", src_path.display()));
    let start = src
        .find("const RETIRED_QUESTDB_TABLES")
        .expect("RETIRED_QUESTDB_TABLES array present in shadow_persistence.rs");
    let block_end = src[start..]
        .find("];")
        .expect("RETIRED_QUESTDB_TABLES array terminated");
    let block = &src[start..start + block_end];
    let mut names = Vec::new();
    let mut rest = block;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let close = after.find('"').expect("closing quote");
        names.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    assert!(
        names.len() >= 18,
        "RETIRED_QUESTDB_TABLES parse looks vacuous ({} names) — the \
         scanner must track the real drop list",
        names.len()
    );
    names
}

/// Extract every `CREATE TABLE IF NOT EXISTS <name>` target from the
/// init script.
fn script_create_targets(script: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for chunk in script.split("CREATE TABLE IF NOT EXISTS ").skip(1) {
        let name: String = chunk
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            targets.push(name);
        }
    }
    targets
}

#[test]
fn test_init_script_never_recreates_a_retired_table() {
    let script = init_script();
    let targets = script_create_targets(&script);
    assert!(
        !targets.is_empty(),
        "init script must still create the KEEP set (ticks) — an empty \
         target list means the scanner or the script broke"
    );
    for retired in retired_tables_from_source() {
        assert!(
            !targets.contains(&retired),
            "scripts/questdb-init.sh re-creates retired table `{retired}` — \
             docker-up would resurrect a table the boot sweep drops"
        );
    }
    // The candle base table + the movers grid are dropped by dedicated
    // sweep arms (not the RETIRED array) — pin them separately.
    for target in &targets {
        assert_ne!(target, "candles_1s", "candles_1s is sweep-dropped");
        assert!(
            !target.starts_with("movers"),
            "movers grid is sweep-dropped: {target}"
        );
        assert!(
            !target.starts_with("candles_"),
            "candle tables are owned by the app ensure DDL (DEDUP-keyed): {target}"
        );
    }
}

#[test]
fn test_init_script_create_targets_are_keep_allowlisted() {
    let script = init_script();
    let allow = ["ticks"];
    for target in script_create_targets(&script) {
        assert!(
            allow.contains(&target.as_str()),
            "init script creates `{target}` which is not in the KEEP \
             allowlist {allow:?} — live-table DDL belongs to the \
             boot-reachable Rust ensure fns (add it there, or extend the \
             allowlist with a dated note)"
        );
    }
}

#[test]
fn test_init_script_ticks_ddl_is_final_schema_with_5_key_dedup() {
    let script = init_script();
    let ticks_create = script
        .split("CREATE TABLE IF NOT EXISTS ticks ")
        .nth(1)
        .expect("init script must create ticks (scoreboard reads it)");
    for col in ["feed SYMBOL", "capture_seq LONG", "payload_hash LONG"] {
        assert!(
            ticks_create.contains(col),
            "ticks CREATE must carry the final-schema column `{col}`"
        );
    }
    assert!(
        script.contains(
            "ALTER TABLE ticks DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, capture_seq, feed)"
        ),
        "ticks DEDUP must be the final 5-key (ts, security_id, segment, capture_seq, feed)"
    );
}

/// Scanner self-test: a synthetic script with a retired-name CREATE must
/// be caught (the parse is not vacuous).
#[test]
fn test_create_target_scanner_detects_names() {
    let fixture = r#"execute_ddl "x" "CREATE TABLE IF NOT EXISTS option_greeks (a INT)""#;
    assert_eq!(script_create_targets(fixture), vec!["option_greeks"]);
}
