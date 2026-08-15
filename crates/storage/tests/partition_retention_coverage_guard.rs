//! Meta-guard: every storage table-name constant MUST have a retention
//! decision — i.e. it must be covered by the partition manager's HOUR/DAY
//! sweep lists OR the EXEMPT list in `partition_manager.rs`.
//!
//! This prevents the 2026-06-05 stale-coverage bug from ever recurring: the
//! sweep lists had drifted to name DELETED tables while OMITTING every live
//! growing table, so nothing was swept (a storage/cost runaway). With this
//! guard, adding a new `*_audit`/data table with a `…TABLE…` constant but
//! forgetting to give it a retention decision fails the build.

use std::fs;
use std::path::Path;

/// Collect every lowercase snake-case string literal in a file. The three
/// retention lists (and the pinning unit tests) in `partition_manager.rs` name
/// each covered table, so a table is "covered" iff its name appears here.
fn string_literals(text: &str) -> Vec<String> {
    text.split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1) // odd segments are inside quotes
        .map(|(_, s)| s.to_string())
        .filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

/// Is this a plausible bare table name — lowercase/digits/underscore only?
///
/// This is what excludes DDL strings (spaces, uppercase) from being mistaken
/// for table names.
fn looks_like_table_name(val: &str) -> bool {
    !val.is_empty()
        && val
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Every `NAME = "value"` pair in a file, for constants whose value is a bare
/// table name. Used to build the alias-resolution map from `constants.rs`.
fn table_name_bindings(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.contains("const ") || !t.contains(": &str") {
            continue;
        }
        let Some(name_start) = t.find("const ") else {
            continue;
        };
        let after = &t[name_start + 6..];
        let Some(colon) = after.find(':') else {
            continue;
        };
        let name = after[..colon].trim().to_string();
        if let Some(start) = t.find("= \"") {
            let rest = &t[start + 3..];
            if let Some(end) = rest.find('"') {
                let val = &rest[..end];
                if looks_like_table_name(val) {
                    out.push((name, val.to_string()));
                }
            }
        }
    }
    out
}

/// Extract the table NAME behind every `const …TABLE… : &str = …` declaration
/// in a storage module — whether it is written as a string literal or as an
/// ALIAS of a constant declared in `crates/common/src/constants.rs`.
///
/// The alias arm was added 2026-08-15 after a test audit found this guard blind
/// to the two HIGHEST-VOLUME tables in the system. `TICKS_TABLE` and
/// `MARKET_DEPTH_TABLE` are both written as
/// `pub const X_TABLE: &str = QUESTDB_TABLE_X;` — no `= "` on the line — so the
/// old literal-only extractor skipped them entirely, and the literal they point
/// at lives in the `common` crate, outside the `src` directory this guard walks.
///
/// A guard whose stated purpose is "adding a new data table but forgetting a
/// retention decision fails the build" was therefore silent about `ticks` and
/// `market_depth`. Both happened to be covered by hand; nothing forced that.
fn table_name_constants(text: &str, aliases: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.contains("const ") || !t.contains("TABLE") || !t.contains(": &str") {
            continue;
        }
        // Arm 1: a direct string literal — `= "foo_audit";`
        if let Some(start) = t.find("= \"") {
            let rest = &t[start + 3..];
            if let Some(end) = rest.find('"') {
                let val = &rest[..end];
                if looks_like_table_name(val) {
                    out.push(val.to_string());
                    continue;
                }
            }
        }
        // Arm 2: an ALIAS — `= QUESTDB_TABLE_FOO;` — resolved through the map.
        let Some(eq) = t.find('=') else { continue };
        let rhs = t[eq + 1..].trim().trim_end_matches(';').trim();
        // Accept both a bare identifier and a path-qualified one.
        let ident = rhs.rsplit("::").next().unwrap_or(rhs).trim();
        if ident.is_empty() || ident.contains(' ') || ident.contains('"') {
            continue;
        }
        if let Some((_, val)) = aliases.iter().find(|(name, _)| name == ident) {
            out.push(val.clone());
        }
    }
    out
}

/// The `NAME -> "table"` map, read from the crate that actually holds the
/// literals. Resolving aliases needs it; without it arm 2 above is inert.
fn common_table_aliases() -> Vec<(String, String)> {
    let constants = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../common/src/constants.rs")
        .canonicalize()
        .expect("crates/common/src/constants.rs must exist — the alias map depends on it");
    let text = fs::read_to_string(&constants).expect("read constants.rs");
    table_name_bindings(&text)
}

#[test]
fn every_storage_table_constant_has_a_retention_decision() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let pm =
        fs::read_to_string(src.join("partition_manager.rs")).expect("read partition_manager.rs");
    let covered = string_literals(&pm);
    let aliases = common_table_aliases();

    let mut missing: Vec<String> = Vec::new();
    let mut seen_any_alias = false;
    for entry in fs::read_dir(&src).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        for tbl in table_name_constants(&text, &aliases) {
            if tbl == "ticks" || tbl == "market_depth" {
                seen_any_alias = true;
            }
            if !covered.contains(&tbl) {
                missing.push(format!("{tbl}  (constant in {})", path.display()));
            }
        }
    }

    // NON-VACUITY. The alias arm is the whole point of the 2026-08-15 fix, and
    // an alias resolver that silently resolves nothing looks exactly like one
    // that works — the failure mode this guard exists to catch, reproduced
    // inside the guard itself. `ticks` and `market_depth` are BOTH declared as
    // aliases, so if neither surfaces, arm 2 is dead.
    assert!(
        seen_any_alias,
        "the alias resolver found neither `ticks` nor `market_depth` — both are declared as \
         `pub const X_TABLE: &str = QUESTDB_TABLE_X;`, so arm 2 of table_name_constants has \
         stopped resolving and this guard is once again blind to the two highest-volume tables"
    );

    assert!(
        missing.is_empty(),
        "these storage table-name constants have NO retention decision — add each \
         to HOUR_PARTITIONED_TABLES / DAY_PARTITIONED_TABLES or \
         RETENTION_EXEMPT_TABLES in partition_manager.rs:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn guard_helpers_are_sane() {
    // self-test the extractors so a future refactor can't silently neuter them
    let sample = r#"
        pub const FOO_TABLE: &str = "foo_audit";
        const BAR_DDL: &str = "CREATE TABLE x ( a int )";
        let xs = &["foo_audit", "kept"];
    "#;
    let no_aliases: Vec<(String, String)> = Vec::new();
    assert_eq!(
        table_name_constants(sample, &no_aliases),
        vec!["foo_audit".to_string()]
    );
    assert!(string_literals(sample).contains(&"foo_audit".to_string()));
    assert!(string_literals(sample).contains(&"kept".to_string()));
    // the DDL string (has spaces/uppercase) must NOT be treated as a table name
    assert!(
        !table_name_constants(sample, &no_aliases)
            .iter()
            .any(|t| t.contains(' '))
    );

    // ARM 2 self-test — the alias form that made this guard blind to `ticks`
    // and `market_depth` until 2026-08-15. Without the alias map it must
    // resolve to NOTHING (proving the old behaviour); with it, to the name.
    let aliased = r#"pub const TICKS_TABLE: &str = QUESTDB_TABLE_TICKS;"#;
    assert!(
        table_name_constants(aliased, &no_aliases).is_empty(),
        "premise: with no alias map an aliased const is invisible — this is exactly \
         the blindness the fix removes"
    );
    let map = vec![("QUESTDB_TABLE_TICKS".to_string(), "ticks".to_string())];
    assert_eq!(
        table_name_constants(aliased, &map),
        vec!["ticks".to_string()],
        "arm 2 must resolve an aliased table constant through the common-crate map"
    );

    // The binding extractor that BUILDS that map.
    let decls = r#"
        pub const QUESTDB_TABLE_TICKS: &str = "ticks";
        pub const SOME_DDL: &str = "CREATE TABLE x ( a int )";
    "#;
    let bindings = table_name_bindings(decls);
    assert_eq!(
        bindings,
        vec![("QUESTDB_TABLE_TICKS".to_string(), "ticks".to_string())],
        "table_name_bindings must capture the name->value pair and skip DDL strings"
    );

    // The real map must be non-empty, or every alias silently resolves to
    // nothing and arm 2 is decoration.
    assert!(
        common_table_aliases()
            .iter()
            .any(|(n, _)| n.starts_with("QUESTDB_TABLE_")),
        "constants.rs yielded no QUESTDB_TABLE_* bindings — the alias map is empty"
    );
}

/// The 21 live candle tables are swept by iterating `candle_table_names()` (the
/// single source of truth) in `detach_old_partitions`. This cross-references
/// that source — closing the gap where #1022's meta-guard was blind to candle
/// tables (it only saw literals inside `partition_manager.rs`). Catches both
/// (a) deletion of the candle sweep loop and (b) a future `_shadow` rename.
#[test]
fn candle_tables_are_swept_via_single_source() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let pm =
        fs::read_to_string(src.join("partition_manager.rs")).expect("read partition_manager.rs");
    assert!(
        pm.contains("candle_table_names()"),
        "partition_manager must sweep candle tables via candle_table_names() — the candle \
         retention loop is missing (the #1022 phantom-`_shadow`-name bug would return)"
    );

    let names = tickvault_storage::shadow_persistence::candle_table_names();

    // 21 → 24 on 2026-08-11: the live-feed revival added the three
    // second-scale frames the operator's 13-timeframe requirement needs.
    //
    // The count is asserted BY NAME, not as a bare number. A count tells you
    // a number moved; it does not tell you WHICH table lost its retention
    // sweep — and an unswept candle table grows until the disk fills, which
    // is a slow failure nobody attributes to a test that once said "21".
    let expected = [
        // Second scale — 1s..15s plus 30s. These are the frames the
        // 13-timeframe requirement added, and the reason the count moved.
        "candles_1s",
        "candles_2s",
        "candles_3s",
        "candles_4s",
        "candles_5s",
        "candles_6s",
        "candles_7s",
        "candles_8s",
        "candles_9s",
        "candles_10s",
        "candles_11s",
        "candles_12s",
        "candles_13s",
        "candles_14s",
        "candles_15s",
        "candles_30s",
        // Minute scale and the day frame.
        "candles_1m",
        "candles_2m",
        "candles_3m",
        "candles_5m",
        "candles_15m",
        "candles_30m",
        "candles_60m",
        "candles_1d",
    ];
    let actual: std::collections::BTreeSet<&str> = names.iter().copied().collect();
    let want: std::collections::BTreeSet<&str> = expected.iter().copied().collect();
    let missing: Vec<_> = want.difference(&actual).collect();
    let extra: Vec<_> = actual.difference(&want).collect();
    assert!(
        missing.is_empty(),
        "these candle tables lost their retention sweep — they will grow \
         without bound: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "new candle tables appeared without being added to this ledger — \
         confirm each is swept, then list it here: {extra:?}"
    );

    for n in names {
        assert!(
            n.starts_with("candles_") && !n.contains("_shadow"),
            "candle table name must be plain candles_<TF> (no _shadow): {n}"
        );
    }
}
