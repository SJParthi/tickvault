//! SOURCE SIZE RATCHET (2026-10-12, deep audit INC-1).
//!
//! `dhan_feed_stack.rs` (33,753 lines) and `pool_supervisor.rs` (26,621) each
//! hold several subsystems, and `main.rs` holds the whole boot. Splitting them
//! is several PRs, because many source-scan guards read function bodies by
//! name and must be re-pointed with each move. Until that is done, these files
//! must at least stop growing.
//!
//! `oversized_source_files_may_only_shrink` pins each listed file at its line
//! count when this guard was written plus `SLACK` (200 lines), and fails when
//! a file grows past its pin. It also fails when any other production source
//! file passes `NEW_FILE_CEILING` (10,000 lines) without a row here.
//!
//! The pin is LOWERED in the same PR that extracts a module out of a file
//! (first candidate: the top-volume sweep out of `dhan_feed_stack.rs`). A pin
//! may never be raised without a reason recorded on its row.
//!
//! HONEST LIMIT: line count is a proxy for size, not for coupling; a PR can
//! keep a file under its pin and still add a subsystem to it.

use std::path::{Path, PathBuf};

/// Lines of headroom over the measured size, so small fixes still land.
const SLACK: usize = 200;

/// A production source file over this many lines needs a row in `PINS`.
const NEW_FILE_CEILING: usize = 10_000;

/// (file, lines) measured 2026-10-12 with `wc -l` on this branch, after the
/// DA-12 batch merged into it. The ceiling is `measured + SLACK`.
/// LOWER a row when a module is extracted; never raise one silently.
const PINS: &[(&str, usize)] = &[
    // 2026-10-14 (ITEM 57): lowered 14 (the file is 14 lines below main) after
    // `contract_attach_tests` moved out and ITEM 57 code moved in.
    ("crates/app/src/dhan_feed_stack.rs", 33_752),
    ("crates/app/src/main.rs", 7_002),
    ("crates/core/src/websocket/pool_supervisor.rs", 26_621),
    ("crates/storage/src/ws_frame_spill.rs", 13_467),
    ("crates/trading/src/candles/multi_tf_aggregator.rs", 12_755),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("source_size_ratchet_guard: cannot canonicalize repo root")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// `(file, lines, ceiling)` for every file over its ceiling: a pinned file
/// over `measured + SLACK`, or an unpinned one over `NEW_FILE_CEILING`.
fn oversize(sizes: &[(String, usize)], pins: &[(&str, usize)]) -> Vec<(String, usize, usize)> {
    sizes
        .iter()
        .filter_map(|(f, n)| {
            let ceiling = pins
                .iter()
                .find(|(p, _)| p == f)
                .map_or(NEW_FILE_CEILING, |(_, m)| m + SLACK);
            (*n > ceiling).then(|| (f.clone(), *n, ceiling))
        })
        .collect()
}

#[test]
fn oversized_source_files_may_only_shrink() {
    let names: Vec<&str> = PINS.iter().map(|r| r.0).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "PINS must be sorted and unique");

    let root = repo_root();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates dir")
        .flatten()
    {
        collect_rs(&entry.path().join("src"), &mut files);
    }
    assert!(files.len() > 200, "only {} files walked", files.len());
    let sizes: Vec<(String, usize)> = files
        .iter()
        .map(|f| {
            let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
            let n = std::fs::read_to_string(f)
                .unwrap_or_default()
                .lines()
                .count();
            (rel, n)
        })
        .collect();

    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !sizes.iter().any(|(f, _)| f == n))
        .collect();
    assert!(
        missing.is_empty(),
        "pinned files not found (moved or deleted?): {missing:?}. Update PINS."
    );

    let over = oversize(&sizes, PINS);
    assert!(
        over.is_empty(),
        "source files grew past their size pin: {over:?} (file, lines, ceiling). \
         These files are scheduled to be split; move the new code into a module of \
         its own instead of growing them."
    );
}

#[test]
fn source_size_detector_bites() {
    let sizes = vec![
        ("big.rs".to_string(), 1_201),
        ("ok.rs".to_string(), 1_200),
        ("new.rs".to_string(), NEW_FILE_CEILING + 1),
        ("small.rs".to_string(), NEW_FILE_CEILING),
    ];
    let pins = [("big.rs", 1_000), ("ok.rs", 1_000)];
    let got: Vec<String> = oversize(&sizes, &pins).into_iter().map(|r| r.0).collect();
    assert_eq!(got, vec!["big.rs".to_string(), "new.rs".to_string()]);
}
