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

fn resolve_logs_dir() -> PathBuf {
    if let Ok(custom) = std::env::var(LOGS_DIR_ENV) {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
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
}
