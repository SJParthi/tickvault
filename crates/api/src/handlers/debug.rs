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
pub async fn logs_jsonl_latest() -> impl IntoResponse {
    let dir = resolve_logs_dir();
    match newest_jsonl(&dir) {
        Some(path) => read_text_file(&path, "application/x-ndjson; charset=utf-8").await,
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            "{\"error\":\"no errors.jsonl file present\"}".to_string(),
        ),
    }
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
    let dir = resolve_spill_dir();
    let exists = dir.is_dir();

    let mut categories = String::from("{");
    let mut first = true;
    for kind in ["ticks", "candles", "depth"] {
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

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
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

/// Default cross-verify artefact directory. Matches `CROSS_VERIFY_CSV_DIR`
/// in `crates/app/src/cross_verify_1m_boot.rs` (the 15:31 IST post-market
/// run writes `cross-verify-1m-YYYY-MM-DD.csv` + a sibling
/// `cross-verify-1m-YYYY-MM-DD.summary.json` there).
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
    let entries = std::fs::read_dir(dir).ok()?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(CROSS_VERIFY_CSV_PREFIX) && n.ends_with(CROSS_VERIFY_CSV_SUFFIX)
            })
        })
        .collect();
    matches.sort();
    matches.pop()
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
    let Some(csv_path) = newest_cross_verify_csv(&dir) else {
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
    let entries = std::fs::read_dir(dir).ok()?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(ERRORS_JSONL_PREFIX))
        })
        .collect();
    matches.sort();
    matches.pop()
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
}
