//! The 14 MCP tools — behavior-parity ports of server.py's tool functions
//! (`tool_tail_errors` .. `tool_cloudwatch_logs`) + the advertised registry
//! (byte-identical names / descriptions / inputSchema values; object key
//! ORDER is normalized by the parity harness, array order is preserved).
//!
//! Documented bounded deviations from CPython (each also listed in the PR
//! body; none reachable from the parity transcript):
//!   - Non-object JSON lines in errors.jsonl.* are skipped/filtered
//!     gracefully where Python would raise AttributeError.
//!   - Non-string values for string-typed args produce a typed error
//!     instead of CPython's TypeError text.
//!   - OS/library-level failure TEXT (spawn errors, transport errors,
//!     invalid-regex details, JSON-decode details) differs from CPython's
//!     exception strings; the surrounding JSON shape is identical and the
//!     harness masks exactly those fields.
//!   - Invalid UTF-8 in log files: Python read_text() raises (tool error);
//!     Rust skips the file (tail/history) — the app's sinks are UTF-8.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::config::{self, Ctx, Env, RealEnv};
use crate::pycompat::{
    decode_utf8_ignore, decode_utf8_replace, py_fnmatch, py_int, py_slice_chars, py_splitlines,
    py_tail_chars, py_urllib_quote,
};
use crate::signature::signature_hash;
use crate::sigv4::{
    AmzNow, build_cloudwatch_filter_args, build_cloudwatch_sigv4_request,
    filter_and_trim_portal_events, parse_cloudwatch_events, parse_portal_logs_raw,
};

pub const ERRORS_JSONL_PREFIX: &str = "errors.jsonl";
pub const SUMMARY_FILENAME: &str = "errors.summary.md";
pub const AUTO_FIX_LOG: &str = "auto-fix.log";

// ---------------------------------------------------------------------------
// Small CPython-parity helpers
// ---------------------------------------------------------------------------

/// Python text-mode universal-newline translation (`\r\n`/`\r` -> `\n`) —
/// applied everywhere server.py reads files / subprocess output in text
/// mode (read_text, open(..., "r"), subprocess text=True).
fn py_textmode(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Python file-object line iteration semantics AFTER universal-newline
/// translation, with terminators stripped: `"a\nb\n"` -> ["a","b"],
/// `"a\nb"` -> ["a","b"], `""` -> [].
fn py_file_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        v.pop();
    }
    v
}

/// Python `lines[-limit:]` slice (handles 0 => everything, negatives).
fn py_neg_slice<T>(lines: &[T], limit: i64) -> &[T] {
    let n = lines.len() as i64;
    let start = if limit > 0 {
        (n - limit).max(0)
    } else {
        (-limit).min(n)
    };
    &lines[start as usize..]
}

/// Python `datetime.isoformat()` for an offset-aware datetime: seconds,
/// `.ffffff` only when the microseconds are non-zero, offset as `+HH:MM`.
fn py_isoformat(dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
    use chrono::{Offset, Timelike};
    let micros = dt.nanosecond() / 1_000;
    let mut s = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    if micros != 0 {
        s.push_str(&format!(".{micros:06}"));
    }
    let secs = dt.offset().fix().local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    s.push_str(&format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60));
    s
}

/// Python truthy env read with `or`-chaining semantics ("" is falsy).
fn env_truthy(env: &dyn Env, key: &str) -> Option<String> {
    env.get(key).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// errors.jsonl.* discovery + event parsing (server.py:209-237)
// ---------------------------------------------------------------------------

/// Newest-first list of errors.jsonl.* files, with the 2026-07-05 grace
/// window merging the legacy top-level dir when `dir_path` is `machine/`.
/// Duplicate rotation names prefer the machine copy.
pub fn iter_errors_jsonl_files(dir_path: &Path) -> Vec<(String, PathBuf)> {
    use std::collections::BTreeMap;
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    let legacy_parent = if dir_path.file_name().and_then(|n| n.to_str()) == Some("machine") {
        dir_path.parent().map(Path::to_path_buf)
    } else {
        None
    };
    for candidate in [legacy_parent, Some(dir_path.to_path_buf())]
        .into_iter()
        .flatten()
    {
        if !candidate.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&candidate) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_file() && name.starts_with(ERRORS_JSONL_PREFIX) {
                // Later dir (machine) overwrites the legacy copy — same as
                // the Python dict assignment.
                seen.insert(name, path);
            }
        }
    }
    let mut out: Vec<(String, PathBuf)> = seen.into_iter().collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

/// Python `_parse_event`: strip, skip empty, JSON-parse or None.
pub fn parse_event(line: &str) -> Option<Value> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(t).ok()
}

/// `_signature_hash(ev.get("code"), ev.get("target") or "", ev.get("message") or "")`
/// over a parsed event object (non-string fields degrade to None/"" — a
/// documented deviation; Python would raise on non-string values).
fn event_signature(ev: &Map<String, Value>) -> String {
    let code = ev.get("code").and_then(Value::as_str);
    let target = ev.get("target").and_then(Value::as_str).unwrap_or("");
    let message = ev.get("message").and_then(Value::as_str).unwrap_or("");
    signature_hash(code, target, message)
}

// ---------------------------------------------------------------------------
// File-backed tools
// ---------------------------------------------------------------------------

/// server.py `tool_tail_errors`.
pub fn tool_tail_errors(ctx: &Ctx, limit: i64, code: Option<&str>) -> Value {
    let dir_path = ctx.machine_logs_dir();
    let files = iter_errors_jsonl_files(&dir_path);
    let mut events: Vec<Value> = Vec::new();
    for (_, f) in &files {
        let Ok(raw) = std::fs::read_to_string(f) else {
            continue;
        };
        let text = py_textmode(&raw);
        let lines = py_splitlines(&text);
        for line in lines.iter().rev() {
            let Some(ev) = parse_event(line) else {
                continue;
            };
            if let Some(want) = code {
                let got = ev
                    .as_object()
                    .and_then(|o| o.get("code"))
                    .and_then(Value::as_str);
                if got != Some(want) {
                    continue;
                }
            }
            events.push(ev);
            if events.len() as i64 >= limit {
                break;
            }
        }
        if events.len() as i64 >= limit {
            break;
        }
    }
    json!({
        "dir": dir_path.to_string_lossy(),
        "count": events.len(),
        "files_scanned": files.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        "events": events,
    })
}

struct NovelInfo {
    code: Value,
    severity: Value,
    target: Value,
    message_trunc: String,
    first_seen_ts: Option<chrono::DateTime<chrono::FixedOffset>>,
}

/// Python's `ts_str.replace("Z", "+00:00")` + `datetime.fromisoformat`.
fn parse_event_ts(ev: &Map<String, Value>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let ts_str = ev.get("timestamp").and_then(Value::as_str)?;
    if ts_str.is_empty() {
        return None;
    }
    let normalised = ts_str.replace('Z', "+00:00");
    chrono::DateTime::parse_from_rfc3339(&normalised).ok()
}

/// server.py `tool_list_novel_signatures`.
pub fn tool_list_novel_signatures(ctx: &Ctx, since_minutes: i64) -> Value {
    use std::collections::HashMap;
    let dir_path = ctx.machine_logs_dir();
    let files = iter_errors_jsonl_files(&dir_path);
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(since_minutes);

    let mut order: Vec<String> = Vec::new();
    let mut first_seen: HashMap<String, NovelInfo> = HashMap::new();
    for (_, f) in &files {
        let Ok(raw) = std::fs::read_to_string(f) else {
            continue;
        };
        let text = py_textmode(&raw);
        for line in py_splitlines(&text) {
            let Some(ev) = parse_event(line) else {
                continue;
            };
            let Some(obj) = ev.as_object() else {
                continue; // documented deviation (Python raises)
            };
            let sig = event_signature(obj);
            let ts = parse_event_ts(obj);
            let replace = match first_seen.get(&sig) {
                None => true,
                Some(existing) => match (&ts, &existing.first_seen_ts) {
                    (Some(new_ts), Some(old_ts)) => new_ts < old_ts,
                    _ => false,
                },
            };
            if replace {
                if !first_seen.contains_key(&sig) {
                    order.push(sig.clone());
                }
                first_seen.insert(
                    sig.clone(),
                    NovelInfo {
                        code: obj.get("code").cloned().unwrap_or(Value::Null),
                        severity: obj.get("severity").cloned().unwrap_or(Value::Null),
                        target: obj.get("target").cloned().unwrap_or(Value::Null),
                        message_trunc: py_slice_chars(
                            obj.get("message").and_then(Value::as_str).unwrap_or(""),
                            200,
                        )
                        .to_string(),
                        first_seen_ts: ts,
                    },
                );
            }
        }
    }

    let mut novel: Vec<Value> = Vec::new();
    for sig in &order {
        let Some(info) = first_seen.get(sig) else {
            continue;
        };
        if let Some(ts) = &info.first_seen_ts
            && *ts >= cutoff
        {
            novel.push(json!({
                "signature": sig,
                "code": info.code,
                "severity": info.severity,
                "target": info.target,
                "message": info.message_trunc,
                "first_seen_ts": py_isoformat(ts),
            }));
        }
    }

    // cutoff_utc: shape-parity only (isoformat of an aware-UTC now); the
    // harness masks this volatile field.
    let cutoff_fixed = cutoff.fixed_offset();
    json!({
        "dir": dir_path.to_string_lossy(),
        "since_minutes": since_minutes,
        "cutoff_utc": py_isoformat(&cutoff_fixed),
        "novel_count": novel.len(),
        "novel": novel,
    })
}

/// server.py `tool_summary_snapshot`.
pub fn tool_summary_snapshot(ctx: &Ctx) -> Value {
    let mut path = ctx.machine_logs_dir().join(SUMMARY_FILENAME);
    if !path.exists() {
        let legacy = ctx.logs_dir().join(SUMMARY_FILENAME);
        if legacy.exists() {
            path = legacy;
        }
    }
    if !path.exists() {
        return json!({
            "path": path.to_string_lossy(),
            "exists": false,
            "markdown": "",
        });
    }
    match std::fs::read_to_string(&path) {
        Err(err) => json!({
            "path": path.to_string_lossy(),
            "exists": true,
            "error": err.to_string(), // OS error text — documented deviation
            "markdown": "",
        }),
        Ok(raw) => {
            let markdown = py_textmode(&raw);
            let line_count = markdown.matches('\n').count();
            json!({
                "path": path.to_string_lossy(),
                "exists": true,
                "markdown": markdown,
                "line_count": line_count,
            })
        }
    }
}

/// server.py `tool_triage_log_tail`.
pub fn tool_triage_log_tail(ctx: &Ctx, limit: i64) -> Value {
    let path = ctx.logs_dir().join(AUTO_FIX_LOG);
    if !path.exists() {
        return json!({"path": path.to_string_lossy(), "exists": false, "lines": []});
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            return json!({
                "path": path.to_string_lossy(),
                "exists": true,
                "error": err.to_string(),
                "lines": [],
            });
        }
    };
    let text = py_textmode(&raw);
    let lines = py_splitlines(&text);
    let tail: &[&str] = if lines.len() as i64 > limit {
        py_neg_slice(&lines, limit)
    } else {
        &lines
    };
    json!({
        "path": path.to_string_lossy(),
        "exists": true,
        "total_lines": lines.len(),
        "returned": tail.len(),
        "lines": tail,
    })
}

/// server.py `tool_signature_history`. `signature` is echoed verbatim
/// (Python echoes whatever JSON value was passed).
pub fn tool_signature_history(ctx: &Ctx, signature: &Value, limit: i64) -> Value {
    let want = signature.as_str();
    let dir_path = ctx.machine_logs_dir();
    let files = iter_errors_jsonl_files(&dir_path);
    let mut matches: Vec<Value> = Vec::new();
    for (_, f) in &files {
        let Ok(raw) = std::fs::read_to_string(f) else {
            continue;
        };
        let text = py_textmode(&raw);
        for line in py_splitlines(&text) {
            let Some(ev) = parse_event(line) else {
                continue;
            };
            let Some(obj) = ev.as_object() else {
                continue; // documented deviation
            };
            let sig = event_signature(obj);
            if Some(sig.as_str()) == want {
                matches.push(ev);
                if matches.len() as i64 >= limit {
                    break;
                }
            }
        }
        if matches.len() as i64 >= limit {
            break;
        }
    }
    json!({
        "signature": signature,
        "count": matches.len(),
        "events": matches,
    })
}

/// Recursive `*.md` walk (Python `Path.rglob("*.md")` equivalent for the
/// runbook/rules trees — no symlinked dirs exist there).
fn rglob_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            rglob_md(&path, out);
        } else if ft.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if py_fnmatch(&name, "*.md") {
                out.push(path);
            }
        }
    }
}

/// server.py `tool_find_runbook_for_code`.
pub fn tool_find_runbook_for_code(ctx: &Ctx, code: &str) -> Value {
    let root = &ctx.repo_root;
    let runbooks_dir = root.join("docs").join("runbooks");
    let rules_dir = root.join(".claude").join("rules");

    let mut matches: Vec<Value> = Vec::new();
    for search_dir in [runbooks_dir, rules_dir] {
        if !search_dir.exists() {
            continue;
        }
        let mut md_files = Vec::new();
        rglob_md(&search_dir, &mut md_files);
        for md in md_files {
            let Ok(raw) = std::fs::read_to_string(&md) else {
                continue;
            };
            let text = py_textmode(&raw);
            if !text.contains(code) {
                continue;
            }
            let lines = py_splitlines(&text);
            for (idx, line) in lines.iter().enumerate() {
                if line.contains(code) {
                    let start = idx.saturating_sub(2);
                    let end = (idx + 4).min(lines.len());
                    let preview = lines[start..end].join("\n");
                    let rel = md
                        .strip_prefix(root)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| md.to_string_lossy().into_owned());
                    matches.push(json!({
                        "file": rel,
                        "first_line": idx + 1,
                        "preview": preview,
                    }));
                    break;
                }
            }
        }
    }

    json!({
        "code": code,
        "match_count": matches.len(),
        "matches": matches,
    })
}

// ---------------------------------------------------------------------------
// HTTP tools (blocking reqwest — cold path, out-of-process)
// ---------------------------------------------------------------------------

fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("http client build failed: {e}"))
}

/// server.py `tool_questdb_sql`.
pub fn tool_questdb_sql(ctx: &Ctx, query: &str) -> Value {
    let env = RealEnv;
    let qdb = config::endpoint_url(
        &env,
        &ctx.cfg,
        "questdb_url",
        "TICKVAULT_QUESTDB_URL",
        "http://127.0.0.1:9000",
        None,
    );
    let full = format!("{qdb}/exec?query={}", py_urllib_quote(query));
    let client = match http_client(15) {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "query": query, "error": e}),
    };
    let resp = match client.get(&full).send() {
        Ok(r) => r,
        Err(e) => {
            // Transport error text differs from urllib's — harness masks.
            return json!({"ok": false, "query": query, "error": e.to_string()});
        }
    };
    let status = resp.status();
    if status.as_u16() >= 400 {
        // Python urlopen raises HTTPError; str(err) == "HTTP Error {code}: {reason}".
        let reason = status.canonical_reason().unwrap_or("");
        return json!({
            "ok": false,
            "query": query,
            "error": format!("HTTP Error {}: {}", status.as_u16(), reason),
        });
    }
    let body = match resp.bytes() {
        Ok(b) => decode_utf8_replace(&b),
        Err(e) => return json!({"ok": false, "query": query, "error": e.to_string()}),
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(parsed) => json!({"ok": true, "query": query, "response": parsed}),
        Err(e) => {
            // Python JSONDecodeError text differs — documented deviation.
            json!({"ok": false, "query": query, "error": e.to_string()})
        }
    }
}

/// Minimal `urllib.parse.urlsplit`-alike: (lowercased scheme, lowercased
/// hostname). Only what the bearer plaintext-refusal check needs.
fn split_scheme_host(url: &str) -> (String, Option<String>) {
    let Some(idx) = url.find("://") else {
        return (String::new(), None);
    };
    let scheme = url[..idx].to_ascii_lowercase();
    let rest = &url[idx + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut netloc = &rest[..end];
    if let Some(at) = netloc.rfind('@') {
        netloc = &netloc[at + 1..];
    }
    let host = if let Some(stripped) = netloc.strip_prefix('[') {
        stripped
            .split(']')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    } else {
        netloc.split(':').next().unwrap_or("").to_ascii_lowercase()
    };
    if host.is_empty() {
        (scheme, None)
    } else {
        (scheme, Some(host))
    }
}

/// server.py `tool_tickvault_api`.
pub fn tool_tickvault_api(ctx: &Ctx, path: &str, base_url: Option<&str>) -> Value {
    let env = RealEnv;
    let api_url = config::endpoint_url(
        &env,
        &ctx.cfg,
        "tickvault_api_url",
        "TICKVAULT_API_URL",
        "http://127.0.0.1:3001",
        base_url,
    );
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let full = format!("{api_url}{path}");
    let bearer = env
        .get("TICKVAULT_API_BEARER_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut req_builder = match http_client(10) {
        Ok(c) => c.get(&full),
        Err(e) => return json!({"ok": false, "error": e, "url": full}),
    };
    if !bearer.is_empty() {
        // Security parity with server.py (adversarial re-review 2026-07-04):
        // never send the bearer over plaintext http to a non-local host.
        let (scheme, host) = split_scheme_host(&full);
        let host = host.unwrap_or_default();
        let is_local = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
        if scheme != "https" && !is_local {
            return json!({
                "ok": false,
                "error": "refusing to send TICKVAULT_API_BEARER_TOKEN over plaintext http to a non-localhost host — use an https:// tickvault_api_url, or unset the token for the tokenless public GETs",
                "url": full,
            });
        }
        req_builder = req_builder.header("Authorization", format!("Bearer {bearer}"));
    }
    let resp = match req_builder.send() {
        Ok(r) => r,
        Err(e) => {
            // urllib error text differs — never echoes the Authorization
            // header (reqwest error Display carries the URL only).
            return json!({"ok": false, "error": e.to_string(), "url": full});
        }
    };
    let status = resp.status();
    if status.as_u16() >= 400 {
        let reason = status.canonical_reason().unwrap_or("");
        return json!({
            "ok": false,
            "error": format!("HTTP Error {}: {}", status.as_u16(), reason),
            "url": full,
        });
    }
    let status_code = status.as_u16();
    let body = match resp.bytes() {
        Ok(b) => decode_utf8_replace(&b),
        Err(e) => return json!({"ok": false, "error": e.to_string(), "url": full}),
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(parsed) => json!({"ok": true, "status": status_code, "url": full, "json": parsed}),
        Err(_) => json!({
            "ok": true,
            "status": status_code,
            "url": full,
            "text": py_slice_chars(&body, 4000),
        }),
    }
}

// ---------------------------------------------------------------------------
// Subprocess tools (manual timeout — std only)
// ---------------------------------------------------------------------------

pub struct ProcResult {
    pub code: i64,
    pub stdout: String,
    pub stderr: String,
}

pub enum ProcError {
    Spawn(String),
    Timeout,
}

fn run_with_timeout(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<ProcResult, ProcError> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ProcError::Spawn(e.to_string()))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ignored = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ignored = p.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_handle.join().unwrap_or_default();
                let stderr = err_handle.join().unwrap_or_default();
                let code = match status.code() {
                    Some(c) => i64::from(c),
                    None => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            status.signal().map(|s| -i64::from(s)).unwrap_or(-1)
                        }
                        #[cfg(not(unix))]
                        {
                            -1
                        }
                    }
                };
                return Ok(ProcResult {
                    code,
                    stdout: py_textmode(&decode_utf8_replace(&stdout)),
                    stderr: py_textmode(&decode_utf8_replace(&stderr)),
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ignored = child.kill();
                    let _ignored = child.wait();
                    return Err(ProcError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ignored = child.kill();
                return Err(ProcError::Spawn(e.to_string()));
            }
        }
    }
}

/// Pure parser for doctor.sh stdout (unit-testable).
pub fn parse_doctor_stdout(stdout: &str) -> (Vec<Value>, i64, i64) {
    let mut rows: Vec<Value> = Vec::new();
    let mut pass_count = 0i64;
    let mut fail_count = 0i64;
    for line in py_splitlines(stdout) {
        let line = line.trim_end();
        for status in ["[PASS]", "[FAIL]", "[WARN]", "[SKIP]"] {
            if let Some(stripped) = line.strip_prefix(status) {
                let rest = stripped.trim();
                let bare = status.trim_matches(['[', ']']);
                rows.push(json!({"status": bare, "detail": rest}));
                if status == "[PASS]" {
                    pass_count += 1;
                } else if status == "[FAIL]" {
                    fail_count += 1;
                }
                break;
            }
        }
    }
    (rows, pass_count, fail_count)
}

/// server.py `tool_run_doctor`.
pub fn tool_run_doctor(ctx: &Ctx) -> Value {
    let out = match run_with_timeout(
        "bash",
        &["scripts/doctor.sh".to_string()],
        &ctx.repo_root,
        Duration::from_secs(120),
    ) {
        Ok(out) => out,
        Err(ProcError::Timeout) => {
            return json!({"ok": false, "error": "doctor.sh timed out after 120s"});
        }
        Err(ProcError::Spawn(err)) => {
            return json!({"ok": false, "error": format!("bash not available: {err}")});
        }
    };
    let (rows, pass_count, fail_count) = parse_doctor_stdout(&out.stdout);
    json!({
        "ok": out.code == 0,
        "exit_code": out.code,
        "pass_count": pass_count,
        "fail_count": fail_count,
        "rows": rows,
        "raw_stdout_tail": if out.stdout.is_empty() { "" } else { py_tail_chars(&out.stdout, 2000) },
    })
}

/// Pure parser for the `git log --pretty=format:%H%x1f%an%x1f%aI%x1f%s`
/// output (unit-testable).
pub fn parse_git_log_stdout(stdout: &str) -> Vec<Value> {
    let mut commits: Vec<Value> = Vec::new();
    for line in py_splitlines(stdout) {
        let parts: Vec<&str> = line.split('\u{1f}').collect();
        if parts.len() != 4 {
            continue;
        }
        commits.push(json!({
            "sha": py_slice_chars(parts[0], 12),
            "author": parts[1],
            "date": parts[2],
            "subject": parts[3],
        }));
    }
    commits
}

/// server.py `tool_git_recent_log`.
pub fn tool_git_recent_log(ctx: &Ctx, limit: i64) -> Value {
    let fmt = "%H%x1f%an%x1f%aI%x1f%s";
    let out = match run_with_timeout(
        "git",
        &[
            "log".to_string(),
            format!("-{limit}"),
            format!("--pretty=format:{fmt}"),
        ],
        &ctx.repo_root,
        Duration::from_secs(10),
    ) {
        Ok(out) => out,
        Err(ProcError::Timeout) => return json!({"ok": false, "error": "git log timed out"}),
        Err(ProcError::Spawn(_)) => return json!({"ok": false, "error": "git not available"}),
    };
    if out.code != 0 {
        return json!({"ok": false, "error": out.stderr.trim()});
    }
    let commits = parse_git_log_stdout(&out.stdout);
    json!({"ok": true, "count": commits.len(), "commits": commits})
}

/// server.py `tool_docker_status`.
pub fn tool_docker_status(ctx: &Ctx) -> Value {
    let compose_file = ctx
        .repo_root
        .join("deploy")
        .join("docker")
        .join("docker-compose.yml");
    if !compose_file.exists() {
        return json!({
            "ok": false,
            "error": format!("compose file not found: {}", compose_file.to_string_lossy()),
        });
    }
    let out = match run_with_timeout(
        "docker",
        &[
            "compose".to_string(),
            "-f".to_string(),
            compose_file.to_string_lossy().into_owned(),
            "ps".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ],
        &ctx.repo_root,
        Duration::from_secs(15),
    ) {
        Ok(out) => out,
        Err(ProcError::Spawn(_)) => {
            return json!({"ok": false, "error": "docker CLI not available"});
        }
        Err(ProcError::Timeout) => {
            return json!({"ok": false, "error": "docker compose ps timed out after 15s"});
        }
    };
    if out.code != 0 {
        return json!({
            "ok": false,
            "exit_code": out.code,
            "stderr": py_slice_chars(out.stderr.trim(), 1000),
        });
    }
    let mut containers: Vec<Value> = Vec::new();
    for line in py_splitlines(&out.stdout) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            containers.push(v);
        }
    }
    json!({"ok": true, "count": containers.len(), "containers": containers})
}

/// server.py `tool_app_log_tail`.
pub fn tool_app_log_tail(ctx: &Ctx, limit: i64, date: Option<&str>) -> Value {
    let log_dir = ctx.logs_dir();
    let date = match date {
        Some(d) => d.to_string(),
        None => chrono::Utc::now().format("%Y-%m-%d").to_string(),
    };
    let log_file = log_dir.join(format!("app.{date}.log"));
    if !log_file.exists() {
        return json!({
            "ok": false,
            "error": format!("log file not found: {}", log_file.to_string_lossy()),
            "log_dir": log_dir.to_string_lossy(),
        });
    }
    let bytes = match std::fs::read(&log_file) {
        Ok(b) => b,
        Err(err) => {
            return json!({
                "ok": false,
                "error": err.to_string(),
                "path": log_file.to_string_lossy(),
            });
        }
    };
    let text = py_textmode(&decode_utf8_replace(&bytes));
    let lines = py_file_lines(&text);
    let tail: &[&str] = if limit > 0 {
        py_neg_slice(&lines, limit)
    } else {
        &lines
    };
    json!({
        "ok": true,
        "path": log_file.to_string_lossy(),
        "total_lines": lines.len(),
        "returned": tail.len(),
        "lines": tail,
    })
}

// ---------------------------------------------------------------------------
// grep_codebase
// ---------------------------------------------------------------------------

const GREP_SKIP_DIRS: [&str; 5] = ["target", ".git", "node_modules", "data", ".terraform"];

#[allow(clippy::too_many_arguments)]
fn grep_walk(
    dir: &Path,
    root: &Path,
    regex: &regex::Regex,
    file_glob: Option<&str>,
    max_matches: usize,
    matches: &mut Vec<Value>,
) {
    if matches.len() >= max_matches {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if !GREP_SKIP_DIRS.contains(&name.as_str()) {
                subdirs.push(path);
            }
            continue;
        }
        if let Some(glob) = file_glob
            && !py_fnmatch(&name, glob)
        {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > 2_000_000 {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = py_textmode(&decode_utf8_ignore(&bytes));
        for (idx, line) in py_file_lines(&text).iter().enumerate() {
            if regex.is_match(line) {
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| path.to_string_lossy().into_owned());
                matches.push(json!({
                    "file": rel,
                    "line": idx + 1,
                    "text": py_slice_chars(line, 500),
                }));
                if matches.len() >= max_matches {
                    break;
                }
            }
        }
        // CPython quirk kept faithfully: at/above the cap, remaining FILES
        // in this directory are still visited (each can add one more match
        // — the per-line check runs only AFTER an append); only further
        // DIRECTORIES stop (the fn-entry guard mirrors os.walk's
        // outer-loop break).
    }
    for sub in subdirs {
        grep_walk(&sub, root, regex, file_glob, max_matches, matches);
        if matches.len() >= max_matches {
            return;
        }
    }
}

/// server.py `tool_grep_codebase` (max_matches fixed at 200 — the Python
/// registry never passes it).
pub fn tool_grep_codebase(
    ctx: &Ctx,
    pattern: &str,
    path: Option<&str>,
    file_glob: Option<&str>,
) -> Value {
    let max_matches = 200usize;
    let root = &ctx.repo_root;
    let search_root = match path {
        Some(p) => {
            let pb = PathBuf::from(p);
            if pb.is_absolute() { pb } else { root.join(pb) }
        }
        None => root.clone(),
    };
    let regex = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(err) => {
            // Rust regex error text differs from CPython `re.error` —
            // the harness verifies the "invalid regex: " prefix on both
            // sides then masks the detail.
            return json!({
                "ok": false,
                "pattern": pattern,
                "error": format!("invalid regex: {err}"),
            });
        }
    };
    let mut matches: Vec<Value> = Vec::new();
    grep_walk(
        &search_root,
        root,
        &regex,
        file_glob,
        max_matches,
        &mut matches,
    );
    json!({
        "ok": true,
        "pattern": pattern,
        "match_count": matches.len(),
        "truncated": matches.len() >= max_matches,
        "matches": matches,
    })
}

// ---------------------------------------------------------------------------
// cloudwatch_logs — SigV4 -> portal -> aws CLI fallback chain
// ---------------------------------------------------------------------------

fn cloudwatch_log_group(env: &dyn Env) -> String {
    env.get("TICKVAULT_CLOUDWATCH_LOG_GROUP")
        .unwrap_or_else(|| "/tickvault/prod/app".to_string())
}

fn aws_region(env: &dyn Env) -> String {
    env_truthy(env, "AWS_REGION")
        .or_else(|| env_truthy(env, "AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "ap-south-1".to_string())
}

fn aws_credentials(env: &dyn Env) -> (String, String, Option<String>) {
    (
        env.get("AWS_ACCESS_KEY_ID").unwrap_or_default(),
        env.get("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
        env_truthy(env, "AWS_SESSION_TOKEN"),
    )
}

fn portal_url(env: &dyn Env) -> String {
    env.get("TICKVAULT_PORTAL_URL")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

fn portal_token(env: &dyn Env) -> String {
    env.get("TICKVAULT_PORTAL_TOKEN").unwrap_or_default()
}

fn cloudwatch_via_sigv4(
    env: &dyn Env,
    minutes: i64,
    filter_pattern: Option<&str>,
    limit: i64,
) -> Value {
    let region = aws_region(env);
    let group = cloudwatch_log_group(env);
    // Python: re.fullmatch(r"[a-z0-9-]+", region)
    let region_ok = !region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !region_ok {
        return json!({
            "ok": false,
            "source": "cloudwatch_sigv4",
            "log_group": group,
            "error": "invalid AWS region — set AWS_DEFAULT_REGION to a region like ap-south-1 (lowercase letters, digits, hyphens only).",
        });
    }
    let (access_key, secret_key, session_token) = aws_credentials(env);
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let start_ms = ((now_unix - (minutes.max(1) as f64) * 60.0) * 1000.0) as i64;
    let amz = AmzNow::from_utc(chrono::Utc::now());
    let (url, body, headers) = build_cloudwatch_sigv4_request(
        &region,
        &group,
        start_ms,
        limit,
        filter_pattern,
        &access_key,
        &secret_key,
        session_token.as_deref(),
        &amz,
    );
    let client = match http_client(30) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "ok": false,
                "source": "cloudwatch_sigv4",
                "log_group": group,
                "region": region,
                "error": format!("CloudWatch FilterLogEvents failed: {e}"),
            });
        }
    };
    let mut req = client.post(&url).body(body);
    for (name, value) in &headers {
        req = req.header(name, value);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            // Exception TYPE name differs from CPython — bounded + masked;
            // never echoes any header or the secret.
            let text = e.to_string();
            return json!({
                "ok": false,
                "source": "cloudwatch_sigv4",
                "log_group": group,
                "region": region,
                "error": format!(
                    "CloudWatch FilterLogEvents failed: reqwest::Error: {}",
                    py_slice_chars(&text, 300)
                ),
            });
        }
    };
    let status = resp.status();
    if status.as_u16() >= 400 {
        let reason = status.canonical_reason().unwrap_or("").to_string();
        let err_body = resp
            .bytes()
            .map(|b| py_slice_chars(&decode_utf8_replace(&b), 400).to_string())
            .unwrap_or_default();
        let detail = if err_body.is_empty() {
            reason
        } else {
            err_body
        };
        return json!({
            "ok": false,
            "source": "cloudwatch_sigv4",
            "log_group": group,
            "region": region,
            "error": format!("CloudWatch FilterLogEvents HTTP {}: {}", status.as_u16(), detail),
        });
    }
    let payload = resp
        .bytes()
        .map(|b| decode_utf8_replace(&b))
        .unwrap_or_default();
    let events = parse_cloudwatch_events(&payload, limit);
    json!({
        "ok": true,
        "source": "cloudwatch_sigv4",
        "log_group": group,
        "region": region,
        "lookback_minutes": minutes,
        "filter_pattern": filter_pattern.unwrap_or(""),
        "returned": events.len(),
        "events": events,
    })
}

fn cloudwatch_via_portal(env: &dyn Env, filter_pattern: Option<&str>, limit: i64) -> Value {
    let url = portal_url(env);
    let token = portal_token(env);
    if !url.starts_with("https://") {
        return json!({
            "ok": false,
            "source": "portal",
            "portal_url": url,
            "error": "TICKVAULT_PORTAL_URL must use https:// (refusing to send the bearer token over plaintext).",
        });
    }
    let client = match http_client(30) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "ok": false,
                "source": "portal",
                "portal_url": url,
                "error": format!("portal logs call failed: {e}"),
            });
        }
    };
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body("{\"action\": \"logs\"}")
        .send();
    let parsed: Value = match resp {
        Err(e) => {
            return json!({
                "ok": false,
                "source": "portal",
                "portal_url": url,
                "error": format!("portal logs call failed: reqwest::Error: {e}"),
            });
        }
        Ok(r) => {
            let status = r.status();
            if status.as_u16() >= 400 {
                let reason = status.canonical_reason().unwrap_or("");
                return json!({
                    "ok": false,
                    "source": "portal",
                    "portal_url": url,
                    "error": format!(
                        "portal logs call failed: HTTPError: HTTP Error {}: {}",
                        status.as_u16(),
                        reason
                    ),
                });
            }
            let body = r
                .bytes()
                .map(|b| decode_utf8_replace(&b))
                .unwrap_or_default();
            if body.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                match serde_json::from_str::<Value>(&body) {
                    Ok(Value::Object(o)) => Value::Object(o),
                    Ok(_) => Value::Object(Map::new()),
                    Err(e) => {
                        return json!({
                            "ok": false,
                            "source": "portal",
                            "portal_url": url,
                            "error": format!("portal logs call failed: JSONDecodeError: {e}"),
                        });
                    }
                }
            }
        }
    };
    let ok = parsed.get("ok").map(python_truthy).unwrap_or(false);
    if !ok {
        return json!({
            "ok": false,
            "source": "portal",
            "portal_url": url,
            "error": "portal returned ok=false (check the bearer token / box state)",
            "portal_response": py_slice_chars(&parsed.to_string(), 400),
        });
    }
    let raw = parsed.get("raw").and_then(Value::as_str).unwrap_or("");
    let events = filter_and_trim_portal_events(parse_portal_logs_raw(raw), filter_pattern, limit);
    json!({
        "ok": true,
        "source": "portal",
        "portal_url": url,
        "filter_pattern": filter_pattern.unwrap_or(""),
        "returned": events.len(),
        "events": events,
        "note": "live journalctl err+app tail via the operator portal (no aws CLI / no AWS key needed)",
    })
}

fn python_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// server.py `tool_cloudwatch_logs` — SigV4 -> portal -> aws CLI.
pub fn tool_cloudwatch_logs(
    ctx: &Ctx,
    minutes: i64,
    filter_pattern: Option<&str>,
    limit: i64,
) -> Value {
    let env = RealEnv;
    let (access_key, secret_key, _session_token) = aws_credentials(&env);
    if !access_key.is_empty() && !secret_key.is_empty() {
        return cloudwatch_via_sigv4(&env, minutes, filter_pattern, limit);
    }
    if !portal_url(&env).is_empty() && !portal_token(&env).is_empty() {
        return cloudwatch_via_portal(&env, filter_pattern, limit);
    }
    let region = aws_region(&env);
    let group = cloudwatch_log_group(&env);
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let start_ms = ((now_unix - (minutes.max(1) as f64) * 60.0) * 1000.0) as i64;
    let argv = build_cloudwatch_filter_args(&group, &region, start_ms, limit, filter_pattern);
    let out = match run_with_timeout(
        &argv[0],
        &argv[1..],
        &ctx.repo_root,
        Duration::from_secs(30),
    ) {
        Ok(out) => out,
        Err(ProcError::Spawn(_)) => {
            return json!({
                "ok": false,
                "error": "no log reader configured — set AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY (+ AWS_DEFAULT_REGION) for the direct SigV4 path (no aws CLI, no portal), OR set TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN to read via the operator dashboard, OR wire a read-only AWS credential + aws CLI. None is present in this session.",
                "log_group": group,
            });
        }
        Err(ProcError::Timeout) => {
            return json!({"ok": false, "error": "aws logs filter-log-events timed out after 30s"});
        }
    };
    if out.code != 0 {
        return json!({
            "ok": false,
            "exit_code": out.code,
            "error": "aws logs call failed — most likely no read-only AWS credentials in this environment yet. Prefer the direct SigV4 path (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY) or the portal path (TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN). stderr below.",
            "stderr": py_slice_chars(out.stderr.trim(), 800),
            "log_group": group,
        });
    }
    let events = parse_cloudwatch_events(&out.stdout, limit);
    json!({
        "ok": true,
        "source": "aws_cli",
        "log_group": group,
        "region": region,
        "lookback_minutes": minutes,
        "filter_pattern": filter_pattern.unwrap_or(""),
        "returned": events.len(),
        "events": events,
    })
}

// ---------------------------------------------------------------------------
// Registry — names / descriptions / schemas byte-identical to server.py
// ---------------------------------------------------------------------------

pub const TOOL_NAMES: [&str; 14] = [
    "tail_errors",
    "list_novel_signatures",
    "summary_snapshot",
    "triage_log_tail",
    "signature_history",
    "find_runbook_for_code",
    "questdb_sql",
    "grep_codebase",
    "run_doctor",
    "git_recent_log",
    "tickvault_api",
    "docker_status",
    "app_log_tail",
    "cloudwatch_logs",
];

const DESC_TAIL_ERRORS: &str = "Return the last N ERROR events from data/logs/machine/errors.jsonl.*. Optionally filter by `code` (e.g. 'I-P1-11', 'DH-904').";
const DESC_NOVEL: &str = "Signatures first observed within the last N minutes. Uses the same FNV-1a signature hash as the Rust summary_writer.";
const DESC_SUMMARY: &str = "Return the current errors.summary.md markdown. Regenerated every 60s by the Rust summary_writer task.";
const DESC_TRIAGE: &str =
    "Last N lines of data/logs/auto-fix.log — audit trail of the error-triage hook's actions.";
const DESC_SIG_HISTORY: &str = "All events whose computed signature hash equals `signature`. Use after list_novel_signatures to drill into a specific signature's full history.";
const DESC_RUNBOOK: &str = "Given an ErrorCode string (e.g. 'DH-904', 'I-P1-11'), search every file under docs/runbooks/ AND .claude/rules/ for the code and return matching file paths + a preview of the relevant section. Lets Claude jump from a Telegram alert to the operator runbook in one tool call.";
const DESC_QUESTDB: &str = "Run arbitrary SQL against the local QuestDB (HTTP /exec). Returns columnar JSON. Lets Claude query the trading data plane — ticks, orders, historical_candles, materialized views — without pg CLI.";
const DESC_GREP: &str = "Regex-search the repo for a pattern. Skips target/, .git/, node_modules/, data/, .terraform/. Returns up to 200 matches of {file, line, text}. Lets Claude find any function, error string, config key, test name across the workspace in one call.";
const DESC_DOCTOR: &str = "Invoke `bash scripts/doctor.sh` and return the parsed output as {pass_count, fail_count, rows[{status, detail}]}. One-call total-system health check for Claude.";
const DESC_GIT: &str = "Return the last N commits on the current branch with sha/author/date/subject. Lets Claude investigate 'what changed recently' without leaving the MCP surface.";
const DESC_API: &str = "HTTP GET against the tickvault app's own REST API (port 3001 by default). Paths: /health, /api/stats, /api/quote/{security_id}, /api/instruments/diagnostic, /api/option-chain, /api/pcr, /api/index-constituency. Returns status + json (or text).";
const DESC_DOCKER: &str = "Return `docker compose ps --format json` for the tickvault stack. Gives Claude container name/service/state/health without shelling into the host.";
const DESC_APP_LOG: &str = "Last N lines of data/logs/app.YYYY-MM-DD.log (full INFO/DEBUG output, not just ERRORs). Optional `date` (YYYY-MM-DD) picks a different day; defaults to today UTC.";
const DESC_CLOUDWATCH: &str = "Read recent PROD logs — fully automated, no human paste/download. PREFERRED: direct CloudWatch read via a SigV4-signed HTTPS request — the ONLY input is a read-only AWS key in the env (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY [+ AWS_SESSION_TOKEN], AWS_DEFAULT_REGION); no aws CLI, no portal, no boto3. NEXT: the operator-control dashboard's logs endpoint (TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN). FALLBACK: read-only aws CLI on /tickvault/prod/app. Args: minutes (lookback; SigV4 + aws paths), filter_pattern (CloudWatch filter on SigV4 + aws, substring on portal, e.g. \"ERROR\" or \"WS-GAP-05\"), limit (default 100). The AWS secret/token is NEVER logged nor returned. Clear ok=false error if no path is configured.";

/// (name, description) pairs in registry order — used by the self-test.
pub fn tool_descriptions() -> Vec<(&'static str, &'static str)> {
    vec![
        ("tail_errors", DESC_TAIL_ERRORS),
        ("list_novel_signatures", DESC_NOVEL),
        ("summary_snapshot", DESC_SUMMARY),
        ("triage_log_tail", DESC_TRIAGE),
        ("signature_history", DESC_SIG_HISTORY),
        ("find_runbook_for_code", DESC_RUNBOOK),
        ("questdb_sql", DESC_QUESTDB),
        ("grep_codebase", DESC_GREP),
        ("run_doctor", DESC_DOCTOR),
        ("git_recent_log", DESC_GIT),
        ("tickvault_api", DESC_API),
        ("docker_status", DESC_DOCKER),
        ("app_log_tail", DESC_APP_LOG),
        ("cloudwatch_logs", DESC_CLOUDWATCH),
    ]
}

/// The `tools/list` result array — same names, descriptions and
/// inputSchema VALUES as server.py's TOOLS registry (object key order is
/// normalized by the harness; array order — incl. `required` — preserved).
pub fn tools_list_json() -> Value {
    json!([
        {
            "name": "tail_errors",
            "description": DESC_TAIL_ERRORS,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max events to return (default 100)",
                        "default": 100,
                    },
                    "code": {
                        "type": "string",
                        "description": "Optional ErrorCode.code_str() filter",
                    },
                },
            },
        },
        {
            "name": "list_novel_signatures",
            "description": DESC_NOVEL,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since_minutes": {
                        "type": "integer",
                        "description": "Lookback window in minutes (default 60)",
                        "default": 60,
                    },
                },
            },
        },
        {
            "name": "summary_snapshot",
            "description": DESC_SUMMARY,
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "triage_log_tail",
            "description": DESC_TRIAGE,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max lines to return (default 50)",
                        "default": 50,
                    },
                },
            },
        },
        {
            "name": "signature_history",
            "description": DESC_SIG_HISTORY,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "signature": {
                        "type": "string",
                        "description": "16-hex-char signature hash",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max events to return (default 500)",
                        "default": 500,
                    },
                },
                "required": ["signature"],
            },
        },
        {
            "name": "find_runbook_for_code",
            "description": DESC_RUNBOOK,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "ErrorCode.code_str() e.g. 'DH-904', 'OMS-GAP-03'",
                    },
                },
                "required": ["code"],
            },
        },
        {
            "name": "questdb_sql",
            "description": DESC_QUESTDB,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "SQL query (e.g. 'SELECT count() FROM ticks')",
                    },
                },
                "required": ["query"],
            },
        },
        {
            "name": "grep_codebase",
            "description": DESC_GREP,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Python regex (anchors, character classes supported)",
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional relative path to narrow the search",
                    },
                    "file_glob": {
                        "type": "string",
                        "description": "Optional glob like '*.rs' to limit file types",
                    },
                },
                "required": ["pattern"],
            },
        },
        {
            "name": "run_doctor",
            "description": DESC_DOCTOR,
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "git_recent_log",
            "description": DESC_GIT,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Number of commits (default 20)",
                        "default": 20,
                    },
                },
            },
        },
        {
            "name": "tickvault_api",
            "description": DESC_API,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "API path starting with /",
                    },
                    "base_url": {
                        "type": "string",
                        "description": "Override TICKVAULT_API_URL for a single call.",
                    },
                },
                "required": ["path"],
            },
        },
        {
            "name": "docker_status",
            "description": DESC_DOCKER,
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "app_log_tail",
            "description": DESC_APP_LOG,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max lines (default 100)",
                        "default": 100,
                    },
                    "date": {
                        "type": "string",
                        "description": "YYYY-MM-DD (default: today UTC)",
                    },
                },
            },
        },
        {
            "name": "cloudwatch_logs",
            "description": DESC_CLOUDWATCH,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "minutes": {
                        "type": "integer",
                        "description": "Lookback window in minutes (default 60)",
                        "default": 60,
                    },
                    "filter_pattern": {
                        "type": "string",
                        "description": "Optional CloudWatch filter pattern (e.g. ERROR)",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max events (default 100)",
                        "default": 100,
                    },
                },
            },
        },
    ])
}

// ---------------------------------------------------------------------------
// Dispatch — mirrors the Python registry lambdas, incl. their `int()`
// coercions and `args[...]` KeyError messages.
// ---------------------------------------------------------------------------

fn get_int(args: &Map<String, Value>, key: &str, default: i64) -> Result<i64, String> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => py_int(v),
    }
}

fn get_opt_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        // Null == Python None == "not provided" for these optional args;
        // non-string values are a documented deviation (Python would pass
        // them through and TypeError later).
        _ => None,
    }
}

fn require(args: &Map<String, Value>, key: &str) -> Result<Value, String> {
    args.get(key).cloned().ok_or_else(|| format!("'{key}'"))
}

fn require_str(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    match require(args, key)? {
        Value::String(s) => Ok(s),
        other => Err(format!(
            "argument `{key}` must be a string, got {other} (Rust deviation: Python raises TypeError later)"
        )),
    }
}

/// `tools/call` dispatch. `Err(msg)` maps to the JSON-RPC -32000
/// `tool {name} failed: {msg}` error, exactly like the Python `except`.
pub fn call_tool(
    ctx: &Ctx,
    name: &str,
    args: &Map<String, Value>,
) -> Option<Result<Value, String>> {
    let out = match name {
        "tail_errors" => {
            let limit = match get_int(args, "limit", 100) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let code = get_opt_str(args, "code");
            Ok(tool_tail_errors(ctx, limit, code.as_deref()))
        }
        "list_novel_signatures" => match get_int(args, "since_minutes", 60) {
            Ok(v) => Ok(tool_list_novel_signatures(ctx, v)),
            Err(e) => Err(e),
        },
        "summary_snapshot" => Ok(tool_summary_snapshot(ctx)),
        "triage_log_tail" => match get_int(args, "limit", 50) {
            Ok(v) => Ok(tool_triage_log_tail(ctx, v)),
            Err(e) => Err(e),
        },
        "signature_history" => {
            let signature = match require(args, "signature") {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            match get_int(args, "limit", 500) {
                Ok(limit) => Ok(tool_signature_history(ctx, &signature, limit)),
                Err(e) => Err(e),
            }
        }
        "find_runbook_for_code" => match require_str(args, "code") {
            Ok(code) => Ok(tool_find_runbook_for_code(ctx, &code)),
            Err(e) => Err(e),
        },
        "questdb_sql" => match require_str(args, "query") {
            Ok(query) => Ok(tool_questdb_sql(ctx, &query)),
            Err(e) => Err(e),
        },
        "grep_codebase" => match require_str(args, "pattern") {
            Ok(pattern) => {
                let path = get_opt_str(args, "path");
                let file_glob = get_opt_str(args, "file_glob");
                Ok(tool_grep_codebase(
                    ctx,
                    &pattern,
                    path.as_deref(),
                    file_glob.as_deref(),
                ))
            }
            Err(e) => Err(e),
        },
        "run_doctor" => Ok(tool_run_doctor(ctx)),
        "git_recent_log" => match get_int(args, "limit", 20) {
            Ok(v) => Ok(tool_git_recent_log(ctx, v)),
            Err(e) => Err(e),
        },
        "tickvault_api" => match require_str(args, "path") {
            Ok(path) => {
                let base_url = get_opt_str(args, "base_url");
                Ok(tool_tickvault_api(ctx, &path, base_url.as_deref()))
            }
            Err(e) => Err(e),
        },
        "docker_status" => Ok(tool_docker_status(ctx)),
        "app_log_tail" => {
            let limit = match get_int(args, "limit", 100) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let date = get_opt_str(args, "date");
            Ok(tool_app_log_tail(ctx, limit, date.as_deref()))
        }
        "cloudwatch_logs" => {
            let minutes = match get_int(args, "minutes", 60) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let limit = match get_int(args, "limit", 100) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let filter_pattern = get_opt_str(args, "filter_pattern");
            Ok(tool_cloudwatch_logs(
                ctx,
                minutes,
                filter_pattern.as_deref(),
                limit,
            ))
        }
        _ => return None,
    };
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_match_registry_order() {
        let list = tools_list_json();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 14);
        for (i, name) in TOOL_NAMES.iter().enumerate() {
            assert_eq!(arr[i]["name"], *name);
        }
        let descs = tool_descriptions();
        assert_eq!(descs.len(), 14);
        for (i, (name, desc)) in descs.iter().enumerate() {
            assert_eq!(arr[i]["name"], *name);
            assert_eq!(arr[i]["description"], *desc);
        }
    }

    #[test]
    fn py_neg_slice_matches_python_semantics() {
        let v = [1, 2, 3, 4, 5];
        assert_eq!(py_neg_slice(&v, 2), &[4, 5]);
        assert_eq!(py_neg_slice(&v, 10), &v);
        assert_eq!(py_neg_slice(&v, 0), &v); // lines[-0:] == lines[0:]
        assert_eq!(py_neg_slice(&v, -2), &[3, 4, 5]); // lines[2:]
    }

    #[test]
    fn py_file_lines_matches_python_iteration() {
        assert_eq!(py_file_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(py_file_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(py_file_lines(""), Vec::<&str>::new());
        assert_eq!(py_file_lines("\n"), vec![""]);
    }

    #[test]
    fn doctor_parser_counts_and_strips() {
        let stdout =
            "[PASS] questdb reachable  \n[FAIL] api offline\nnoise\n[WARN]  disk 81%\n[SKIP] aws\n";
        let (rows, pass, fail) = parse_doctor_stdout(stdout);
        assert_eq!(pass, 1);
        assert_eq!(fail, 1);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            json!({"status": "PASS", "detail": "questdb reachable"})
        );
        assert_eq!(rows[2], json!({"status": "WARN", "detail": "disk 81%"}));
    }

    #[test]
    fn git_parser_splits_on_unit_separator() {
        let stdout =
            "abcdef0123456789\u{1f}Alice\u{1f}2026-07-18T00:00:00+05:30\u{1f}feat: x\nbad-line\n";
        let commits = parse_git_log_stdout(stdout);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["sha"], "abcdef012345");
        assert_eq!(commits[0]["subject"], "feat: x");
    }

    #[test]
    fn split_scheme_host_cases() {
        assert_eq!(
            split_scheme_host("http://EXAMPLE.com:8080/x"),
            ("http".into(), Some("example.com".into()))
        );
        assert_eq!(
            split_scheme_host("https://[::1]:9/x"),
            ("https".into(), Some("::1".into()))
        );
        assert_eq!(split_scheme_host("notaurl"), (String::new(), None));
        assert_eq!(
            split_scheme_host("http://user@h/x"),
            ("http".into(), Some("h".into()))
        );
    }

    #[test]
    fn python_truthy_matches() {
        assert!(!python_truthy(&json!(null)));
        assert!(!python_truthy(&json!(false)));
        assert!(!python_truthy(&json!(0)));
        assert!(!python_truthy(&json!("")));
        assert!(python_truthy(&json!("x")));
        assert!(python_truthy(&json!(1)));
        assert!(python_truthy(&json!({"a": 1})));
    }

    #[test]
    fn py_isoformat_matches_python() {
        let dt = chrono::DateTime::parse_from_rfc3339("2026-07-18T05:30:00+00:00").unwrap();
        assert_eq!(py_isoformat(&dt), "2026-07-18T05:30:00+00:00");
        let dt2 = chrono::DateTime::parse_from_rfc3339("2026-07-18T05:30:00.123456+05:30").unwrap();
        assert_eq!(py_isoformat(&dt2), "2026-07-18T05:30:00.123456+05:30");
    }

    #[test]
    fn iter_errors_jsonl_files_grace_merge_prefers_machine_copy() {
        let base = std::env::temp_dir().join(format!("tv-mcp-iter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let machine = base.join("machine");
        std::fs::create_dir_all(&machine).unwrap();
        std::fs::write(base.join("errors.jsonl.2026-07-18-05"), "legacy").unwrap();
        std::fs::write(machine.join("errors.jsonl.2026-07-18-05"), "machine").unwrap();
        std::fs::write(machine.join("errors.jsonl.2026-07-18-06"), "m6").unwrap();
        std::fs::write(base.join("errors.jsonl.2026-07-18-04"), "l4").unwrap();
        std::fs::write(base.join("unrelated.log"), "x").unwrap();
        let files = iter_errors_jsonl_files(&machine);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "errors.jsonl.2026-07-18-06",
                "errors.jsonl.2026-07-18-05",
                "errors.jsonl.2026-07-18-04",
            ]
        );
        // The duplicate name resolves to the machine copy.
        let dup = files.iter().find(|(n, _)| n.ends_with("-05")).unwrap();
        assert_eq!(std::fs::read_to_string(&dup.1).unwrap(), "machine");
        let _ = std::fs::remove_dir_all(&base);
    }
}
