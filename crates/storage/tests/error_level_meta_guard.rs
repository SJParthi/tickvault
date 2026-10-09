//! Rule 5 meta-guard — a write, flush, persist, sync or upload FAILURE must
//! not be logged at `warn!` unless it is a reviewed, recorded exception.
//!
//! ## Why this file was rewritten (2026-10-09)
//!
//! The first version of this guard (2026-04-18) matched a fixed list of 30
//! exact message phrases, and only when the phrase sat on the SAME source line
//! as `warn!(`. By 2026-10-09 every one of those 30 phrases named a writer
//! that had been deleted (the old tick ring, the movers, the candle writer),
//! and every live log macro in the tree spans several lines. So the guard
//! scanned ~700 files and could not fail on anything: a stress audit counted
//! 58 `warn!` sites reporting a failed flush, sync, persist or upload, and the
//! guard passed over all of them. A guard that cannot fail reads exactly like
//! a guard that found nothing.
//!
//! ## What it checks now
//!
//! 1. Every `warn!( … )` and `tracing::warn!( … )` invocation in every
//!    `crates/*/src` file is read WHOLE (balanced parentheses, across lines,
//!    with `\` line continuations inside strings), and its string literals are
//!    joined and lower-cased.
//! 2. A site is a **write-failure warn** when that text names a durability
//!    action ([`DURABILITY_WORDS`]) AND a failure ([`FAILURE_WORDS`]).
//! 3. Every write-failure warn must be either
//!    - preceded by a `// APPROVED: <reason>` line, or
//!    - matched by an entry in [`REVIEWED_WARN_SITES`] (file + a substring of
//!      the message), each with the reason it stays a warning.
//! 4. Every entry in [`REVIEWED_WARN_SITES`] must still match a live site, so
//!    the list can only shrink: converting a site to `error!` forces its entry
//!    out, and a new write-failure `warn!` fails the build until someone
//!    either makes it `error!` or records why not.
//! 5. Bite tests feed the scanner fixtures that must and must not be flagged,
//!    so the scanner itself cannot silently stop matching.
//!
//! ## Honest limits
//!
//! - It reads string literals only. A message built entirely from a variable
//!   is invisible to it.
//! - The word lists are heuristics. They catch the shape "X failed / could not
//!   X"; a failure worded some other way is not caught.
//! - Code after a `#[cfg(test)] mod` line is skipped (test fixtures log
//!   failures on purpose).

use std::fs;
use std::path::{Path, PathBuf};

/// Words that mean "this was about making data durable or moving it on".
const DURABILITY_WORDS: &[&str] = &[
    "flush", "persist", "sync", "spill", "write", "written", "append", "upload", "drain", "save",
    "rename",
];

/// Words that mean "and it did not happen".
const FAILURE_WORDS: &[&str] = &["fail", "could not", "cannot", "couldn't", "not be"];

/// Reviewed write-failure `warn!` sites: `(file under the workspace root, a
/// lower-cased substring of the joined message, why it stays a warning)`.
///
/// SHRINK ONLY. An entry that no longer matches a live site fails the build,
/// so converting a site to `error!` must remove its entry in the same change.
/// Adding an entry is a review decision: write the reason a reader would need
/// to agree the failure does not lose data or is already paged elsewhere.
const REVIEWED_WARN_SITES: &[(&str, &str, &str)] = &[
    // ---- app ----
    (
        "crates/app/src/depth_subscription_view.rs",
        "could not save the depth held-today set",
        "latched once per process; only a same-day restart loses the held-before list, no market data is lost",
    ),
    (
        "crates/app/src/dhan_rest_stack.rs",
        "order-update audit row dropped",
        "deliberate per the site: the health verdict is set before the write; counted on tv_order_update_ws_audit_dropped_total",
    ),
    (
        "crates/app/src/dhan_live_crossverify_boot.rs",
        "could not save the note that today was paged",
        "worst case is one duplicate page on a later failure, never a missed one",
    ),
    (
        "crates/app/src/dhan_live_crossverify_boot.rs",
        "its folder could not be synced to disk",
        "the marker file itself was written; only a host crash in the next moments can lose it, and the day then re-runs",
    ),
    (
        "crates/app/src/dhan_live_crossverify_boot.rs",
        "but the day marker could not be saved",
        "retried marker-only and paged on xverify_failed after the last attempt (no-rest rule section 12.15.7)",
    ),
    (
        "crates/app/src/dhan_live_crossverify_boot.rs",
        "stopped saving its audit rows",
        "a deliberate stop at the 17:23 deadline (section 12.15.8); real row losses are counted by the writer and paged on audit_rows",
    ),
    (
        "crates/app/src/depth_unsubscribe_probe.rs",
        "could not write the probe's day-latch",
        "the probe then runs at most once per process instead of once per day; no data is lost",
    ),
    (
        "crates/app/src/main_feed_backup.rs",
        "the saved publication history could not be read",
        "replay then keeps both copies of a backup packet: a duplicate row, never a lost one",
    ),
    (
        "crates/app/src/dhan_feed_stack/deferred_depth_pass.rs",
        "could not finish a bucket",
        "the bucket stays marked and is retried by the next pass; its WAL segments are kept",
    ),
    (
        "crates/app/src/tf_consistency_boot.rs",
        "today's delivered marker",
        "worst case is one repeated daily report card after a restart",
    ),
    (
        "crates/app/src/main.rs",
        "errors.jsonl appender init failed",
        "boot continues with the other log sinks; the JSONL sink is the one that failed, so error! has nowhere new to go",
    ),
    (
        "crates/app/src/main.rs",
        "seal writer loop already installed",
        "an idempotent skip, not a failure: the first installer keeps running",
    ),
    (
        "crates/app/src/dhan_feed_stack.rs",
        "the ilp buffer refused snapshot rows",
        "derived top_volume table only; ranking runs from RAM; counted and power-of-two logged",
    ),
    (
        "crates/app/src/dhan_feed_stack.rs",
        "the saved main-feed backup set could not be read",
        "replay keeps both copies of a backup packet: a duplicate row, never a lost one",
    ),
    (
        "crates/app/src/dhan_feed_stack.rs",
        "open bar(s) were not written",
        "by design (PR31b-2): a bar cut short by a mid-session exit is withheld, counted on tv_candle_refold_partial_suppressed_total",
    ),
    (
        "crates/app/src/dhan_feed_stack.rs",
        "carried a vendor stamp for another trading day",
        "an input refusal, counted; not a write failure (matched only because the text says written)",
    ),
    (
        "crates/app/src/dhan_feed_stack.rs",
        "the set could not be saved for the crash replay",
        "replay after a crash then keeps both copies of a backup packet: duplicates, never loss",
    ),
    (
        "crates/app/src/dhan_feed_stack.rs",
        "the save task failed",
        "same as above: the backup set is a dedup aid; losing it duplicates rows, never drops them",
    ),
    (
        "crates/app/src/index_constituency_boot.rs",
        "questdb not ready within the quiet probe bound",
        "the migration is attempted anyway and repeats next boot; a boot-time wait, not a write failure",
    ),
    (
        "crates/app/src/depth_rebalance.rs",
        "could not write tomorrow's dial seed",
        "the next boot falls back to the index at-the-money dial; no data is lost",
    ),
    (
        "crates/app/src/daily_archive_boot.rs",
        "only its latch marker was not saved",
        "the archive itself completed; the next run re-checks and finds it done",
    ),
    // ---- aws-lambdas ----
    (
        "crates/aws-lambdas/src/hard_stop_guard.rs",
        "ping-state write failed",
        "worst case a duplicate hourly ping; the guard prefers a duplicate over silence",
    ),
    // ---- core ----
    (
        "crates/core/src/auth/token_cache.rs",
        "failed to write token cache temp file",
        "a cache only: the live token stays in memory and in SSM",
    ),
    (
        "crates/core/src/auth/token_cache.rs",
        "failed to rename token cache file",
        "a cache only: the live token stays in memory and in SSM",
    ),
    (
        "crates/core/src/websocket/pool_supervisor.rs",
        "the repeat unsubscribe for a ghost contract did not reach",
        "nothing is lost: the resend is retried after its cooldown and the extra packets are kept",
    ),
    (
        "crates/core/src/websocket/connection.rs",
        "batch could not be written to the socket",
        "returns SocketFailure, so the connection redials and replays the full set; counted",
    ),
    (
        "crates/core/src/websocket/connection.rs",
        "could not be queued for the socket writer",
        "nothing reached the wire; counted on the subscribe-failed counter and coded WS-GAP-02",
    ),
    // ---- storage ----
    (
        "crates/storage/src/tick_persistence.rs",
        "so the tick spill ceiling falls back",
        "a sizing probe; the spill keeps working at its fixed floor",
    ),
    (
        "crates/storage/src/depth_persistence.rs",
        "so the depth spill ceiling falls back",
        "a sizing probe; the spill keeps working at its fixed floor",
    ),
    (
        "crates/storage/src/wal_deferred_depth.rs",
        "deferred-depth marks could not be persisted",
        "coded WS-SPILL-01 and latched once per episode; paged by durability-sync-failing on its counter",
    ),
    (
        "crates/storage/src/wal_applied_watermark.rs",
        "applied-watermark could not be persisted",
        "coded WS-SPILL-01 and latched once per episode; paged by durability-sync-failing; fails toward replaying more",
    ),
    (
        "crates/storage/src/resource_monitor.rs",
        "the probe task failed",
        "a monitoring probe, not a write; the next cycle probes again",
    ),
    (
        "crates/storage/src/resource_monitor.rs",
        "spill-dir free-space probe failed",
        "a monitoring probe, not a write; the next cycle probes again",
    ),
    (
        "crates/storage/src/seal_spill.rs",
        "cannot be cut back; set aside",
        "the damaged file is kept and appending moves to a fresh file; nothing is deleted",
    ),
    (
        "crates/storage/src/seal_writer_task.rs",
        "cannot open staged spill file",
        "the caller refuses to archive a file it could not open; it is retried next scan",
    ),
    (
        "crates/storage/src/seal_writer_task.rs",
        "archive rename failed",
        "rows are already committed and the file is kept; DEDUP collapses a re-read",
    ),
    (
        "crates/storage/src/seal_writer_task.rs",
        "seal recovery skipped",
        "the undecodable seals are paged through AGGREGATOR-DROP-01 source=seal_unrecovered",
    ),
    (
        "crates/storage/src/seal_writer_task.rs",
        "cannot stage a spill file",
        "the file stays where it is and is retried on the next scan",
    ),
    (
        "crates/storage/src/ws_frame_spill.rs",
        "could not clone the new segment's file handle",
        "not a loss: the writer syncs that segment inline instead",
    ),
    (
        "crates/storage/src/ws_frame_spill.rs",
        "wal segment sync failed",
        "paged by durability-sync-failing on tv_wal_fsync_errors_total; warn level is a documented decision at the site",
    ),
    (
        "crates/storage/src/ws_frame_spill.rs",
        "wal sync failed while finalising a segment",
        "paged by durability-sync-failing on tv_wal_fsync_errors_total; the records reached the kernel",
    ),
    (
        "crates/storage/src/shadow_persistence.rs",
        "failed to write legacy-drop marker",
        "a one-time cleanup marker; the cleanup repeats next boot",
    ),
    (
        "crates/storage/src/shadow_persistence.rs",
        "failed to write the retired-candle marker",
        "a one-time sweep marker; the sweep repeats next boot",
    ),
    (
        "crates/storage/src/partition_archive.rs",
        "archive-audit failure-row write failed",
        "a best-effort row about a failure already logged; the partition itself is kept",
    ),
    (
        "crates/storage/src/order_leg_pnl_persistence.rs",
        "ilp sender build failed",
        "rows go to the disk spill at flush and are replayed; a spill refusal is logged at error",
    ),
    (
        "crates/storage/src/index_constituency_persistence.rs",
        "failed to write marker",
        "a migration marker; the migration repeats next boot",
    ),
    (
        "crates/storage/src/seal_writer_loop.rs",
        "the unwritten-seal marker from the previous process could not",
        "a read failure of a report marker, matched only because the text says unwritten",
    ),
    (
        "crates/storage/src/tick_spill_replay.rs",
        "could not read a spill file",
        "the file is kept and retried next round",
    ),
    (
        "crates/storage/src/tick_spill_replay.rs",
        "could not seek to the resume offset",
        "the file is kept and retried next round",
    ),
    (
        "crates/storage/src/tick_spill_replay.rs",
        "could not be written to quarantine",
        "the file is kept intact and retried next round",
    ),
    (
        "crates/storage/src/tick_spill_replay.rs",
        "could not be moved to quarantine",
        "the file is left in place and retried next round",
    ),
    (
        "crates/storage/src/tick_spill_replay.rs",
        "could not reach questdb",
        "the file is kept intact and retried next round",
    ),
    (
        "crates/storage/src/tick_spill_replay.rs",
        "could not re-check a drained spill file",
        "the file is not truncated, so no bytes can be lost; retried next round",
    ),
    (
        "crates/storage/src/seal_absorption.rs",
        "tier-2 spill failed",
        "escalates to the tier-3 dead-letter queue; if that fails too the caller fires AGGREGATOR-DROP-01",
    ),
];

#[test]
fn write_failure_warns_are_errors_or_reviewed() {
    let root = workspace_root();
    let mut scanned_files: usize = 0;
    let mut sites: Vec<WarnSite> = Vec::new();
    for src_dir in crate_src_dirs(&root) {
        collect_sites_recursive(&root, &src_dir, &mut sites, &mut scanned_files);
    }

    // Anti-vacuity, two ways: enough files read, and enough warn! sites found
    // in them. Measured 2026-10-09: ~330 src files, several hundred warn!s.
    assert!(
        scanned_files > 200,
        "error-level guard read only {scanned_files} files; expected > 200"
    );
    assert!(
        sites.len() > 200,
        "error-level guard found only {} warn! sites; the extractor has \
         probably stopped matching",
        sites.len()
    );

    let flagged: Vec<&WarnSite> = sites.iter().filter(|s| is_write_failure(&s.text)).collect();

    let mut unreviewed: Vec<String> = Vec::new();
    for site in &flagged {
        if site.approved {
            continue;
        }
        let reviewed = REVIEWED_WARN_SITES
            .iter()
            .any(|(file, needle, _)| site.file == *file && site.text.contains(needle));
        if !reviewed {
            unreviewed.push(format!(
                "  {}:{} | {}",
                site.file,
                site.line,
                excerpt(&site.text)
            ));
        }
    }

    let mut stale: Vec<String> = Vec::new();
    for (file, needle, _) in REVIEWED_WARN_SITES {
        let live = flagged
            .iter()
            .any(|s| s.file == *file && s.text.contains(needle));
        if !live {
            stale.push(format!("  ({file:?}, {needle:?})"));
        }
    }

    assert!(
        unreviewed.is_empty(),
        "Rule 5: these warn! sites report a failed write, flush, persist, \
         sync or upload. Make each one `error!` with `code = \
         ErrorCode::X.code_str()`, or add it to REVIEWED_WARN_SITES with the \
         reason it stays a warning:\n{}",
        unreviewed.join("\n")
    );
    assert!(
        stale.is_empty(),
        "REVIEWED_WARN_SITES entries that no longer match any write-failure \
         warn! (the site was fixed, moved or reworded). Remove them:\n{}",
        stale.join("\n")
    );
}

#[test]
fn reviewed_list_is_lowercase_and_reasoned() {
    for (file, needle, reason) in REVIEWED_WARN_SITES {
        assert!(
            file.starts_with("crates/"),
            "{file}: path must start at crates/"
        );
        assert_eq!(
            *needle,
            needle.to_lowercase(),
            "{file}: needle must be lower-case"
        );
        assert!(
            needle.len() >= 12,
            "{file}: needle {needle:?} is too short to be specific"
        );
        assert!(
            reason.len() >= 20,
            "{file}: reason {reason:?} is too short to review"
        );
    }
}

#[test]
fn bite_two_line_warn_on_a_failed_flush_is_flagged() {
    let src =
        "fn f() {\n    warn!(\n        error = %e,\n        \"wal flush failed\"\n    );\n}\n";
    let sites = extract_warn_sites("x.rs", src);
    assert_eq!(sites.len(), 1);
    assert!(is_write_failure(&sites[0].text));
    assert!(!sites[0].approved);
}

#[test]
fn bite_continued_string_and_tracing_path_are_read() {
    let src = "fn f() {\n    tracing::warn!(\n        \"the marker could not \\\n         be persisted\"\n    );\n}\n";
    let sites = extract_warn_sites("x.rs", src);
    assert_eq!(sites.len(), 1);
    assert!(
        sites[0].text.contains("could not be persisted"),
        "{:?}",
        sites[0].text
    );
    assert!(is_write_failure(&sites[0].text));
}

#[test]
fn bite_error_macro_and_benign_warn_are_not_flagged() {
    let src = "fn f() {\n    error!(\"wal flush failed\");\n    warn!(\"spot price is stale\");\n    warn!(\"flush took 3 s\");\n}\n";
    let sites = extract_warn_sites("x.rs", src);
    assert_eq!(
        sites.len(),
        2,
        "error! must not be collected as a warn! site"
    );
    assert!(sites.iter().all(|s| !is_write_failure(&s.text)));
}

#[test]
fn bite_approved_comment_and_test_module_are_honoured() {
    let src = "fn f() {\n    // APPROVED: probe write, nothing is lost\n    warn!(\"probe write failed\");\n}\n#[cfg(test)]\nmod tests {\n    fn t() { warn!(\"flush failed\"); }\n}\n";
    let sites = extract_warn_sites("x.rs", src);
    assert_eq!(sites.len(), 1, "the test-module warn! must be skipped");
    assert!(sites[0].approved);
}

#[test]
fn bite_commented_out_warn_is_ignored() {
    let src = "fn f() {\n    // warn!(\"flush failed\");\n}\n";
    assert!(extract_warn_sites("x.rs", src).is_empty());
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

struct WarnSite {
    file: String,
    line: usize,
    /// String literals of the macro body, joined with spaces, lower-cased.
    text: String,
    /// `// APPROVED:` on the line before the macro.
    approved: bool,
}

fn is_write_failure(text: &str) -> bool {
    DURABILITY_WORDS.iter().any(|w| text.contains(w))
        && FAILURE_WORDS.iter().any(|w| text.contains(w))
}

fn excerpt(text: &str) -> String {
    text.chars().take(140).collect()
}

/// Byte offset where a `#[cfg(test)]` attribute is followed by a `mod` line.
/// Everything after it is test code.
fn test_module_start(src: &str) -> usize {
    let mut offset = 0;
    let mut lines = src.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines.peek().map(|l| l.trim_start()).unwrap_or("");
            if next.starts_with("mod ") || next.starts_with("pub mod ") {
                return offset;
            }
        }
        offset += line.len();
    }
    src.len()
}

fn extract_warn_sites(file: &str, src: &str) -> Vec<WarnSite> {
    const PAT: &[u8] = b"warn!(";
    let end_of_prod = test_module_start(src);
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + PAT.len() <= end_of_prod {
        if &bytes[i..i + PAT.len()] != PAT {
            i += 1;
            continue;
        }
        let prev = if i == 0 { b' ' } else { bytes[i - 1] };
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            i += 1;
            continue;
        }
        let line_start = src[..i].rfind('\n').map_or(0, |p| p + 1);
        if src[line_start..i].trim_start().starts_with("//") {
            i += PAT.len();
            continue;
        }
        let line = src[..i].matches('\n').count() + 1;
        let (text, end) = read_macro_strings(bytes, i + PAT.len());
        let approved = src[..line_start]
            .trim_end_matches('\n')
            .rsplit('\n')
            .next()
            .is_some_and(|l| l.trim_start().starts_with("// APPROVED:"));
        out.push(WarnSite {
            file: file.to_string(),
            line,
            text: text.to_lowercase(),
            approved,
        });
        i = end;
    }
    out
}

/// Reads from just after `warn!(` to the matching `)`, returning the string
/// literals joined by spaces and the index after the closing parenthesis.
fn read_macro_strings(bytes: &[u8], start: usize) -> (String, usize) {
    let mut depth = 1usize;
    let mut j = start;
    let mut text: Vec<u8> = Vec::new();
    let mut in_str = false;
    while j < bytes.len() && depth > 0 {
        let c = bytes[j];
        if in_str {
            match c {
                b'\\' if bytes.get(j + 1) == Some(&b'\n') => {
                    // Line continuation: skip the newline and the indent.
                    j += 2;
                    while bytes.get(j).is_some_and(|b| *b == b' ' || *b == b'\t') {
                        j += 1;
                    }
                    continue;
                }
                b'\\' => {
                    if let Some(next) = bytes.get(j + 1) {
                        text.push(*next);
                    }
                    j += 2;
                    continue;
                }
                b'"' => {
                    in_str = false;
                    text.push(b' ');
                }
                _ => text.push(c),
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
        }
        j += 1;
    }
    // Bytes, not chars: a multi-byte character (an em dash) must survive.
    (String::from_utf8_lossy(&text).into_owned(), j)
}

fn collect_sites_recursive(
    root: &Path,
    dir: &Path,
    sites: &mut Vec<WarnSite>,
    scanned: &mut usize,
) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("error-level guard corpus unreadable {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sites_recursive(root, &path, sites, scanned);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("error-level guard cannot read {path:?}: {e}"));
        *scanned = scanned.saturating_add(1);
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        sites.extend(extract_warn_sites(&rel, &contents));
    }
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        // Fail loudly: a silent fallback once rooted the scan at crates/storage
        // and passed having scanned nothing.
        .expect("workspace root must exist above crates/storage")
}

/// Every `crates/*/src` directory, discovered from the filesystem so a crate
/// cannot be left out by forgetting to list it (a hardcoded list once skipped
/// two of eight crates).
fn crate_src_dirs(root: &Path) -> Vec<PathBuf> {
    let crates_dir = root.join("crates");
    let mut out: Vec<PathBuf> = fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("cannot read {crates_dir:?}: {e}"))
        .filter_map(|entry| {
            let src = entry.ok()?.path().join("src");
            src.is_dir().then_some(src)
        })
        .collect();
    out.sort();
    assert!(
        out.len() >= 8,
        "crate discovery found only {} src dir(s) under {crates_dir:?}",
        out.len()
    );
    out
}
