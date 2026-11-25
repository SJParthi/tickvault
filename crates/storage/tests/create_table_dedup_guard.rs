//! Every `CREATE TABLE` this workspace sends carries a DEDUP key (audit J3,
//! 2026-10-26).
//!
//! `dedup_segment_meta_guard` pins the CONTENT of every `DEDUP_KEY_*`
//! constant. Nothing pinned that a `CREATE TABLE` statement actually USES one:
//! a new table written with no `DEDUP UPSERT KEYS` clause and no
//! `DEDUP ENABLE` re-run compiles, passes every key-content check, and on the
//! first replay appends a second copy of every row. That is the class the
//! 2026-10-26 audit found on `instrument_lifecycle` (whose ensure had lost its
//! production caller, so ILP auto-created it with no key).
//!
//! The scan: every Rust string literal in the production half of each file
//! under `crates/storage/src` and `crates/app/src` that holds a `CREATE TABLE`
//! statement (the words followed by a column list `(`) must either contain
//! `DEDUP UPSERT KEYS` itself, or the same file's production half must hold a
//! literal with `DEDUP ENABLE` that names the same table token (the house
//! self-heal shape). A table with no natural key is listed in
//! [`NO_DEDUP_ALLOWED`] with its dated reason; the list is shrink-only in
//! spirit, and an entry that matches no live literal fails the build.

use std::path::{Path, PathBuf};

/// `(file relative to the workspace root, table token in the CREATE, reason)`.
///
/// The token is the text after `CREATE TABLE [IF NOT EXISTS]` up to the
/// column list, exactly as written in the literal (a `{CONST}` placeholder
/// stays a placeholder).
const NO_DEDUP_ALLOWED: &[(&str, &str, &str)] = &[
    (
        "crates/storage/src/fresh_start_reset.rs",
        "{SCHEMA_RESET_LOG_TABLE}",
        "2026-10-26: one row per one-shot reset id, written once and read only to \
         decide whether that reset already ran; a duplicate row reads the same \
         answer, so a DEDUP key buys nothing",
    ),
    (
        "crates/storage/src/contract_name_repair.rs",
        "{CONTRACT_NAME_REPAIR_MAP_TABLE}",
        "2026-10-26: a scratch map the repair run drops and re-creates fresh each \
         day before filling it; it has no designated timestamp, so QuestDB makes \
         it unpartitioned and non-WAL and cannot hold a DEDUP key on it at all",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Everything above the first column-0 `#[cfg(test)]` (house convention).
fn production_half(src: &str) -> &str {
    src.find("\n#[cfg(test)]").map_or(src, |i| &src[..i])
}

/// The string literals of `src`, comments skipped. Handles `"..."` with
/// escapes, raw `r"..."` / `r#"..."#`, char literals and lifetimes. A `\`
/// line continuation is folded the way the compiler folds it (newline and
/// the following leading whitespace dropped) so a multi-line `format!`
/// statement reads as one statement.
fn string_literals(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'\'' => {
                // Char literal `'x'` / `'\n'` / `'"'`, else a lifetime.
                if bytes.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else if bytes.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            b'r' if matches!(bytes.get(i + 1), Some(b'"') | Some(b'#'))
                && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')) =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while bytes.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if bytes.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let start = j + 1;
                let closing: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let end = src[start..].find(&closing).map_or(src.len(), |k| start + k);
                out.push(src[start..end].to_string());
                i = end + closing.len();
            }
            b'"' => {
                let mut lit = String::new();
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\n') {
                        i += 2;
                        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        continue;
                    }
                    if bytes[i] == b'\\' {
                        lit.push(bytes[i] as char);
                        i += 1;
                    }
                    if i < bytes.len() {
                        // Literals are ASCII DDL in practice; a multi-byte
                        // char is copied byte-wise and never matches a needle.
                        lit.push(bytes[i] as char);
                        i += 1;
                    }
                }
                out.push(lit);
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// The table token of a `CREATE TABLE` statement inside `lit`, or `None`
/// when the literal is not a statement (a log message that names the words).
fn create_table_token(lit: &str) -> Option<String> {
    let at = lit.find("CREATE TABLE ")?;
    let mut rest = lit[at + "CREATE TABLE ".len()..].trim_start();
    if let Some(r) = rest.strip_prefix("IF NOT EXISTS ") {
        rest = r.trim_start();
    }
    let token: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '(')
        .collect();
    let after = rest[token.len()..].trim_start();
    if token.is_empty() || !after.starts_with('(') {
        return None;
    }
    Some(token)
}

/// `(file, token)` for every CREATE literal in `prod` with no key, given the
/// file's literals.
fn unkeyed_creates(prod: &str) -> Vec<String> {
    let lits = string_literals(prod);
    let mut out = Vec::new();
    for lit in &lits {
        let Some(token) = create_table_token(lit) else {
            continue;
        };
        if lit.contains("DEDUP UPSERT KEYS") {
            continue;
        }
        let enabled_elsewhere = lits
            .iter()
            .any(|l| l.contains("DEDUP ENABLE") && l.contains(token.as_str()));
        if !enabled_elsewhere {
            out.push(token);
        }
    }
    out
}

#[test]
fn every_create_table_carries_a_dedup_key() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates/storage/src"), &mut files);
    rust_files(&root.join("crates/app/src"), &mut files);
    assert!(
        files.len() > 50,
        "the scan found only {} files",
        files.len()
    );

    let mut violations = Vec::new();
    let mut allow_hits = vec![false; NO_DEDUP_ALLOWED.len()];
    let mut creates_seen = 0usize;
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .expect("under the root")
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(path).expect("readable source");
        let prod = production_half(&src);
        creates_seen += string_literals(prod)
            .iter()
            .filter(|l| create_table_token(l).is_some())
            .count();
        for token in unkeyed_creates(prod) {
            match NO_DEDUP_ALLOWED
                .iter()
                .position(|(f, t, _)| *f == rel && *t == token)
            {
                Some(idx) => allow_hits[idx] = true,
                None => violations.push(format!("{rel}: CREATE TABLE {token}")),
            }
        }
    }
    assert!(
        creates_seen >= 20,
        "only {creates_seen} CREATE TABLE statements found — the scanner is broken"
    );
    assert!(
        violations.is_empty(),
        "CREATE TABLE with no DEDUP UPSERT KEYS and no DEDUP ENABLE in the same file \
         (a replay or re-run will duplicate every row):\n{}",
        violations.join("\n")
    );
    for (idx, hit) in allow_hits.iter().enumerate() {
        assert!(
            *hit,
            "NO_DEDUP_ALLOWED entry {:?} matches no live CREATE TABLE — remove it",
            NO_DEDUP_ALLOWED[idx]
        );
    }
}

/// The scanner bites: an unkeyed CREATE is reported, a keyed one and the
/// self-heal shape are not, and a log message naming the words is ignored.
#[test]
fn the_scanner_bites() {
    let unkeyed = r#"fn ddl() -> String { format!("CREATE TABLE IF NOT EXISTS {T} (\
            ts TIMESTAMP, \
            v LONG\
        ) timestamp(ts) PARTITION BY DAY WAL") }"#;
    assert_eq!(unkeyed_creates(unkeyed), vec!["{T}".to_string()]);

    let keyed = r#"fn ddl() -> String { format!("CREATE TABLE IF NOT EXISTS {T} (\
            ts TIMESTAMP\
        ) timestamp(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts);") }"#;
    assert!(unkeyed_creates(keyed).is_empty());

    let self_heal = r#"fn a() -> String { format!("CREATE TABLE IF NOT EXISTS {T} (ts TIMESTAMP)") }
        fn b() -> String { format!("ALTER TABLE {T} DEDUP ENABLE UPSERT KEYS(ts)") }"#;
    assert!(unkeyed_creates(self_heal).is_empty());

    let message = r#"fn m() { error!("CREATE TABLE returned non-2xx (status {s})"); }"#;
    assert!(unkeyed_creates(message).is_empty());

    // A commented-out key never counts, and a test module never counts.
    let commented = "fn a() { let _ = \"CREATE TABLE x (ts TIMESTAMP)\"; }\n\
        // \"ALTER TABLE x DEDUP ENABLE UPSERT KEYS(ts)\"\n";
    assert_eq!(unkeyed_creates(commented), vec!["x".to_string()]);
    let in_tests =
        "fn p() {}\n#[cfg(test)]\nmod tests { const D: &str = \"CREATE TABLE y (a INT)\"; }";
    assert!(unkeyed_creates(production_half(in_tests)).is_empty());
}

/// Char literals and lifetimes do not derail the literal scanner.
#[test]
fn the_literal_scanner_survives_chars_and_lifetimes() {
    let src = r#"fn f<'a>(x: &'a str) -> char { let q = '"'; let e = '\''; g("CREATE TABLE z (a INT)"); q }"#;
    let lits = string_literals(src);
    assert_eq!(lits, vec!["CREATE TABLE z (a INT)".to_string()]);
}
