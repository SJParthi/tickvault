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
/// `<repo_root>/data/logs`. Overridable via env var `TV_LOGS_DIR` so
/// deployments that store logs elsewhere (e.g. EFS mount on AWS) work
/// without code changes.
const DEFAULT_LOGS_DIR: &str = "data/logs";
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
    use std::sync::atomic::{AtomicU64, Ordering};

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
        // Safety: tests run serially per #[test] and read env vars.
        // SAFETY: set_var/remove_var require a mutable environment which
        // is safe within a single-threaded test execution.
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
        unsafe {
            std::env::remove_var(LOGS_DIR_ENV);
        }
        let dir = resolve_logs_dir();
        assert_eq!(dir, PathBuf::from(DEFAULT_LOGS_DIR));
    }

    #[test]
    fn resolve_logs_dir_ignores_whitespace_env() {
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
        unsafe {
            std::env::remove_var(SPILL_DIR_ENV);
        }
        assert_eq!(resolve_spill_dir(), PathBuf::from(DEFAULT_SPILL_DIR));
    }

    #[test]
    fn resolve_spill_dir_respects_env_override() {
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

    #[tokio::test]
    async fn spill_status_reports_real_counts_when_dir_populated() {
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
