//! Every QuestDB table in this crate carries its column list **three times**,
//! and until now nothing compared the copies.
//!
//! 1. the `//! ## Schema` block in the module header — what a human reads at
//!    3am while debugging "why is this column empty?";
//! 2. the runtime `CREATE TABLE IF NOT EXISTS` literal — what the box actually
//!    executes at boot;
//! 3. for some tables, an `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` self-heal
//!    manifest — what an ALREADY-CREATED table gets when a column is added
//!    later.
//!
//! Copies 1 and 2 had already drifted when this guard was written:
//! `feed_scoreboard_persistence.rs` documents `feed_scoreboard_daily` in full
//! and never documented `feed_coverage_daily` at all, so eight real columns
//! -- including the whole per-instrument coverage detail -- existed only in
//! executable code. That is the exact shape of drift this file now blocks.
//!
//! ## Why the type check (test 2) matters more than it looks
//!
//! QuestDB's ILP ingestion **auto-creates a missing column** using the type it
//! infers from the wire value, and this deployment does not turn that off. So a
//! column declared `SYMBOL` in the DDL but emitted with `.column_str()` behaves
//! two different ways depending on history: on a table that already has the
//! column it lands as SYMBOL, and on a fresh table it is auto-created VARCHAR.
//! Same binary, same code, different column type -- which then changes how a
//! DEDUP key containing that column behaves. A dev container and the prod box
//! would silently disagree, which is precisely the "common runtime" property
//! this repo is built on.
//!
//! ## What this guard deliberately does NOT do
//!
//! It does not require every table to have a generic self-heal loop. Eight do
//! not, and the reasons differ per table (one builds its DDL *from* the
//! manifest, one is an internal audit table, several only ever needed the
//! single `feed` column the 2026-06-28 override added). Test 3 pins that set
//! shrink-only instead: the count can fall, never rise, so a new table cannot
//! quietly join the group that cannot heal itself.
//!
//! It also does not compare against a live database -- there is no QuestDB in
//! CI. Every check here is source-vs-source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TYPES: &[&str] = &[
    "TIMESTAMP",
    "SYMBOL",
    "LONG",
    "INT",
    "DOUBLE",
    "BOOLEAN",
    "STRING",
    "SHORT",
    "FLOAT",
    "CHAR",
    "UUID",
    "VARCHAR",
];

/// Tables whose column list is NOT covered by a generic
/// `ADD COLUMN IF NOT EXISTS {col} {ty}` self-heal loop. Adding a column to
/// one of these reaches a table that already exists only if someone also
/// writes a hand-rolled `ALTER` for it.
///
/// SHRINK-ONLY. Moving a file off this list is always welcome; adding one is
/// a deliberate decision that must be argued in review.
const NO_GENERIC_SELF_HEAL: &[(&str, &str)] = &[
    (
        "feed_episode_audit_persistence.rs",
        "one hand-rolled ALTER (the 2026-06-28 `feed` column)",
    ),
    (
        "feed_scoreboard_persistence.rs",
        "two hand-rolled ALTERs, one per scoreboard table",
    ),
    (
        "index_constituency_persistence.rs",
        "four hand-rolled ALTERs; SEBI point-in-time table, never dropped",
    ),
    (
        "instrument_lifecycle_persistence.rs",
        "two hand-rolled ALTERs; SEBI never-delete table",
    ),
    (
        "order_leg_pnl_persistence.rs",
        "DDL is BUILT FROM the manifest, so CREATE cannot drift from it; only \
         the ADD COLUMN half is absent",
    ),
    (
        "partition_archive.rs",
        "internal archive-audit table written only by the archiver itself",
    ),
    (
        "shadow_persistence.rs",
        "five hand-rolled ALTERs across the shadow candle tables",
    ),
    (
        "ws_event_audit_persistence.rs",
        "one hand-rolled ALTER (the 2026-06-28 `feed` column)",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/storage -> crates -> repo root")
        .to_path_buf()
}

fn storage_sources() -> Vec<PathBuf> {
    let dir = repo_root().join("crates/storage/src");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("crates/storage/src must be readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    out.sort();
    assert!(
        out.len() > 20,
        "storage source scan found only {} files -- the scan is broken, not the crate",
        out.len()
    );
    out
}

/// Split a module into (doc-comment lines, production lines), dropping the
/// trailing `#[cfg(test)]` module.
///
/// The cut is on a COLUMN-ZERO `#[cfg(test)]` only. An indented one is an
/// item-level attribute on a test helper, and cutting there silently discards
/// the production code below it -- which is exactly the bug the first draft of
/// this scanner had, and it hid every ILP call in one of the largest modules.
fn split_module(src: &str) -> (String, String) {
    let mut doc = String::new();
    let mut prod = String::new();
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        // Cut at the TEST MODULE only: a column-zero `#[cfg(test)]` immediately
        // followed by `mod`. Column-zero `#[cfg(test)]` also guards test-only
        // consts partway up a file, and cutting at the FIRST one discards the
        // production DDL below it -- which is exactly what this scanner did on
        // its first run, silently dropping two tables from the comparison.
        if *line == "#[cfg(test)]"
            && lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.starts_with("mod "))
        {
            break;
        }
        let t = line.trim_start();
        // Only the MODULE header (`//!`) counts as documentation of the
        // schema. A `///` doc line on a function says things like "the
        // idempotent CREATE TABLE DDL", which contains the phrase without
        // being a schema -- and treating it as one made two modules look
        // documented while their headers carry no schema block at all.
        if t.starts_with("//!") {
            doc.push_str(line);
            doc.push('\n');
        } else if !t.starts_with("//") {
            prod.push_str(line);
            prod.push('\n');
        }
    }
    (doc, prod)
}

/// Pull `name TYPE` pairs out of every `CREATE TABLE` body in `src`.
fn create_table_columns(src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = src;
    while let Some(start) = rest.find("CREATE TABLE") {
        let body_start = &rest[start..];
        // A table body ends at the designated-timestamp clause or the
        // partition clause, whichever comes first.
        let end = body_start
            .find("timestamp(")
            .into_iter()
            .chain(body_start.find("PARTITION BY"))
            .min()
            .unwrap_or(body_start.len());
        for (name, ty) in scan_pairs(&body_start[..end]) {
            out.insert(name, ty);
        }
        rest = &body_start[end.max(1)..];
    }
    out
}

/// Find `identifier TYPE` adjacency in a DDL body.
fn scan_pairs(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let toks: Vec<&str> = body
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .collect();
    for w in toks.windows(2) {
        let (name, ty) = (w[0], w[1]);
        if TYPES.contains(&ty)
            && !TYPES.contains(&name)
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        {
            out.push((name.to_string(), ty.to_string()));
        }
    }
    out
}

/// The ILP builder method each declared type must be written with.
fn accepts(ddl_type: &str, method: &str) -> bool {
    match method {
        "symbol" => ddl_type == "SYMBOL",
        "column_str" => matches!(ddl_type, "STRING" | "VARCHAR"),
        "column_i64" => matches!(ddl_type, "LONG" | "INT" | "SHORT"),
        "column_f64" => matches!(ddl_type, "DOUBLE" | "FLOAT"),
        "column_bool" => ddl_type == "BOOLEAN",
        "column_ts" => ddl_type == "TIMESTAMP",
        _ => true,
    }
}

/// Every `.method("name"` call in production source.
fn emitted_columns(src: &str) -> Vec<(String, String)> {
    const METHODS: &[&str] = &[
        "symbol",
        "column_str",
        "column_i64",
        "column_f64",
        "column_bool",
        "column_ts",
    ];
    let mut out = Vec::new();
    for m in METHODS {
        let needle = format!(".{m}(");
        let mut rest = src;
        while let Some(i) = rest.find(&needle) {
            let after = &rest[i + needle.len()..];
            let after = after.trim_start();
            if let Some(stripped) = after.strip_prefix('"')
                && let Some(close) = stripped.find('"')
            {
                let name = &stripped[..close];
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
                {
                    out.push(((*m).to_string(), name.to_string()));
                }
            }
            rest = &rest[i + needle.len()..];
        }
    }
    out
}

#[test]
fn documented_schema_matches_the_ddl_that_actually_runs() {
    let mut problems = Vec::new();
    let mut compared = 0usize;

    for path in storage_sources() {
        let src = std::fs::read_to_string(&path).expect("readable");
        let (doc, prod) = split_module(&src);
        let documented = create_table_columns(&doc);
        let created = create_table_columns(&prod);
        // Compare only when the header really carries a SQL schema block; a
        // prose-only header is caught by `every_table_documents_its_schema`.
        if !doc.contains("CREATE TABLE") || documented.is_empty() || created.is_empty() {
            continue;
        }
        compared += 1;
        let name = path.file_name().unwrap().to_string_lossy().to_string();

        for (col, ty) in &created {
            match documented.get(col) {
                None => problems.push(format!(
                    "{name}: `{col} {ty}` is created but not documented"
                )),
                Some(d) if d != ty => {
                    problems.push(format!("{name}: `{col}` documented {d}, created {ty}"));
                }
                Some(_) => {}
            }
        }
        for col in documented.keys() {
            if !created.contains_key(col) {
                problems.push(format!("{name}: `{col}` is documented but never created"));
            }
        }
    }

    assert!(
        compared >= 11,
        "only {compared} modules had both a documented and an executable schema -- \
         the scanner is broken, not the crate"
    );
    assert!(
        problems.is_empty(),
        "module header schema and runtime CREATE TABLE disagree:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn every_written_column_uses_the_builder_its_declared_type_requires() {
    let mut problems = Vec::new();
    let mut checked = 0usize;

    for path in storage_sources() {
        let src = std::fs::read_to_string(&path).expect("readable");
        let (_doc, prod) = split_module(&src);
        let mut declared = create_table_columns(&prod);
        // Fold in `("name", "TYPE")` manifests so tables whose DDL is built
        // from a const array are covered too.
        for (name, ty) in manifest_pairs(&prod) {
            declared.entry(name).or_insert(ty);
        }
        if declared.is_empty() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (method, col) in emitted_columns(&prod) {
            let Some(ty) = declared.get(&col) else {
                continue;
            };
            checked += 1;
            if !accepts(ty, &method) {
                problems.push(format!(
                    "{name}: `{col}` is declared {ty} but written with .{method}() -- on a \
                     table that does not yet have this column, ILP auto-create would give it \
                     the WRONG type"
                ));
            }
        }
    }

    assert!(
        checked >= 200,
        "only {checked} column writes were type-checked -- the scanner is broken"
    );
    assert!(
        problems.is_empty(),
        "declared column type and ILP builder disagree:\n  {}",
        problems.join("\n  ")
    );
}

/// `("name", "TYPE")` tuples used as self-heal / DDL-generation manifests.
fn manifest_pairs(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("(\"") {
        let after = &rest[i + 2..];
        if let Some(close) = after.find('"') {
            let name = &after[..close];
            let tail = after[close + 1..].trim_start();
            if let Some(tail) = tail.strip_prefix(',') {
                let tail = tail.trim_start();
                if let Some(tail) = tail.strip_prefix('"')
                    && let Some(tclose) = tail.find('"')
                {
                    let ty = &tail[..tclose];
                    if TYPES.contains(&ty) && !name.is_empty() {
                        out.push((name.to_string(), ty.to_string()));
                    }
                }
            }
        }
        rest = &rest[i + 2..];
    }
    out
}

#[test]
fn the_set_of_tables_that_cannot_self_heal_a_new_column_only_shrinks() {
    let allow: Vec<&str> = NO_GENERIC_SELF_HEAL.iter().map(|(f, _)| *f).collect();
    let mut actual = Vec::new();

    for path in storage_sources() {
        let src = std::fs::read_to_string(&path).expect("readable");
        let (_doc, prod) = split_module(&src);
        if !prod.contains("CREATE TABLE IF NOT EXISTS {") {
            continue;
        }
        if !prod.contains("ADD COLUMN IF NOT EXISTS {col}") {
            actual.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
    }

    let unexpected: Vec<&String> = actual
        .iter()
        .filter(|f| !allow.contains(&f.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "these tables gained no generic `ADD COLUMN IF NOT EXISTS {{col}} {{ty}}` self-heal, so \
         a column added to their DDL will never reach a table that already exists:\n  {unexpected:?}\n\
         Add the self-heal loop, or argue the entry onto NO_GENERIC_SELF_HEAL in review."
    );

    let stale: Vec<&&str> = allow
        .iter()
        .filter(|f| !actual.contains(&(**f).to_string()))
        .collect();
    assert!(
        stale.is_empty(),
        "these are on NO_GENERIC_SELF_HEAL but now DO self-heal -- remove them so the list \
         keeps shrinking:\n  {stale:?}"
    );
}

#[test]
fn guard_self_test() {
    // Column-zero cut only: an indented `#[cfg(test)]` must not truncate.
    let src = "//! doc\nfn a() {}\n    #[cfg(test)]\n    fn helper() {}\n#[cfg(test)]\nconst ONLY_IN_TESTS: u8 = 1;\nfn b() {}\n#[cfg(test)]\nmod t {}\n";
    let (doc, prod) = split_module(src);
    assert!(doc.contains("doc"));
    assert!(
        prod.contains("fn b()"),
        "a non-module cfg(test) truncated production source"
    );
    assert!(!prod.contains("mod t"), "top-level test module was not cut");

    // DDL parse.
    let cols = create_table_columns(
        "CREATE TABLE IF NOT EXISTS x ( ts TIMESTAMP, a SYMBOL, b LONG ) timestamp(ts)",
    );
    assert_eq!(cols.get("a").map(String::as_str), Some("SYMBOL"));
    assert_eq!(cols.get("b").map(String::as_str), Some("LONG"));

    // Type/builder pairing.
    assert!(accepts("SYMBOL", "symbol"));
    assert!(!accepts("SYMBOL", "column_str"));
    assert!(accepts("INT", "column_i64"));
    assert!(!accepts("DOUBLE", "column_i64"));

    // Emit extraction ignores non-literal names.
    let e = emitted_columns("buf.symbol(\"feed\", f)?.column_i64(\"n\", 1)?.column_f64(name, v)?;");
    assert_eq!(
        e.len(),
        2,
        "expected exactly the two literal names, got {e:?}"
    );

    // Manifest extraction.
    let m = manifest_pairs("&[(\"a\", \"SYMBOL\"), (\"b\", \"LONG\"), (\"c\", \"not-a-type\")]");
    assert_eq!(m.len(), 2, "got {m:?}");
}

/// Modules that CREATE a table at runtime but carry no `## Schema` block in
/// their module header.
///
/// A reader debugging "why is this column empty?" opens the header first. When
/// it has no schema, the only place the column list exists is a `format!`
/// string partway down the file, which is not where anyone looks at 3am.
///
/// SHRINK-ONLY. Documenting a module is always welcome; adding one here is a
/// deliberate decision that must be argued in review.
const NO_DOCUMENTED_SCHEMA: &[&str] = &[
    "depth_persistence.rs",
    "dhan_live_crossverify_persistence.rs",
    "instrument_lifecycle_persistence.rs",
    "order_leg_pnl_persistence.rs",
    "order_update_events_persistence.rs",
    "partition_archive.rs",
    "position_update_events_persistence.rs",
    "tick_persistence.rs",
];

#[test]
fn the_set_of_tables_with_no_documented_schema_only_shrinks() {
    let mut actual = Vec::new();
    for path in storage_sources() {
        let src = std::fs::read_to_string(&path).expect("readable");
        let (doc, prod) = split_module(&src);
        if !prod.contains("CREATE TABLE IF NOT EXISTS") {
            continue;
        }
        if !doc.contains("CREATE TABLE") {
            actual.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
    }

    let unexpected: Vec<&String> = actual
        .iter()
        .filter(|f| !NO_DOCUMENTED_SCHEMA.contains(&f.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "these modules create a table but their header does not show its schema:\n  {unexpected:?}"
    );

    let stale: Vec<&&str> = NO_DOCUMENTED_SCHEMA
        .iter()
        .filter(|f| !actual.contains(&(**f).to_string()))
        .collect();
    assert!(
        stale.is_empty(),
        "these are on NO_DOCUMENTED_SCHEMA but now DO document their schema -- remove them so \
         the list keeps shrinking:\n  {stale:?}"
    );
}
