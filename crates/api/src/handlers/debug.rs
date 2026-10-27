//! Debug endpoints for Claude MCP / autonomous-ops observability.
//!
//! Read-only HTTP access to tickvault's error-log artefacts so that
//! remote Claude sessions (sandbox, cloud IDE, claude.ai) can reach
//! them over a Tailscale Funnel or reverse proxy without needing
//! filesystem sync. This is Layer 1 (Observability) of the autonomous
//! operations plan at `.claude/plans/autonomous-operations-100pct.md`.
//!
//! These endpoints expose the same files the MCP server reads locally:
//! - `errors.summary.md` (60s-refreshed signature-hash summary)
//! - `errors.jsonl.<YYYY-MM-DD-HH>` (hourly-rotated ERROR JSONL)
//!
//! They do NOT expose app.log, secrets, or any write action.

use std::path::{Path, PathBuf};

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

/// Default logs directory. Mirrors the MCP server's default resolution:
/// `<repo_root>/data/logs/machine` (2026-07-05 operator directive — every
/// machine sink lives under `data/logs/machine/`; the `data/logs/` top
/// level is the human log surface). Overridable via env var `TV_LOGS_DIR`
/// so deployments that store logs elsewhere (e.g. EFS mount on AWS) work
/// without code changes.
const DEFAULT_LOGS_DIR: &str = "data/logs/machine";
const ERRORS_SUMMARY_FILENAME: &str = "errors.summary.md";
const ERRORS_JSONL_PREFIX: &str = "errors.jsonl";

/// Env-var override for the logs directory location.
const LOGS_DIR_ENV: &str = "TV_LOGS_DIR";

/// Default spill directory. Matches `TICK_SPILL_DIR` in
/// `crates/storage/src/tick_persistence.rs`. Read-only diagnostics
/// only — never deletes or replays from this dir.
const DEFAULT_SPILL_DIR: &str = "data/spill";
const SPILL_DIR_ENV: &str = "TV_SPILL_DIR";

fn resolve_logs_dir() -> PathBuf {
    if let Ok(custom) = std::env::var(LOGS_DIR_ENV)
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    PathBuf::from(DEFAULT_LOGS_DIR)
}

/// GET /api/debug/logs/summary — returns errors.summary.md as text/markdown.
/// Returns 404 with `{"error": "summary not yet generated"}` if the file
/// does not exist. Never panics on missing files.
pub async fn logs_summary() -> impl IntoResponse {
    let path = resolve_logs_dir().join(ERRORS_SUMMARY_FILENAME);
    read_text_file(&path, "text/markdown; charset=utf-8").await
}

/// GET /api/debug/logs/jsonl/latest — returns the newest errors.jsonl.*
/// file as application/x-ndjson. Lexical sort is sufficient because the
/// filenames embed ISO-8601 YYYY-MM-DD-HH suffixes.
/// Returns 404 if no errors.jsonl.* file exists yet.
///
/// Bounded read (audit J4, 2026-10-26): the hourly file has no byte cap, so
/// an error storm can make it gigabytes long. A file larger than
/// [`MAX_JSONL_RESPONSE_BYTES`] is served as its last 10 MiB, starting at a
/// line boundary, with the header [`TRUNCATED_HEADER`] set to `true` and a
/// first NDJSON line `{"truncated":true,"file_bytes":N,"served_bytes":M}`.
pub async fn logs_jsonl_latest() -> axum::response::Response {
    let dir = resolve_logs_dir();
    let Some(path) = on_blocking_pool(move || newest_jsonl(&dir)).await.flatten() else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            "{\"error\":\"no errors.jsonl file present\"}".to_string(),
        )
            .into_response();
    };
    let read_path = path.clone();
    match on_blocking_pool(move || read_tail(&read_path, MAX_JSONL_RESPONSE_BYTES)).await {
        Some(Ok(tail)) => {
            let truncated = if tail.truncated { "true" } else { "false" };
            let body = if tail.truncated {
                format!(
                    "{{\"truncated\":true,\"file_bytes\":{},\"served_bytes\":{}}}\n{}",
                    tail.file_bytes,
                    tail.body.len(),
                    tail.body
                )
            } else {
                tail.body
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8"),
                    (header::HeaderName::from_static(TRUNCATED_HEADER), truncated),
                ],
                body,
            )
                .into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            format!(
                "{{\"error\":\"file not found\",\"path\":\"{}\"}}",
                json_escape(&path.display().to_string())
            ),
        )
            .into_response(),
    }
}

/// Largest JSONL body the debug endpoint serves (the same 10 MiB as
/// [`MAX_CROSS_VERIFY_CSV_BYTES`]).
const MAX_JSONL_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

/// Response header naming whether the JSONL body is a tail of a larger file.
const TRUNCATED_HEADER: &str = "x-tickvault-truncated";

/// The last bytes of a file, as read by [`read_tail`].
#[derive(Debug)]
struct FileTail {
    body: String,
    file_bytes: u64,
    truncated: bool,
}

/// Reads at most `cap` bytes from the end of `path`. When the file is larger,
/// the reader seeks to `len - cap` and drops everything up to the first
/// newline, so the body starts on a whole line. Invalid UTF-8 is replaced,
/// never refused. Blocking: run it on the blocking pool. Memory is at most
/// `cap` bytes whatever the file size.
fn read_tail(path: &Path, cap: u64) -> std::io::Result<FileTail> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let file_bytes = file.metadata()?.len();
    let start = file_bytes.saturating_sub(cap);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut raw = Vec::with_capacity(usize::try_from(file_bytes - start).unwrap_or(0));
    file.take(cap).read_to_end(&mut raw)?;
    let truncated = start > 0;
    let from = if truncated {
        raw.iter()
            .position(|b| *b == b'\n')
            .map_or(raw.len(), |i| i + 1)
    } else {
        0
    };
    Ok(FileTail {
        body: String::from_utf8_lossy(&raw[from..]).into_owned(),
        file_bytes,
        truncated,
    })
}

fn resolve_spill_dir() -> PathBuf {
    if let Ok(custom) = std::env::var(SPILL_DIR_ENV)
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    PathBuf::from(DEFAULT_SPILL_DIR)
}

/// GET /api/debug/spill/status — read-only spill-directory diagnostics.
///
/// Returns JSON of the form:
/// ```json
/// {
///   "spill_dir": "data/spill",
///   "exists": true,
///   "categories": {
///     "ticks": {"file_count": 0, "total_bytes": 0, "newest_file": null},
///     "candles": {"file_count": 0, "total_bytes": 0, "newest_file": null},
///     "depth":   {"file_count": 0, "total_bytes": 0, "newest_file": null}
///   }
/// }
/// ```
///
/// Lets Claude / operator query "is anything spilled right now?" via MCP
/// without filesystem access. Strictly read-only — never deletes or
/// drains. The `auto-fix-drain-spill.sh` script (M3) consumes this
/// endpoint as its pre-flight check.
pub async fn spill_status() -> impl IntoResponse {
    // The scan stats every spill file, so it runs on the blocking pool and
    // never on a runtime worker (O(1) sweep S4, 2026-10-03).
    let body = on_blocking_pool(spill_status_body)
        .await
        .unwrap_or_else(|| "{\"error\":\"spill scan failed\"}".to_string());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
}

/// Builds the `spill_status` JSON body. Blocking: one `read_dir` and one
/// `stat` per spill file, so callers run it on the blocking pool.
fn spill_status_body() -> String {
    let dir = resolve_spill_dir();
    let exists = dir.is_dir();

    let mut categories = String::from("{");
    let mut first = true;
    // `top_volume` added 2026-10-13 (top_volume spill plan item 7).
    for kind in ["ticks", "candles", "depth", "top_volume"] {
        let cat_dir = dir.join(kind);
        let (count, bytes, newest) = scan_spill_category(&cat_dir);
        if !first {
            categories.push(',');
        }
        first = false;
        let newest_field = match newest {
            Some(name) => format!("\"{}\"", json_escape(&name)),
            None => "null".to_string(),
        };
        categories.push_str(&format!(
            "\"{kind}\":{{\"file_count\":{count},\"total_bytes\":{bytes},\"newest_file\":{newest_field}}}"
        ));
    }
    categories.push('}');

    // Also scan the top-level spill dir for legacy ticks-YYYYMMDD.bin
    // files that pre-date the per-category subdirs.
    let (legacy_count, legacy_bytes, legacy_newest) = scan_legacy_spill(&dir);
    let legacy_newest_field = match legacy_newest {
        Some(name) => format!("\"{}\"", json_escape(&name)),
        None => "null".to_string(),
    };

    let body = format!(
        "{{\"spill_dir\":\"{}\",\"exists\":{},\"categories\":{},\"legacy_top_level\":{{\"file_count\":{legacy_count},\"total_bytes\":{legacy_bytes},\"newest_file\":{legacy_newest_field}}}}}",
        json_escape(&dir.display().to_string()),
        exists,
        categories
    );
    body
}

fn scan_spill_category(dir: &Path) -> (u64, u64, Option<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0, None);
    };
    let mut count: u64 = 0;
    let mut bytes: u64 = 0;
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // `.ilp` counted since 2026-10-13: the tick, depth and top_volume
        // spill files are ILP text, so this scan read 0 for all three.
        if !(name.ends_with(".bin") || name.ends_with(".ilp")) {
            continue;
        }
        count += 1;
        bytes += meta.len();
        if let Ok(mtime) = meta.modified() {
            match newest {
                Some((existing, _)) if mtime <= existing => {}
                _ => newest = Some((mtime, name)),
            }
        }
    }
    (count, bytes, newest.map(|(_, n)| n))
}

fn scan_legacy_spill(dir: &Path) -> (u64, u64, Option<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0, None);
    };
    let mut count: u64 = 0;
    let mut bytes: u64 = 0;
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Legacy pattern: ticks-YYYYMMDD.bin or candles-*.bin at top level.
        if !name.ends_with(".bin") {
            continue;
        }
        count += 1;
        bytes += meta.len();
        if let Ok(mtime) = meta.modified() {
            match newest {
                Some((existing, _)) if mtime <= existing => {}
                _ => newest = Some((mtime, name)),
            }
        }
    }
    (count, bytes, newest.map(|(_, n)| n))
}

/// Default cross-verify artefact directory. The PRODUCER (the 15:31 IST
/// post-market run, `cross_verify_1m_boot.rs`) was RETIRED with the Dhan
/// live WS and deleted in PR-C3 (2026-07-14) — this endpoint keeps serving
/// the RETAINED forensic artifacts (`cross-verify-1m-YYYY-MM-DD.csv` + the
/// sibling `.summary.json`) read-only; no new files are written.
const DEFAULT_CROSS_VERIFY_DIR: &str = "data/cross-verify";
const CROSS_VERIFY_DIR_ENV: &str = "TV_CROSS_VERIFY_DIR";
const CROSS_VERIFY_CSV_PREFIX: &str = "cross-verify-1m-";
const CROSS_VERIFY_CSV_SUFFIX: &str = ".csv";
const CROSS_VERIFY_SUMMARY_SUFFIX: &str = ".summary.json";
/// RAM cap for serving the mismatch CSV inline. A normal day's file is a
/// few KB (mismatch rows only); beyond this cap the endpoint serves the
/// summary with `csv: null` instead of slurping the file into memory.
const MAX_CROSS_VERIFY_CSV_BYTES: u64 = 10 * 1024 * 1024;

fn resolve_cross_verify_dir() -> PathBuf {
    if let Ok(custom) = std::env::var(CROSS_VERIFY_DIR_ENV)
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    PathBuf::from(DEFAULT_CROSS_VERIFY_DIR)
}

/// Newest per-day mismatch CSV by lexical order (filenames embed
/// ISO-8601 dates). STRICT filename predicate — both the
/// `cross-verify-1m-` prefix AND the `.csv` suffix are required, so the
/// sibling `.summary.json` (which shares the prefix and sorts AFTER the
/// `.csv` for the same date) and any foreign/symlinked file are never
/// selected (2026-06-10 pre-impl security review).
fn newest_cross_verify_csv(dir: &Path) -> Option<PathBuf> {
    // One pass keeping the greatest name: O(entries), no list, no sort.
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(CROSS_VERIFY_CSV_PREFIX) && n.ends_with(CROSS_VERIFY_CSV_SUFFIX)
            })
        })
        .max()
}

/// GET /api/debug/cross-verify/latest — the latest post-market 1-minute
/// cross-verification artefacts (visibility directive 2026-06-10).
///
/// Returns JSON:
/// ```json
/// {
///   "date": "2026-06-10",
///   "csv_file": "cross-verify-1m-2026-06-10.csv",
///   "mismatch_rows": 0,
///   "summary": { "compared": 91230, "mismatches": 0, ... } | null,
///   "csv": "run_ts,trading_date,...\n"
/// }
/// ```
///
/// `summary` is the parsed sibling `.summary.json` (null for runs that
/// pre-date the summary artefact). 404 with a GENERIC error body when no
/// run has happened yet — the resolved filesystem path is deliberately
/// NOT disclosed. Read-only; never blocks the app.
pub async fn cross_verify_latest() -> impl IntoResponse {
    let dir = resolve_cross_verify_dir();
    // Dynamic operator data — never cache (post-impl security review).
    let headers = || {
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ]
    };
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            headers(),
            "{\"error\":\"no cross-verify run yet\"}".to_string(),
        )
    };
    let scan_dir = dir.clone();
    let Some(csv_path) = on_blocking_pool(move || newest_cross_verify_csv(&scan_dir))
        .await
        .flatten()
    else {
        return not_found();
    };
    // Bounded read (post-impl security review): a pathological mismatch
    // file is not slurped into RAM — `csv` goes null and the caller falls
    // back to the sibling summary counts.
    let csv_bytes = tokio::fs::metadata(&csv_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let (csv, mismatch_rows) = if csv_bytes > MAX_CROSS_VERIFY_CSV_BYTES {
        (serde_json::Value::Null, serde_json::Value::Null)
    } else {
        let Ok(csv) = tokio::fs::read_to_string(&csv_path).await else {
            return not_found();
        };
        // The CSV is header + one line per mismatched field-cell.
        let rows = csv.lines().filter(|l| !l.trim().is_empty()).count() as u64;
        (
            serde_json::Value::String(csv),
            serde_json::json!(rows.saturating_sub(1)),
        )
    };
    let file_name = csv_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let date = file_name
        .strip_prefix(CROSS_VERIFY_CSV_PREFIX)
        .and_then(|rest| rest.strip_suffix(CROSS_VERIFY_CSV_SUFFIX))
        .unwrap_or_default()
        .to_string();
    // Sibling summary JSON (written by the same run); null when absent
    // or unparseable (runs that pre-date the summary artefact).
    let summary_path = dir.join(format!(
        "{CROSS_VERIFY_CSV_PREFIX}{date}{CROSS_VERIFY_SUMMARY_SUFFIX}"
    ));
    let summary: serde_json::Value = match tokio::fs::read_to_string(&summary_path).await {
        Ok(body) => serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::Null,
    };
    // serde_json builds the body — the CSV payload (newlines, quotes) is
    // escaped correctly by construction, never by hand.
    let body = serde_json::json!({
        "date": date,
        "csv_file": file_name,
        "csv_bytes": csv_bytes,
        "mismatch_rows": mismatch_rows,
        "summary": summary,
        "csv": csv,
    })
    .to_string();
    (StatusCode::OK, headers(), body)
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    // One pass keeping the greatest name: O(entries), no list, no sort.
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(ERRORS_JSONL_PREFIX))
        })
        .max()
}

/// Runs a blocking directory scan on the blocking pool so it never holds a
/// runtime worker. `None` only when the task panicked or the runtime is
/// shutting down; callers answer as if nothing were found.
async fn on_blocking_pool<T: Send + 'static>(
    scan: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    tokio::task::spawn_blocking(scan).await.ok()
}

async fn read_text_file(
    path: &Path,
    content_type: &'static str,
) -> (StatusCode, [(header::HeaderName, &'static str); 1], String) {
    match tokio::fs::read_to_string(path).await {
        Ok(body) => (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body),
        Err(_) => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            format!(
                "{{\"error\":\"file not found\",\"path\":\"{}\"}}",
                path.display()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// O(1) sweep S4 (2026-10-03): every directory scan behind a handler
    /// runs on the blocking pool. A bare `read_dir` in an async handler
    /// holds a runtime worker for as long as the disk takes.
    #[test]
    fn every_handler_scan_runs_on_the_blocking_pool() {
        let src = include_str!("debug.rs");
        let production = &src[..src.find("#[cfg(test)]").expect("tests exist")];
        for (handler, scan) in [
            (
                "pub async fn logs_jsonl_latest",
                "on_blocking_pool(move || newest_jsonl(",
            ),
            (
                "pub async fn spill_status",
                "on_blocking_pool(spill_status_body)",
            ),
            (
                "pub async fn cross_verify_latest",
                "on_blocking_pool(move || newest_cross_verify_csv(",
            ),
        ] {
            let rest = production.split(handler).nth(1).expect("handler exists");
            // Up to the next top-level item, so one handler's check cannot
            // be satisfied by its neighbour.
            let end = ["\nfn ", "\npub ", "\nasync fn "]
                .iter()
                .filter_map(|item| rest.find(item))
                .min()
                .unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains(scan),
                "{handler} must run its scan via `{scan}`"
            );
        }
    }

    /// Process-global env-var lock. Plain `cargo test` runs every `#[test]`
    /// in this binary as a parallel THREAD of ONE process, and
    /// `LOGS_DIR_ENV` / `SPILL_DIR_ENV` are process-global state — two
    /// tests mutating them concurrently race. Reproduced locally at 3/60
    /// runs (`spill_status_reports_real_counts_when_dir_populated` reads
    /// `"exists":false` after its sibling swaps the var); on CI this was
    /// the chronic "Coverage & Perf" red, because that job runs plain
    /// `cargo test` under llvm-cov while the green Test (api) job uses
    /// nextest, whose process-per-test isolation masks the race.
    /// Every test that sets/removes/depends-on these vars takes this lock
    /// first. Poison is swallowed so one failing test doesn't cascade.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Minimal tempdir helper — avoids pulling in the `tempfile` crate
    /// (which isn't in the workspace deps). Returns a unique
    /// per-invocation path under the OS temp dir. Caller is responsible
    /// for cleanup on drop via the `DropDir` guard.
    fn mktemp(prefix: &str) -> DropDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("tv-debug-{prefix}-{pid}-{n}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create tmp dir");
        DropDir(path)
    }

    struct DropDir(PathBuf);
    impl DropDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for DropDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolve_logs_dir_respects_env_override() {
        let _env = env_guard();
        // SAFETY: set_var/remove_var mutate the process environment; the
        // ENV_LOCK guard above serializes every test that touches it.
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, "/tmp/tickvault-debug-test");
        }
        let dir = resolve_logs_dir();
        assert_eq!(dir, PathBuf::from("/tmp/tickvault-debug-test"));
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
    }

    #[test]
    fn resolve_logs_dir_default_when_env_unset() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
        let dir = resolve_logs_dir();
        assert_eq!(dir, PathBuf::from(DEFAULT_LOGS_DIR));
    }

    #[test]
    fn resolve_logs_dir_ignores_whitespace_env() {
        let _env = env_guard();
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, "   ");
        }
        let dir = resolve_logs_dir();
        assert_eq!(dir, PathBuf::from(DEFAULT_LOGS_DIR));
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
    }

    #[test]
    fn newest_jsonl_picks_latest_by_lex_order() {
        let tmp = mktemp("newest_jsonl");
        let dir = tmp.path();
        fs::write(dir.join("errors.jsonl.2026-04-18-15"), "").unwrap();
        fs::write(dir.join("errors.jsonl.2026-04-18-17"), "").unwrap();
        fs::write(dir.join("errors.jsonl.2026-04-18-16"), "").unwrap();
        fs::write(dir.join("unrelated.log"), "").unwrap();

        let newest = newest_jsonl(dir).expect("should find one");
        assert!(
            newest
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("2026-04-18-17")
        );
    }

    #[test]
    fn newest_jsonl_returns_none_when_dir_empty() {
        let tmp = mktemp("test");
        assert!(newest_jsonl(tmp.path()).is_none());
    }

    #[test]
    fn newest_jsonl_returns_none_when_no_match() {
        let tmp = mktemp("test");
        fs::write(tmp.path().join("app.log"), "").unwrap();
        fs::write(tmp.path().join("other.txt"), "").unwrap();
        assert!(newest_jsonl(tmp.path()).is_none());
    }

    #[test]
    fn newest_jsonl_returns_none_when_dir_missing() {
        let path = PathBuf::from("/nonexistent/path/claude-debug-test");
        assert!(newest_jsonl(&path).is_none());
    }

    #[tokio::test]
    async fn read_text_file_returns_404_when_missing() {
        let path = PathBuf::from("/nonexistent/claude-debug-test.md");
        let (status, _, body) = read_text_file(&path, "text/markdown").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("file not found"));
    }

    #[tokio::test]
    async fn read_text_file_returns_200_with_contents() {
        let tmp = mktemp("test");
        let path = tmp.path().join("summary.md");
        fs::write(&path, "# Errors\nnovel=0\n").unwrap();
        let (status, _, body) = read_text_file(&path, "text/markdown").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("novel=0"));
    }

    #[tokio::test]
    async fn logs_summary_returns_404_when_file_missing() {
        let _env = env_guard();
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, "/nonexistent-tickvault-logs-test");
        }
        let response = logs_summary().await.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
    }

    #[tokio::test]
    async fn logs_jsonl_latest_returns_404_when_dir_empty() {
        let _env = env_guard();
        let tmp = mktemp("test");
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = logs_jsonl_latest().await.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
    }

    #[test]
    fn read_tail_serves_a_small_file_whole() {
        let tmp = mktemp("tail-whole");
        let path = tmp.path().join("errors.jsonl.2026-10-26-09");
        fs::write(&path, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let tail = read_tail(&path, MAX_JSONL_RESPONSE_BYTES).unwrap();
        assert!(!tail.truncated);
        assert_eq!(tail.file_bytes, 16);
        assert_eq!(tail.body, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn read_tail_caps_a_large_file_at_a_line_boundary() {
        // Audit J4: a file over the cap is served as its tail, never read
        // whole, and the cut line is dropped.
        let tmp = mktemp("tail-cut");
        let path = tmp.path().join("errors.jsonl.2026-10-26-09");
        let line = "{\"message\":\"storm\"}\n";
        let body = line.repeat(100);
        fs::write(&path, &body).unwrap();
        // A cap that lands mid-line: 10 whole lines plus half of one.
        let cap = (line.len() * 10 + line.len() / 2) as u64;
        let tail = read_tail(&path, cap).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.file_bytes, body.len() as u64);
        assert_eq!(tail.body, line.repeat(10));
        assert!(tail.body.len() as u64 <= cap);
    }

    #[test]
    fn read_tail_errors_on_a_missing_file() {
        let tmp = mktemp("tail-missing");
        assert!(read_tail(&tmp.path().join("nope"), 1024).is_err());
    }

    #[test]
    fn read_tail_with_no_newline_in_the_tail_serves_nothing_rather_than_half_a_line() {
        let tmp = mktemp("tail-noline");
        let path = tmp.path().join("errors.jsonl.2026-10-26-09");
        fs::write(&path, "x".repeat(4096)).unwrap();
        let tail = read_tail(&path, 100).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.file_bytes, 4096);
        assert!(tail.body.is_empty());
    }

    #[tokio::test]
    async fn logs_jsonl_latest_answers_404_when_the_newest_entry_cannot_be_read() {
        // A directory carrying the file prefix is found by `newest_jsonl` but
        // fails to read; the handler answers 404 naming the path, never 500.
        let _env = env_guard();
        let tmp = mktemp("jsonl-unreadable");
        fs::create_dir(tmp.path().join("errors.jsonl.2026-10-26-09")).unwrap();
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = logs_jsonl_latest().await;
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("file not found"), "{text}");
        assert!(text.contains("errors.jsonl.2026-10-26-09"), "{text}");
    }

    #[test]
    fn jsonl_response_cap_matches_the_cross_verify_cap() {
        assert_eq!(MAX_JSONL_RESPONSE_BYTES, MAX_CROSS_VERIFY_CSV_BYTES);
        assert_eq!(MAX_JSONL_RESPONSE_BYTES, 10 * 1024 * 1024);
    }

    #[tokio::test]
    async fn logs_jsonl_latest_serves_a_small_file_untruncated() {
        let _env = env_guard();
        let tmp = mktemp("jsonl-small");
        fs::write(
            tmp.path().join("errors.jsonl.2026-10-26-09"),
            "{\"m\":\"x\"}\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = logs_jsonl_latest().await;
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(TRUNCATED_HEADER).unwrap(), "false");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"{\"m\":\"x\"}\n");
    }

    #[tokio::test]
    async fn logs_jsonl_latest_marks_a_file_over_the_cap_as_truncated() {
        let _env = env_guard();
        let tmp = mktemp("jsonl-big");
        let line = format!("{{\"message\":\"{}\"}}\n", "e".repeat(1000));
        let lines = (MAX_JSONL_RESPONSE_BYTES as usize / line.len()) + 50;
        fs::write(
            tmp.path().join("errors.jsonl.2026-10-26-09"),
            line.repeat(lines),
        )
        .unwrap();
        unsafe {
            std::env::set_var(LOGS_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = logs_jsonl_latest().await;
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(TRUNCATED_HEADER).unwrap(), "true");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("{\"truncated\":true,\"file_bytes\":"),
            "{first}"
        );
        // Marker line plus at most the cap.
        assert!(body.len() as u64 <= MAX_JSONL_RESPONSE_BYTES + 128);
        assert!(text.lines().skip(1).all(|l| l == line.trim_end()));
    }

    #[test]
    fn resolve_spill_dir_default_when_env_unset() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var(SPILL_DIR_ENV);
        }
        assert_eq!(resolve_spill_dir(), PathBuf::from(DEFAULT_SPILL_DIR));
    }

    #[test]
    fn resolve_spill_dir_respects_env_override() {
        let _env = env_guard();
        unsafe {
            std::env::set_var(SPILL_DIR_ENV, "/tmp/tv-spill-test");
        }
        assert_eq!(resolve_spill_dir(), PathBuf::from("/tmp/tv-spill-test"));
        unsafe {
            std::env::remove_var(SPILL_DIR_ENV);
        }
    }

    #[test]
    fn scan_spill_category_returns_zero_when_dir_missing() {
        let path = PathBuf::from("/nonexistent/spill-test-dir");
        let (count, bytes, newest) = scan_spill_category(&path);
        assert_eq!(count, 0);
        assert_eq!(bytes, 0);
        assert!(newest.is_none());
    }

    #[test]
    fn scan_spill_category_counts_bin_files_and_picks_newest() {
        let tmp = mktemp("scan_spill");
        let dir = tmp.path();
        fs::write(dir.join("ticks-1.bin"), b"abc").unwrap();
        fs::write(dir.join("ticks-2.bin"), b"defgh").unwrap();
        fs::write(dir.join("not-spill.txt"), b"ignored").unwrap();
        let (count, bytes, newest) = scan_spill_category(dir);
        assert_eq!(count, 2);
        assert_eq!(bytes, 8);
        assert!(newest.is_some());
    }

    #[test]
    fn json_escape_handles_quotes_and_backslashes() {
        assert_eq!(json_escape("simple"), "simple");
        assert_eq!(json_escape(r#"path"with"quotes"#), r#"path\"with\"quotes"#);
        assert_eq!(json_escape(r"back\slash"), r"back\\slash");
    }

    #[tokio::test]
    async fn spill_status_returns_200_with_categories_even_when_dir_missing() {
        let _env = env_guard();
        unsafe {
            std::env::set_var(SPILL_DIR_ENV, "/nonexistent/spill-test-dir-xyz");
        }
        let response = spill_status().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("\"exists\":false"));
        assert!(body_str.contains("\"ticks\""));
        assert!(body_str.contains("\"candles\""));
        assert!(body_str.contains("\"depth\""));
        assert!(body_str.contains("\"file_count\":0"));
        unsafe {
            std::env::remove_var(SPILL_DIR_ENV);
        }
    }

    // -------------------------------------------------------------------
    // GET /api/debug/cross-verify/latest (visibility directive 2026-06-10)
    // -------------------------------------------------------------------

    #[test]
    fn newest_cross_verify_csv_excludes_summary_json_and_foreign_files() {
        // Pre-impl findings H3 + S-H2: the sibling .summary.json shares the
        // prefix and sorts AFTER the .csv for the same date — the strict
        // .csv suffix predicate must keep it (and foreign files) out.
        let tmp = mktemp("cv_newest");
        let dir = tmp.path();
        fs::write(dir.join("cross-verify-1m-2026-06-09.csv"), "h\n").unwrap();
        fs::write(dir.join("cross-verify-1m-2026-06-10.csv"), "h\n").unwrap();
        fs::write(dir.join("cross-verify-1m-2026-06-10.summary.json"), "{}").unwrap();
        fs::write(dir.join("evil.csv"), "h\n").unwrap();
        fs::write(dir.join("unrelated.txt"), "x").unwrap();
        let newest = newest_cross_verify_csv(dir).expect("should find a csv");
        assert_eq!(
            newest.file_name().unwrap().to_str().unwrap(),
            "cross-verify-1m-2026-06-10.csv"
        );
    }

    #[test]
    fn newest_cross_verify_csv_returns_none_when_dir_missing_or_empty() {
        assert!(newest_cross_verify_csv(Path::new("/nonexistent/cv-test")).is_none());
        let tmp = mktemp("cv_empty");
        assert!(newest_cross_verify_csv(tmp.path()).is_none());
    }

    #[tokio::test]
    async fn cross_verify_latest_returns_404_when_missing() {
        let _env = env_guard();
        unsafe {
            std::env::set_var(CROSS_VERIFY_DIR_ENV, "/nonexistent-tv-cross-verify-test");
        }
        let response = cross_verify_latest().await.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("no cross-verify run yet"));
        // Pre-impl finding S-M1: the resolved filesystem path must NOT be
        // disclosed in the 404 body.
        assert!(!body_str.contains("nonexistent-tv-cross-verify-test"));
        unsafe {
            std::env::remove_var(CROSS_VERIFY_DIR_ENV);
        }
    }

    #[tokio::test]
    async fn cross_verify_latest_returns_200_with_content() {
        let _env = env_guard();
        let tmp = mktemp("cv_latest");
        let csv = "run_ts,trading_date,security_id\n1,2026-06-10,13\n";
        fs::write(tmp.path().join("cross-verify-1m-2026-06-10.csv"), csv).unwrap();
        unsafe {
            std::env::set_var(CROSS_VERIFY_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = cross_verify_latest().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(v["date"], "2026-06-10");
        assert_eq!(v["csv_file"], "cross-verify-1m-2026-06-10.csv");
        assert_eq!(v["mismatch_rows"], 1, "header excluded from the count");
        assert!(v["summary"].is_null(), "no summary.json on disk → null");
        assert_eq!(v["csv"].as_str().unwrap(), csv, "CSV served verbatim");
        unsafe {
            std::env::remove_var(CROSS_VERIFY_DIR_ENV);
        }
    }

    #[tokio::test]
    async fn cross_verify_latest_merges_summary_json() {
        let _env = env_guard();
        let tmp = mktemp("cv_summary");
        fs::write(
            tmp.path().join("cross-verify-1m-2026-06-10.csv"),
            "header-only\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("cross-verify-1m-2026-06-10.summary.json"),
            r#"{"compared":91230,"mismatches":0,"degraded":false}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var(CROSS_VERIFY_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = cross_verify_latest().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(v["mismatch_rows"], 0, "header-only CSV = zero mismatches");
        assert_eq!(v["summary"]["compared"], 91_230);
        assert_eq!(v["summary"]["degraded"], false);
        unsafe {
            std::env::remove_var(CROSS_VERIFY_DIR_ENV);
        }
    }

    #[test]
    fn resolve_cross_verify_dir_default_and_override() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var(CROSS_VERIFY_DIR_ENV);
        }
        assert_eq!(
            resolve_cross_verify_dir(),
            PathBuf::from(DEFAULT_CROSS_VERIFY_DIR)
        );
        unsafe {
            std::env::set_var(CROSS_VERIFY_DIR_ENV, "/tmp/tv-cv-test");
        }
        assert_eq!(resolve_cross_verify_dir(), PathBuf::from("/tmp/tv-cv-test"));
        unsafe {
            std::env::remove_var(CROSS_VERIFY_DIR_ENV);
        }
    }

    #[tokio::test]
    async fn spill_status_reports_real_counts_when_dir_populated() {
        let _env = env_guard();
        let tmp = mktemp("spill_status");
        // Create category subdirs with one bin each
        for kind in ["ticks", "candles", "depth"] {
            let cat_dir = tmp.path().join(kind);
            fs::create_dir_all(&cat_dir).unwrap();
            fs::write(cat_dir.join(format!("{kind}-2026-04-19.bin")), b"hello").unwrap();
        }
        unsafe {
            std::env::set_var(SPILL_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let response = spill_status().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("\"exists\":true"));
        assert!(body_str.contains("\"file_count\":1"));
        assert!(body_str.contains("\"total_bytes\":5"));
        assert!(body_str.contains("ticks-2026-04-19.bin"));
        unsafe {
            std::env::remove_var(SPILL_DIR_ENV);
        }
    }

    #[test]
    fn spill_status_reports_the_top_volume_ilp_folder() {
        // 2026-10-13 (top_volume spill plan item 7): the top_volume folder is
        // a category, and ILP spill files are counted.
        let _env = env_guard();
        let tmp = mktemp("spill_status_tv");
        let cat_dir = tmp.path().join("top_volume");
        fs::create_dir_all(&cat_dir).unwrap();
        fs::write(cat_dir.join("top_volume-dhan-490000.ilp"), b"row\n").unwrap();
        unsafe {
            std::env::set_var(SPILL_DIR_ENV, tmp.path().to_str().unwrap());
        }
        let body = spill_status_body();
        unsafe {
            std::env::remove_var(SPILL_DIR_ENV);
        }
        assert!(
            body.contains(
                "\"top_volume\":{\"file_count\":1,\"total_bytes\":4,\"newest_file\":\"top_volume-dhan-490000.ilp\"}"
            ),
            "{body}"
        );
    }
}
