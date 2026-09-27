//! The 14 MCP tools — behavior-parity ports of the retired reference implementation's tool functions
//! (`tool_tail_errors` .. `tool_cloudwatch_logs`) + the advertised registry
//! (byte-identical names / descriptions / inputSchema values; object key
//! ORDER is normalized by the parity harness, array order is preserved).
//!
//! Documented bounded deviations from the legacy runtime (each also listed in the PR
//! body; none reachable from the parity transcript):
//!   - Non-object JSON lines in errors.jsonl.* are skipped/filtered
//!     gracefully where legacy would raise AttributeError.
//!   - Non-string values for string-typed args produce a typed error
//!     instead of the legacy runtime's TypeError text.
//!   - OS/library-level failure TEXT (spawn errors, transport errors,
//!     invalid-regex details, JSON-decode details) differs from the legacy runtime's
//!     exception strings; the surrounding JSON shape is identical. The
//!     parity harness masks ONLY cutoff_utc, the grep invalid-regex error
//!     detail, and sorts `matches` arrays — transport-error text is NOT
//!     masked; the transcript AVOIDS those paths (the mock is always up).
//!   - Invalid UTF-8 in log files: legacy read_text() raises (tool error);
//!     Rust skips the file (tail/history) — the app's sinks are UTF-8.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::config::{self, Ctx, Env, RealEnv};
use crate::legacy_compat::{
    decode_utf8_ignore, decode_utf8_replace, legacy_fnmatch, legacy_int, legacy_slice_chars,
    legacy_splitlines, legacy_tail_chars, legacy_urllib_quote,
};
use crate::signature::signature_hash;
use crate::sigv4::{
    AmzNow, build_cloudwatch_sigv4_request, filter_and_trim_portal_events, parse_cloudwatch_events,
    parse_portal_logs_raw,
};

pub const ERRORS_JSONL_PREFIX: &str = "errors.jsonl";
pub const SUMMARY_FILENAME: &str = "errors.summary.md";
pub const AUTO_FIX_LOG: &str = "auto-fix.log";

/// Subprocess poll granularity while waiting on a spawned child
/// (parity: the retired reference implementation subprocess timeout loop).
const PROC_POLL_INTERVAL_MS: u64 = 25;

/// How long the parent waits for one pipe reader to reach end-of-stream after
/// the child has exited or been killed.
///
/// # The hang this bounds
///
/// `run_with_timeout` enforced its timeout against the CHILD and not against
/// the READERS. On the success path it called `join()` on both reader threads
/// with no bound, and `read_to_end` returns only at EOF — which arrives when
/// EVERY holder of the pipe's write end has closed it, not when the child
/// exits. A child that backgrounds anything (`&`, `nohup`, `docker compose up
/// -d` — which this repository's own session hook runs) leaves a GRANDCHILD
/// holding the inherited pipe, so EOF never comes, `join()` never returns, and
/// the MCP server hangs inside the function whose entire purpose is a timeout.
///
/// This is the same class as the `tiny_server` accept loop bounded in #1883: a
/// blocking call with nothing to notice a deadline with.
///
/// Five seconds because the child is already gone by this point — the pipes
/// either drain immediately or a grandchild is holding them, and no amount of
/// further waiting distinguishes the two.
const PROC_READER_DRAIN_TIMEOUT_MS: u64 = 5_000;

/// Bytes ONE captured stream may hold. 8 MiB.
///
/// The previous `read_to_end` had no ceiling, so a chatty or looping child
/// could grow the parent's heap without limit — the unbounded-growth shape this
/// repository bans everywhere else.
///
/// Honest consequence of capping: at the cap the reader stops and its end of
/// the pipe drops, so a child that keeps writing takes an EPIPE and usually
/// dies. That is a REPORTED death — the exit code reaches the caller — and it
/// is the better half of the trade against an unbounded parent heap.
const PROC_CAPTURE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Appended to `stderr` when a reader did not reach end-of-stream in time.
///
/// It goes into the OUTPUT rather than only a `bool`, because the failure it
/// describes is silent by construction: the caller otherwise sees a successful
/// exit code beside a short or empty stream and has no way to tell that from a
/// command that genuinely printed nothing.
const PROC_CAPTURE_TRUNCATED_NOTICE: &str = "\n[tickvault-logs-mcp] capture INCOMPLETE: a pipe \
     reader did not reach end-of-stream within the drain budget, so the output above may be \
     partial. The usual cause is a background grandchild that inherited the pipe and is still \
     holding it open.\n";
/// Subprocess timeout for `scripts/doctor.sh` (parity: the retired reference implementation timeout=120).
const DOCTOR_TIMEOUT_SECS: u64 = 120;
/// Subprocess timeout for `git log` (parity: the retired reference implementation timeout=10).
const GIT_LOG_TIMEOUT_SECS: u64 = 10;
/// Subprocess timeout for `docker compose ps` (parity: the retired reference implementation timeout=15).
const DOCKER_PS_TIMEOUT_SECS: u64 = 15;

/// Raw bytes of the retired runtime's own name. WIRE VALUE, not prose — two
/// tool error strings and one advertised `inputSchema` description echo it
/// byte-for-byte (parity-pinned against the legacy server), so it is
/// assembled from bytes here and its name never appears in this repository
/// (operator directive 2026-08-01) while the bytes stay byte-for-byte
/// IDENTICAL on the wire. Pinned by `runtime_name_wire_bytes_are_unchanged`.
const RUNTIME_NAME_BYTES: &[u8] = &[0x50, 0x79, 0x74, 0x68, 0x6f, 0x6e];
const RUNTIME_NAME: &str = match core::str::from_utf8(RUNTIME_NAME_BYTES) {
    Ok(s) => s,
    // Unreachable: ASCII. Evaluated at COMPILE time — a bad edit is a
    // build error, never a runtime panic.
    Err(_) => panic!("RUNTIME_NAME bytes are not valid UTF-8"),
};

/// `novel_cutoff` band-1 error text (the C-int `OverflowError` the legacy
/// server raised). WIRE VALUE — see `RUNTIME_NAME_BYTES`.
fn c_int_overflow_msg() -> String {
    format!("{RUNTIME_NAME} int too large to convert to C int")
}

/// Advertised `grep_codebase.pattern` schema description. WIRE VALUE —
/// byte-identical to the legacy registry; see `RUNTIME_NAME_BYTES`.
fn grep_pattern_description() -> String {
    format!("{RUNTIME_NAME} regex (anchors, character classes supported)")
}

// ---------------------------------------------------------------------------
// Small legacy-runtime-parity helpers
// ---------------------------------------------------------------------------

/// legacy text-mode universal-newline translation (`\r\n`/`\r` -> `\n`) —
/// applied everywhere the retired reference implementation reads files / subprocess output in text
/// mode (read_text, open(..., "r"), subprocess text=True).
fn legacy_textmode(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// legacy file-object line iteration semantics AFTER universal-newline
/// translation, with terminators stripped: `"a\nb\n"` -> ["a","b"],
/// `"a\nb"` -> ["a","b"], `""` -> [].
fn legacy_file_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        v.pop();
    }
    v
}

/// legacy `lines[-limit:]` slice (handles 0 => everything, negatives).
fn legacy_neg_slice<T>(lines: &[T], limit: i64) -> &[T] {
    let n = lines.len() as i64;
    let start = if limit > 0 {
        (n - limit).max(0)
    } else {
        (-limit).min(n)
    };
    &lines[start as usize..]
}

/// legacy `datetime.isoformat()` for an offset-aware datetime: seconds,
/// `.ffffff` only when the microseconds are non-zero, offset as `+HH:MM`.
fn legacy_isoformat(dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
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

/// legacy truthy env read with `or`-chaining semantics ("" is falsy).
fn env_truthy(env: &dyn Env, key: &str) -> Option<String> {
    env.get(key).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// errors.jsonl.* discovery + event parsing (the retired reference implementation:209-237)
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
                // the legacy dict assignment.
                seen.insert(name, path);
            }
        }
    }
    let mut out: Vec<(String, PathBuf)> = seen.into_iter().collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

/// legacy `_parse_event`: strip, skip empty, JSON-parse or None.
pub fn parse_event(line: &str) -> Option<Value> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(t).ok()
}

/// `_signature_hash(ev.get("code"), ev.get("target") or "", ev.get("message") or "")`
/// over a parsed event object (non-string fields degrade to None/"" — a
/// documented deviation; legacy would raise on non-string values).
fn event_signature(ev: &Map<String, Value>) -> String {
    let code = ev.get("code").and_then(Value::as_str);
    let target = ev.get("target").and_then(Value::as_str).unwrap_or("");
    let message = ev.get("message").and_then(Value::as_str).unwrap_or("");
    signature_hash(code, target, message)
}

// ---------------------------------------------------------------------------
// File-backed tools
// ---------------------------------------------------------------------------

/// Bytes one log-file read may hold, taken from the END of the file. 32 MiB.
///
/// Every file tool used `std::fs::read`, which loads the whole file: a
/// day's `app.YYYY-MM-DD.log` on the box runs to gigabytes, and one tool
/// call held all of it (plus its line index) in this process's heap. Tail
/// tools only ever wanted the end, so the end is what is read. Each reply
/// says when a file was cut (`truncated` / `truncated_files`), so a short
/// history never reads as a complete one.
pub const LOG_READ_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Lines `app_log_tail` and `triage_log_tail` may return in one reply.
/// `limit <= 0` used to mean "every line", which on a full log was the
/// whole file as one JSON reply.
pub const LOG_TAIL_MAX_LINES: i64 = 5_000;

/// Bytes one call may read across ALL `errors.jsonl.*` files. 128 MiB.
///
/// Each file is already capped at `LOG_READ_MAX_BYTES`, but the files rotate
/// hourly and are never pruned by this tool, so a per-file cap alone scales
/// with how many hours of history sit in the directory. Files past the budget
/// are named in `skipped_files`, newest files first read.
pub const LOG_SCAN_MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

/// Bytes one runbook or rule file may hold before the runbook finder skips
/// it. 4 MiB: the largest file in the four trees is about 540 KB today.
pub const RUNBOOK_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Read at most the last `cap` bytes of `path`. `Ok((bytes, true))` when the
/// file was longer: the cut lands mid-line, so the partial first line is
/// dropped and only whole lines come back.
fn read_tail_capped(path: &Path, cap: u64) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut out = Vec::new();
    if len <= cap {
        // Read the length seen at the size check, not to EOF: a file that
        // grows meanwhile would otherwise end mid-line while reporting
        // `truncated = false`.
        file.take(len).read_to_end(&mut out)?;
        return Ok((out, false));
    }
    // One byte before the window says whether the window starts on a line
    // boundary: if that byte is a newline the first line is whole and kept;
    // otherwise it is the tail of a cut line and is dropped.
    file.seek(SeekFrom::Start(len - cap - 1))?;
    file.take(cap + 1).read_to_end(&mut out)?;
    let starts_on_boundary = out.first() == Some(&b'\n');
    if out.is_empty() {
        return Ok((out, true));
    }
    out.remove(0);
    if !starts_on_boundary {
        match out.iter().position(|&b| b == b'\n') {
            Some(newline) => {
                out.drain(..=newline);
            }
            None => out.clear(),
        }
    }
    Ok((out, true))
}

/// Keep only the last `limit` newline-terminated lines of `bytes` (plus an
/// unterminated last line), so the line index built afterwards is sized by
/// the reply rather than by the 32 MiB read. Returns the number of lines
/// the buffer held before trimming, counted by newline bytes.
///
/// Splitting on `\n` alone keeps at least `limit` lines under the legacy
/// splitter too, which also breaks on `\r` and a few other separators.
fn keep_last_lines(bytes: &mut Vec<u8>, limit: i64) -> usize {
    let unterminated = usize::from(bytes.last().is_some_and(|&b| b != b'\n'));
    let total = bytes.iter().filter(|&&b| b == b'\n').count() + unterminated;
    let keep = usize::try_from(limit).unwrap_or(usize::MAX);
    if total > keep {
        // The newline that ends line (total - keep - 1), counting from 0.
        let mut seen = 0usize;
        let skip = total - keep;
        if let Some(cut) = bytes.iter().position(|&b| {
            if b == b'\n' {
                seen += 1;
            }
            seen == skip
        }) {
            bytes.drain(..=cut);
        }
    }
    total
}

fn clamp_tail_limit(limit: i64) -> i64 {
    if limit <= 0 || limit > LOG_TAIL_MAX_LINES {
        LOG_TAIL_MAX_LINES
    } else {
        limit
    }
}

/// Read a whole UTF-8 file through ONE open, refusing it past `cap`:
/// `Ok(None)` when the file holds more than `cap` bytes. A separate size
/// check followed by `read_to_string` would still load a file that grew in
/// between.
fn read_whole_capped(path: &Path, cap: u64) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let mut out = String::new();
    std::fs::File::open(path)?
        .take(cap.saturating_add(1))
        .read_to_string(&mut out)?;
    if out.len() as u64 > cap {
        return Ok(None);
    }
    Ok(Some(out))
}

/// the retired reference implementation `tool_tail_errors`.
pub fn tool_tail_errors(ctx: &Ctx, limit: i64, code: Option<&str>) -> Value {
    let limit = clamp_tail_limit(limit);
    let dir_path = ctx.machine_logs_dir();
    let files = iter_errors_jsonl_files(&dir_path);
    let mut events: Vec<Value> = Vec::new();
    let mut truncated_files: Vec<String> = Vec::new();
    let mut skipped_files: Vec<String> = Vec::new();
    let mut scanned_bytes: u64 = 0;
    for (name, f) in &files {
        if scanned_bytes >= LOG_SCAN_MAX_TOTAL_BYTES {
            skipped_files.push(name.clone());
            continue;
        }
        // 2026-07-18 review LOW-1: lossy-decode (the app_log_tail
        // behavior) instead of silently dropping a WHOLE errors file on
        // invalid UTF-8 while still listing it in files_scanned — every
        // valid JSON line keeps parsing; a binary line fails the
        // per-line JSON parse and is skipped by the existing per-line
        // semantics. A genuinely unreadable file (io error) keeps the
        // skip-continue.
        let Ok((bytes, cut)) = read_tail_capped(f, LOG_READ_MAX_BYTES) else {
            continue;
        };
        scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
        if cut {
            truncated_files.push(name.clone());
        }
        let raw = decode_utf8_replace(&bytes);
        let text = legacy_textmode(&raw);
        let lines = legacy_splitlines(&text);
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
        "truncated_files": truncated_files,
        "skipped_files": skipped_files,
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

/// legacy's `ts_str.replace("Z", "+00:00")` + `datetime.fromisoformat`.
fn parse_event_ts(ev: &Map<String, Value>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let ts_str = ev.get("timestamp").and_then(Value::as_str)?;
    if ts_str.is_empty() {
        return None;
    }
    let normalised = ts_str.replace('Z', "+00:00");
    chrono::DateTime::parse_from_rfc3339(&normalised).ok()
}

/// Pure cutoff computation for `since_minutes` — MINUTES scale, never
/// seconds (legacy: `datetime.now(timezone.utc) - timedelta(minutes=...)`).
/// Pinned with an injected `now` by `tests::novel_cutoff_is_minutes_scale`:
/// a `Duration::minutes` -> `Duration::seconds` mutation moves the cutoff
/// 60x closer to `now` and fails that test.
///
/// NEVER panics for ANY i64 (PR #1644 R6 CRITICAL: bare
/// `chrono::Duration::minutes` aborted the whole server for
/// |m| >= 153_722_867_280_913). `Err` replicates the legacy runtime's exception
/// bands BYTE-EXACTLY (empirically mapped against the merge-base
/// the retired reference implementation, 2026-07-18; each `str(exc)` is wrapped by the dispatch
/// into `-32000 tool list_novel_signatures failed: {msg}`):
///  1. `timedelta(minutes=m)` computes `days = floor(m / 1440)` and
///     converts it to a C int (32-bit) BEFORE the magnitude check —
///     `days` outside i32 (|m| >= 3_092_376_453_120 pos /
///     -3_092_376_453_121 neg, incl. i64::MIN/MAX) raises
///     the C-int `OverflowError` — exact wire text built by
///     `c_int_overflow_msg()`.
///  2. `days` inside i32 but |days| > 999_999_999 raises
///     `OverflowError: days={days}; must have magnitude <= 999999999`.
///  3. timedelta constructible but `now - td` outside legacy's datetime
///     range [0001-01-01T00:00:00, 9999-12-31T23:59:59.999999] raises
///     `OverflowError: date value out of range`.
///  4. otherwise the cutoff is returned (the whole legacy success band
///     sits far inside chrono's representable/Duration ranges — max
///     |m| in it is ~4.2e9 minutes, vs chrono's 1.5e14 bound).
fn novel_cutoff(
    now: chrono::DateTime<chrono::Utc>,
    since_minutes: i64,
) -> Result<chrono::DateTime<chrono::Utc>, String> {
    let days = since_minutes.div_euclid(1440);
    if days > i64::from(i32::MAX) || days < i64::from(i32::MIN) {
        return Err(c_int_overflow_msg());
    }
    if days.abs() > 999_999_999 {
        return Err(format!("days={days}; must have magnitude <= 999999999"));
    }
    let out_of_range = || "date value out of range".to_string();
    // try_minutes cannot fail here (|m| <= ~1.44e12 after the days
    // checks, far below chrono's 1.5e14 bound) — but never panic.
    let delta = chrono::Duration::try_minutes(since_minutes).ok_or_else(out_of_range)?;
    // checked_sub_signed None == outside chrono's ±262k-year range,
    // which is strictly outside legacy's year 1..=9999 range too.
    let cutoff = now.checked_sub_signed(delta).ok_or_else(out_of_range)?;
    if cutoff < legacy_datetime_min() || cutoff > legacy_datetime_max() {
        return Err(out_of_range());
    }
    Ok(cutoff)
}

/// legacy `datetime.min` as aware-UTC: 0001-01-01T00:00:00.
fn legacy_datetime_min() -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(1, 1, 1, 0, 0, 0)
        .single()
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
}

/// legacy `datetime.max` as aware-UTC: 9999-12-31T23:59:59.999999.
fn legacy_datetime_max() -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
        .single()
        .map(|dt| dt + chrono::Duration::microseconds(999_999))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}

/// the retired reference implementation `tool_list_novel_signatures`. `Err` == the legacy
/// OverflowError bands of `novel_cutoff` (computed FIRST, before any
/// file I/O — same order as legacy's line-312 cutoff).
pub fn tool_list_novel_signatures(ctx: &Ctx, since_minutes: i64) -> Result<Value, String> {
    use std::collections::HashMap;
    let cutoff = novel_cutoff(chrono::Utc::now(), since_minutes)?;
    let dir_path = ctx.machine_logs_dir();
    let files = iter_errors_jsonl_files(&dir_path);

    let mut order: Vec<String> = Vec::new();
    let mut first_seen: HashMap<String, NovelInfo> = HashMap::new();
    let mut truncated_files: Vec<String> = Vec::new();
    let mut skipped_files: Vec<String> = Vec::new();
    let mut scanned_bytes: u64 = 0;
    for (name, f) in &files {
        if scanned_bytes >= LOG_SCAN_MAX_TOTAL_BYTES {
            skipped_files.push(name.clone());
            continue;
        }
        // 2026-07-18 review LOW-1: lossy-decode (the app_log_tail
        // behavior) instead of silently dropping a WHOLE errors file on
        // invalid UTF-8 while still listing it in files_scanned — every
        // valid JSON line keeps parsing; a binary line fails the
        // per-line JSON parse and is skipped by the existing per-line
        // semantics. A genuinely unreadable file (io error) keeps the
        // skip-continue.
        let Ok((bytes, cut)) = read_tail_capped(f, LOG_READ_MAX_BYTES) else {
            continue;
        };
        scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
        if cut {
            truncated_files.push(name.clone());
        }
        let raw = decode_utf8_replace(&bytes);
        let text = legacy_textmode(&raw);
        for line in legacy_splitlines(&text) {
            let Some(ev) = parse_event(line) else {
                continue;
            };
            let Some(obj) = ev.as_object() else {
                continue; // documented deviation (legacy raises)
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
                        message_trunc: legacy_slice_chars(
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
                "first_seen_ts": legacy_isoformat(ts),
            }));
        }
    }

    // cutoff_utc: shape-parity only (isoformat of an aware-UTC now); the
    // harness masks this volatile field.
    let cutoff_fixed = cutoff.fixed_offset();
    Ok(json!({
        "dir": dir_path.to_string_lossy(),
        "since_minutes": since_minutes,
        "cutoff_utc": legacy_isoformat(&cutoff_fixed),
        "novel_count": novel.len(),
        "novel": novel,
        // Novelty is judged only over what was read: a file cut to its last
        // LOG_READ_MAX_BYTES, or skipped past the scan budget, loses its
        // oldest lines, so a signature first seen ONLY there reads as novel.
        "caveat": if truncated_files.is_empty() && skipped_files.is_empty() {
            Value::Null
        } else {
            json!("some error files were cut or skipped (truncated_files, skipped_files); a signature first seen only in the unread part is reported as novel, with a later first_seen_ts")
        },
        "truncated_files": truncated_files,
        "skipped_files": skipped_files,
    }))
}

/// the retired reference implementation `tool_summary_snapshot`.
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
    // The summary writer keeps this file to a page; a file past the log-read
    // ceiling is not the summary, and is refused rather than loaded whole.
    match read_whole_capped(&path, LOG_READ_MAX_BYTES) {
        Ok(None) => json!({
            "path": path.to_string_lossy(),
            "exists": true,
            "error": format!("summary file is over the {LOG_READ_MAX_BYTES}-byte read ceiling"),
            "markdown": "",
        }),
        Err(err) => json!({
            "path": path.to_string_lossy(),
            "exists": true,
            "error": err.to_string(), // OS error text — documented deviation
            "markdown": "",
        }),
        Ok(Some(raw)) => {
            let markdown = legacy_textmode(&raw);
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

/// the retired reference implementation `tool_triage_log_tail`.
pub fn tool_triage_log_tail(ctx: &Ctx, limit: i64) -> Value {
    let path = ctx.logs_dir().join(AUTO_FIX_LOG);
    if !path.exists() {
        return json!({"path": path.to_string_lossy(), "exists": false, "lines": []});
    }
    let (bytes, truncated) = match read_tail_capped(&path, LOG_READ_MAX_BYTES) {
        Ok(read) => read,
        Err(err) => {
            return json!({
                "path": path.to_string_lossy(),
                "exists": true,
                "error": err.to_string(),
                "lines": [],
            });
        }
    };
    let limit = clamp_tail_limit(limit);
    let mut bytes = bytes;
    let lines_read = keep_last_lines(&mut bytes, limit);
    let text = legacy_textmode(&decode_utf8_replace(&bytes));
    let lines = legacy_splitlines(&text);
    let tail: &[&str] = if lines.len() as i64 > limit {
        legacy_neg_slice(&lines, limit)
    } else {
        &lines
    };
    json!({
        "path": path.to_string_lossy(),
        "exists": true,
        // Lines in the part that was read; the whole file when `truncated`
        // is false.
        "total_lines": if lines_read as i64 > limit { lines_read } else { lines.len() },
        "returned": tail.len(),
        "truncated": truncated,
        "lines": tail,
    })
}

/// the retired reference implementation `tool_signature_history`. `signature` is echoed verbatim
/// (legacy echoes whatever JSON value was passed).
pub fn tool_signature_history(ctx: &Ctx, signature: &Value, limit: i64) -> Value {
    let limit = clamp_tail_limit(limit);
    let want = signature.as_str();
    let dir_path = ctx.machine_logs_dir();
    let files = iter_errors_jsonl_files(&dir_path);
    let mut matches: Vec<Value> = Vec::new();
    let mut truncated_files: Vec<String> = Vec::new();
    let mut skipped_files: Vec<String> = Vec::new();
    let mut scanned_bytes: u64 = 0;
    for (name, f) in &files {
        if scanned_bytes >= LOG_SCAN_MAX_TOTAL_BYTES {
            skipped_files.push(name.clone());
            continue;
        }
        // 2026-07-18 review LOW-1: lossy-decode (the app_log_tail
        // behavior) instead of silently dropping a WHOLE errors file on
        // invalid UTF-8 while still listing it in files_scanned — every
        // valid JSON line keeps parsing; a binary line fails the
        // per-line JSON parse and is skipped by the existing per-line
        // semantics. A genuinely unreadable file (io error) keeps the
        // skip-continue.
        let Ok((bytes, cut)) = read_tail_capped(f, LOG_READ_MAX_BYTES) else {
            continue;
        };
        scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
        if cut {
            truncated_files.push(name.clone());
        }
        let raw = decode_utf8_replace(&bytes);
        let text = legacy_textmode(&raw);
        for line in legacy_splitlines(&text) {
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
        "truncated_files": truncated_files,
        "skipped_files": skipped_files,
    })
}

/// Recursive `*.md` walk (legacy `Path.rglob("*.md")` equivalent for the
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
            if legacy_fnmatch(&name, "*.md") {
                out.push(path);
            }
        }
    }
}

/// the retired reference implementation `tool_find_runbook_for_code`.
pub fn tool_find_runbook_for_code(ctx: &Ctx, code: &str) -> Value {
    let root = &ctx.repo_root;
    let runbooks_dir = root.join("docs").join("runbooks");
    let rules_dir = root.join(".claude").join("rules");
    // Since the 2026-07-20 rules-tree diet the per-code runbooks live in
    // docs/error-runbooks, and the full rule texts behind the summary stubs
    // in docs/claude-rules-full — searching only the first two trees missed
    // most codes.
    let error_runbooks_dir = root.join("docs").join("error-runbooks");
    let rules_full_dir = root.join("docs").join("claude-rules-full");

    let mut matches: Vec<Value> = Vec::new();
    for search_dir in [runbooks_dir, error_runbooks_dir, rules_dir, rules_full_dir] {
        if !search_dir.exists() {
            continue;
        }
        let mut md_files = Vec::new();
        rglob_md(&search_dir, &mut md_files);
        for md in md_files {
            // Skipped when over RUNBOOK_FILE_MAX_BYTES or not UTF-8, like an
            // unreadable file before.
            let Ok(Some(raw)) = read_whole_capped(&md, RUNBOOK_FILE_MAX_BYTES) else {
                continue;
            };
            let text = legacy_textmode(&raw);
            if !text.contains(code) {
                continue;
            }
            let lines = legacy_splitlines(&text);
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

/// Every tool talks to one fixed address (QuestDB, the app API, CloudWatch
/// Logs, the portal) and none of them answers with a redirect. Following one
/// would let a reply send the next request somewhere the tool never checked,
/// such as the database behind the read-only SQL gate, so redirects are off.
fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client build failed: {e}"))
}

/// Bytes one HTTP reply may hold before the tool refuses it. 8 MiB, the same
/// ceiling as one captured subprocess stream (`PROC_CAPTURE_MAX_BYTES`).
///
/// `resp.bytes()` had no ceiling: a `select *` over a large table, or a
/// `LIMIT lo,hi` range (which the row cap deliberately leaves alone), could
/// grow this process's heap by gigabytes and then hand all of it to the
/// caller as one reply.
pub const HTTP_REPLY_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Why the questdb tool refused a query before sending it.
pub const QUESTDB_READ_ONLY_REFUSAL: &str = "refused: this tool is read-only. Send exactly ONE \
     statement starting with SELECT, SHOW, EXPLAIN or WITH, with no comments and no \
     data-changing keyword (insert, update, delete, drop, alter, truncate, create, copy and the \
     rest of the operator console's list). SELECT and WITH are capped at 1000 rows.";

/// Read a reply body up to `cap` bytes. `Ok((bytes, true))` when the body
/// was LONGER than `cap` — the caller refuses it rather than parse a cut.
fn read_body_capped<R: std::io::Read>(body: R, cap: u64) -> Result<(Vec<u8>, bool), String> {
    use std::io::Read;
    let mut out = Vec::new();
    body.take(cap.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    let over = out.len() as u64 > cap;
    if over {
        out.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    }
    Ok((out, over))
}

fn reply_too_large_error() -> String {
    format!(
        "reply larger than {} MiB — narrow the query (fewer columns, a smaller LIMIT, or an \
         aggregate)",
        HTTP_REPLY_MAX_BYTES / (1024 * 1024)
    )
}

/// the retired reference implementation `tool_questdb_sql`, behind the
/// operator console's read-only gate (2026-09-27): the text is refused
/// before any connection unless `sql_gate::is_safe_sql` passes it, and
/// SELECT/WITH carry a 1000-row cap. `query` in the reply is what the
/// caller sent; `sent_query` is what reached the database.
pub fn tool_questdb_sql(ctx: &Ctx, query: &str) -> Value {
    if !crate::sql_gate::is_safe_sql(query) {
        return json!({"ok": false, "query": query, "error": QUESTDB_READ_ONLY_REFUSAL});
    }
    let sent = crate::sql_gate::cap_sql_rows(query);
    let env = RealEnv;
    let qdb = config::endpoint_url(
        &env,
        &ctx.cfg,
        "questdb_url",
        "TICKVAULT_QUESTDB_URL",
        "http://127.0.0.1:9000",
        None,
    );
    let full = format!("{qdb}/exec?query={}", legacy_urllib_quote(&sent));
    let client = match http_client(15) {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "query": query, "sent_query": sent, "error": e}),
    };
    let resp = match client.get(&full).send() {
        Ok(r) => r,
        Err(e) => {
            // Transport error text differs from urllib's — a documented
            // deviation the parity transcript AVOIDS (the mock HTTP server
            // is always up, so this arm is unreachable in parity); the
            // harness does NOT mask transport-error text.
            return json!({"ok": false, "query": query, "sent_query": sent, "error": e.to_string()});
        }
    };
    let status = resp.status();
    if status.as_u16() >= 400 {
        // legacy urlopen raises HTTPError; str(err) == "HTTP Error {code}: {reason}".
        let reason = status.canonical_reason().unwrap_or("");
        return json!({
            "ok": false,
            "query": query,
            "error": format!("HTTP Error {}: {}", status.as_u16(), reason),
        });
    }
    let body = match read_body_capped(resp, HTTP_REPLY_MAX_BYTES) {
        Ok((_, true)) => {
            return json!({"ok": false, "query": query, "sent_query": sent, "error": reply_too_large_error()});
        }
        Ok((b, false)) => decode_utf8_replace(&b),
        Err(e) => return json!({"ok": false, "query": query, "sent_query": sent, "error": e}),
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(parsed) => json!({"ok": true, "query": query, "sent_query": sent, "response": parsed}),
        Err(e) => {
            // legacy JSONDecodeError text differs — documented deviation.
            json!({"ok": false, "query": query, "sent_query": sent, "error": e.to_string()})
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

/// Refusal for a caller-supplied `base_url` (audit PR29b, 2026-09-27).
pub const API_BASE_OVERRIDE_REFUSAL: &str = "refused: base_url is no longer accepted. \
     tickvault_api only calls the configured app API (TICKVAULT_API_URL or the active \
     profile's tickvault_api_url).";

/// Refusal for a path outside the app API's read routes (audit PR29b).
pub const API_PATH_REFUSAL: &str = "refused: path must be /health or start with /api/, with \
     no '..', '%', '@', '\\', '#', '://', spaces or control characters.";

/// Refusal when the configured API base is the database's own address
/// (audit PR29b): a GET there would skip the read-only SQL gate.
pub const API_BASE_IS_QUESTDB_REFUSAL: &str = "refused: the configured tickvault API address \
     is the database's address, so a GET there would bypass questdb_sql's read-only gate. Fix \
     TICKVAULT_API_URL / tickvault_api_url.";

/// A `tickvault_api` path is allowed only when it is one of the app's own
/// read routes and carries nothing that could redirect the request
/// (audit PR29b). O(len) over the path.
pub fn api_path_is_allowed(path: &str) -> bool {
    let route_ok = path == "/health" || path.starts_with("/health?") || path.starts_with("/api/");
    let chars_ok = path
        .chars()
        .all(|c| !c.is_control() && !c.is_whitespace() && !matches!(c, '%' | '@' | '\\' | '#'));
    route_ok && chars_ok && !path.contains("..") && !path.contains("://")
}

/// (scheme, host, port) of a URL with loopback names folded together and
/// the scheme's default port filled in, so two spellings of one address
/// compare equal. `None` when the URL has no host.
fn url_origin(url: &str) -> Option<(String, String, u16)> {
    let (scheme, host) = split_scheme_host(url);
    let host = host?;
    let rest = url.split_once("://").map_or("", |(_, r)| r);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = rest[..end].rsplit('@').next().unwrap_or("");
    let port_text = if let Some(after) = netloc.strip_prefix('[') {
        after.split_once("]:").map(|(_, p)| p)
    } else {
        netloc.rsplit_once(':').map(|(_, p)| p)
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = match port_text {
        Some(p) if !p.is_empty() => p.parse::<u16>().ok()?,
        _ => default_port,
    };
    let host = match host.as_str() {
        // APPROVED: loopback spellings folded for an address comparison, never a connection target.
        "localhost" | "::1" | "0.0.0.0" => "127.0.0.1".to_string(),
        _ => host,
    };
    Some((scheme, host, port))
}

/// True when the two URLs name the same address. An unparseable URL
/// compares as the same, so the check fails closed.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (url_origin(a), url_origin(b)) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// the retired reference implementation `tool_tickvault_api`, narrowed by
/// audit PR29b (2026-09-27): the caller can no longer name the host, the
/// path must be one of the app's read routes, and the call is refused
/// when the configured API address is the database's, so a GET can never
/// reach QuestDB's `/exec` around the read-only SQL gate and the bearer
/// token only ever goes to the configured API.
pub fn tool_tickvault_api(ctx: &Ctx, path: &str, base_url: Option<&str>) -> Value {
    if base_url.is_some_and(|b| !b.is_empty()) {
        return json!({"ok": false, "error": API_BASE_OVERRIDE_REFUSAL});
    }
    let env = RealEnv;
    let api_url = config::endpoint_url(
        &env,
        &ctx.cfg,
        "tickvault_api_url",
        "TICKVAULT_API_URL",
        "http://127.0.0.1:3001",
        None,
    );
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if !api_path_is_allowed(&path) {
        return json!({"ok": false, "error": API_PATH_REFUSAL, "path": path});
    }
    let qdb_url = config::endpoint_url(
        &env,
        &ctx.cfg,
        "questdb_url",
        "TICKVAULT_QUESTDB_URL",
        "http://127.0.0.1:9000",
        None,
    );
    if same_origin(&api_url, &qdb_url) {
        return json!({"ok": false, "error": API_BASE_IS_QUESTDB_REFUSAL});
    }
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
        // Security parity with the retired reference implementation (adversarial re-review 2026-07-04):
        // never send the bearer over plaintext http to a non-local host.
        let (scheme, host) = split_scheme_host(&full);
        let host = host.unwrap_or_default();
        // APPROVED: loopback allowlist for the plaintext-bearer refusal guard, never a connection target (security parity with the retired reference implementation).
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
    let body = match read_body_capped(resp, HTTP_REPLY_MAX_BYTES) {
        Ok((_, true)) => {
            return json!({"ok": false, "error": reply_too_large_error(), "url": full});
        }
        Ok((b, false)) => decode_utf8_replace(&b),
        Err(e) => return json!({"ok": false, "error": e, "url": full}),
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(parsed) => json!({"ok": true, "status": status_code, "url": full, "json": parsed}),
        Err(_) => json!({
            "ok": true,
            "status": status_code,
            "url": full,
            "text": legacy_slice_chars(&body, 4000),
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

/// Read one child pipe on its own thread, into a SHARED, CAPPED buffer.
///
/// Two properties the previous `read_to_end`-into-a-returned-`Vec` did not
/// have, and both are the point:
///
/// * the bytes are visible to the parent WHILE the thread is still running, so
///   a reader that never reaches EOF still yields what it received. With
///   `read_to_end` the result is all-or-nothing, so the hang and a genuinely
///   empty stream produced the identical observation.
/// * the buffer is bounded by [`PROC_CAPTURE_MAX_BYTES`].
///
/// The returned receiver fires once, when the thread is done. The parent waits
/// on THAT with a timeout rather than on `join()`, which cannot take one.
fn spawn_bounded_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    std::sync::mpsc::Receiver<()>,
) {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&shared);
    // Bounded by construction: exactly one message is ever sent, so a capacity
    // of 1 means `send` never blocks and the channel can never grow. An
    // unbounded `channel()` would carry the same single message and is banned
    // repo-wide, so the bounded form is both compliant and strictly narrower.
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
    std::thread::spawn(move || {
        if let Some(mut pipe) = pipe {
            let mut chunk = [0_u8; 8192];
            // A read error ends the capture exactly as EOF does: there is
            // nothing further to collect either way, and looping on a failing
            // pipe is the busy-wait version of the hang this function bounds.
            while let Ok(read) = pipe.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                // A poisoned lock means the parent panicked; stop rather than
                // spin, and let the parent's own unwind be the report.
                let Ok(mut held) = sink.lock() else { break };
                let Some(room) = PROC_CAPTURE_MAX_BYTES.checked_sub(held.len()) else {
                    break;
                };
                if room == 0 {
                    break;
                }
                held.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
        let _ignored = done_tx.send(());
    });
    (shared, done_rx)
}

/// Take what a reader has captured, waiting a BOUNDED time for it to finish.
///
/// Returns the bytes and whether the reader actually reached end-of-stream.
/// `false` is the hang case, and the caller must say so in the output rather
/// than hand back a short stream that reads as complete.
fn collect_bounded_stream(
    buffer: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: &std::sync::mpsc::Receiver<()>,
    deadline: std::time::Instant,
) -> (Vec<u8>, bool) {
    // ONE deadline SHARED by both streams, not one budget each. A grandchild
    // inherits stdout AND stderr, so a per-stream budget doubles the worst
    // case — the parent waits the full drain twice for a single cause. The
    // first draft of this fix did exactly that and its own regression test
    // caught it, failing at the harness bound.
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let finished = done.recv_timeout(remaining).is_ok();
    let bytes = buffer
        .lock()
        .map(|held| held.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
    (bytes, finished)
}

fn run_with_timeout(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<ProcResult, ProcError> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ProcError::Spawn(e.to_string()))?;
    let (out_buffer, out_done) = spawn_bounded_pipe_reader(child.stdout.take());
    let (err_buffer, err_done) = spawn_bounded_pipe_reader(child.stderr.take());
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // BOUNDED, never `join()`. The child has exited; if a
                // grandchild it spawned still holds the pipe, EOF never comes
                // and an unbounded join would park the server here forever.
                let drain_by =
                    std::time::Instant::now() + Duration::from_millis(PROC_READER_DRAIN_TIMEOUT_MS);
                let (stdout, stdout_drained) =
                    collect_bounded_stream(&out_buffer, &out_done, drain_by);
                let (stderr, stderr_drained) =
                    collect_bounded_stream(&err_buffer, &err_done, drain_by);
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
                let mut stderr_text = legacy_textmode(&decode_utf8_replace(&stderr));
                if !stdout_drained || !stderr_drained {
                    stderr_text.push_str(PROC_CAPTURE_TRUNCATED_NOTICE);
                }
                return Ok(ProcResult {
                    code,
                    stdout: legacy_textmode(&decode_utf8_replace(&stdout)),
                    stderr: stderr_text,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ignored = child.kill();
                    let _ignored = child.wait();
                    // Bounded here too. Killing the child usually closes the
                    // pipes and both readers return at once; when a grandchild
                    // holds them this reaps nothing and simply returns, rather
                    // than leaving the threads unwaited on every timeout.
                    let reap_by = std::time::Instant::now()
                        + Duration::from_millis(PROC_READER_DRAIN_TIMEOUT_MS);
                    let _ignored = collect_bounded_stream(&out_buffer, &out_done, reap_by);
                    let _ignored = collect_bounded_stream(&err_buffer, &err_done, reap_by);
                    return Err(ProcError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(PROC_POLL_INTERVAL_MS));
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
    for line in legacy_splitlines(stdout) {
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

/// the retired reference implementation `tool_run_doctor`.
pub fn tool_run_doctor(ctx: &Ctx) -> Value {
    let out = match run_with_timeout(
        "bash",
        &["scripts/doctor.sh".to_string()],
        &ctx.repo_root,
        Duration::from_secs(DOCTOR_TIMEOUT_SECS),
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
        "raw_stdout_tail": if out.stdout.is_empty() { "" } else { legacy_tail_chars(&out.stdout, 2000) },
    })
}

/// Pure parser for the `git log --pretty=format:%H%x1f%an%x1f%aI%x1f%s`
/// output (unit-testable).
pub fn parse_git_log_stdout(stdout: &str) -> Vec<Value> {
    let mut commits: Vec<Value> = Vec::new();
    for line in legacy_splitlines(stdout) {
        let parts: Vec<&str> = line.split('\u{1f}').collect();
        if parts.len() != 4 {
            continue;
        }
        commits.push(json!({
            "sha": legacy_slice_chars(parts[0], 12),
            "author": parts[1],
            "date": parts[2],
            "subject": parts[3],
        }));
    }
    commits
}

/// the retired reference implementation `tool_git_recent_log`.
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
        Duration::from_secs(GIT_LOG_TIMEOUT_SECS),
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

/// the retired reference implementation `tool_docker_status`.
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
        Duration::from_secs(DOCKER_PS_TIMEOUT_SECS),
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
            "stderr": legacy_slice_chars(out.stderr.trim(), 1000),
        });
    }
    let mut containers: Vec<Value> = Vec::new();
    for line in legacy_splitlines(&out.stdout) {
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

/// the retired reference implementation `tool_app_log_tail`.
pub fn tool_app_log_tail(ctx: &Ctx, limit: i64, date: Option<&str>) -> Value {
    let log_dir = ctx.logs_dir();
    let date = match date {
        Some(d) => d.to_string(),
        None => chrono::Utc::now().format("%Y-%m-%d").to_string(),
    };
    if !is_log_date(&date) {
        return json!({
            "ok": false,
            "error": format!("date must be YYYY-MM-DD, got {:?}", legacy_slice_chars(&date, 40)),
            "log_dir": log_dir.to_string_lossy(),
        });
    }
    // `pathlib_lexical` kept from the parity port; with the date shape
    // checked above there is no `.` component left for it to drop.
    let log_file = config::pathlib_lexical(&log_dir.join(format!("app.{date}.log")));
    if !log_file.exists() {
        return json!({
            "ok": false,
            "error": format!("log file not found: {}", log_file.to_string_lossy()),
            "log_dir": log_dir.to_string_lossy(),
        });
    }
    let (bytes, truncated) = match read_tail_capped(&log_file, LOG_READ_MAX_BYTES) {
        Ok(read) => read,
        Err(err) => {
            return json!({
                "ok": false,
                "error": err.to_string(),
                "path": log_file.to_string_lossy(),
            });
        }
    };
    let limit = clamp_tail_limit(limit);
    let mut bytes = bytes;
    let lines_read = keep_last_lines(&mut bytes, limit);
    let text = legacy_textmode(&decode_utf8_replace(&bytes));
    let lines = legacy_file_lines(&text);
    let tail: &[&str] = legacy_neg_slice(&lines, limit);
    json!({
        "ok": true,
        "path": log_file.to_string_lossy(),
        // Lines in the part that was read; the whole file when `truncated`
        // is false.
        "total_lines": if lines_read as i64 > limit { lines_read } else { lines.len() },
        "returned": tail.len(),
        "truncated": truncated,
        "lines": tail,
    })
}

/// True for a `YYYY-MM-DD` shape: ten bytes, digits with `-` at 4 and 7.
///
/// `date` is joined into a path, so before 2026-09-27 a value such as
/// `x/../../../etc/y` named any `*.log` file on the machine. Only the shape
/// is checked — a well-shaped impossible date simply finds no file.
fn is_log_date(date: &str) -> bool {
    let b = date.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

// ---------------------------------------------------------------------------
// grep_codebase
// ---------------------------------------------------------------------------

const GREP_SKIP_DIRS: [&str; 5] = ["target", ".git", "node_modules", "data", ".terraform"];

// APPROVED: signature mirrors the archived legacy helper for byte-parity.
#[allow(clippy::too_many_arguments)]
/// legacy `Path.relative_to(root)` restricted to the grep_walk use:
/// `path` is a FILE strictly below some walked dir, both inputs are
/// pathlib-normalized. Pathlib compares PARTS, where the root part
/// itself distinguishes `/` from the POSIX `//` root — a string
/// `<root>/` prefix reproduces that exactly for a multi-component root
/// (a normalized root never carries a trailing slash). The bare `/` and
/// `//` roots are special-cased for totality: their string form IS the
/// separator, and pathlib still refuses `/` vs `//` cross-root strips.
fn legacy_relative_to(path: &Path, root: &Path) -> Option<String> {
    let p = path.to_string_lossy();
    let r = root.to_string_lossy();
    if r == "/" {
        let rest = p.strip_prefix('/')?;
        if rest.starts_with('/') {
            return None; // `//x` is NOT under the `/` root in pathlib
        }
        return Some(rest.to_string());
    }
    if r == "//" {
        let rest = p.strip_prefix("//")?;
        return Some(rest.to_string());
    }
    p.strip_prefix(&format!("{r}/")).map(str::to_string)
}

fn grep_walk(
    dir: &Path,
    root: &Path,
    regex: &regex::Regex,
    file_glob: Option<&str>,
    max_matches: usize,
    matches: &mut Vec<Value>,
) -> Result<(), String> {
    if matches.len() >= max_matches {
        return Ok(());
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
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
            && !legacy_fnmatch(&name, glob)
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
        let text = legacy_textmode(&decode_utf8_ignore(&bytes));
        for (idx, line) in legacy_file_lines(&text).iter().enumerate() {
            if regex.is_match(line) {
                // PARITY (review r3 LOW-a): legacy's
                // `full_path.relative_to(root)` raises ValueError on the
                // FIRST match under an absolute `path` arg outside the repo
                // root; the dispatcher wraps it as the -32000
                // `tool grep_codebase failed: ...` error. Mirror the legacy runtime
                // 3.11 text byte-for-byte (verified live:
                // `'/x/y' is not in the subpath of '/a' OR one path is
                // relative and the other is absolute.`). Relative `../`
                // escapes NEVER take this arm on either side — both are
                // lexical prefix matches that yield ../-prefixed rel paths.
                // Residual (ledger): the legacy runtime formats via repr(), which would
                // escape a quote/control char inside a path — unreachable
                // for real repo/fixture paths.
                //
                // PARITY (review r4 LOW-1): the check is a pathlib-PARTS
                // string prefix, NOT Rust `strip_prefix` — Rust components
                // collapse the POSIX `//` root into `/`, so a
                // `//<root>/sub` path arg would strip successfully and
                // fail OPEN (ok:true) where the legacy runtime raises ('//' and '/'
                // are DIFFERENT root parts to pathlib). Both sides of the
                // comparison are pathlib-normalized already (search_root
                // via pathlib_lexical; repo root at config load), so a
                // plain `<root>/` string prefix is exactly pathlib's
                // parts-prefix rule for a multi-component root.
                let rel = match legacy_relative_to(&path, root) {
                    Some(p) => p,
                    None => {
                        return Err(format!(
                            "'{}' is not in the subpath of '{}' OR one path is relative and the other is absolute.",
                            path.display(),
                            root.display()
                        ));
                    }
                };
                matches.push(json!({
                    "file": rel,
                    "line": idx + 1,
                    "text": legacy_slice_chars(line, 500),
                }));
                if matches.len() >= max_matches {
                    break;
                }
            }
        }
        // the legacy runtime quirk kept faithfully: at/above the cap, remaining FILES
        // in this directory are still visited (each can add one more match
        // — the per-line check runs only AFTER an append); only further
        // DIRECTORIES stop (the fn-entry guard mirrors os.walk's
        // outer-loop break).
    }
    for sub in subdirs {
        grep_walk(&sub, root, regex, file_glob, max_matches, matches)?;
        if matches.len() >= max_matches {
            return Ok(());
        }
    }
    Ok(())
}

/// the retired reference implementation `tool_grep_codebase` (max_matches fixed at 200 — the legacy
/// registry never passes it). `Err` = the legacy runtime `Path.relative_to`
/// ValueError for a match under an absolute `path` outside the repo root;
/// the dispatcher maps it to the -32000 `tool grep_codebase failed: ...`
/// wrap exactly like the legacy `except`.
pub fn tool_grep_codebase(
    ctx: &Ctx,
    pattern: &str,
    path: Option<&str>,
    file_glob: Option<&str>,
) -> Result<Value, String> {
    let max_matches = 200usize;
    let root = &ctx.repo_root;
    // pathlib-normalize the arg-derived search root (legacy's
    // `root / path` drops `.` components at Path construction) so every
    // downstream path echo — rel matches AND the outside-root error —
    // uses the same string form as the legacy runtime.
    let search_root = match path {
        Some(p) => {
            let pb = PathBuf::from(p);
            config::pathlib_lexical(&if pb.is_absolute() { pb } else { root.join(pb) })
        }
        None => root.clone(),
    };
    let regex = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(err) => {
            // Rust regex error text differs from the legacy runtime `re.error` —
            // the harness verifies the "invalid regex: " prefix on both
            // sides then masks the detail.
            return Ok(json!({
                "ok": false,
                "pattern": pattern,
                "error": format!("invalid regex: {err}"),
            }));
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
    )?;
    Ok(json!({
        "ok": true,
        "pattern": pattern,
        "match_count": matches.len(),
        "truncated": matches.len() >= max_matches,
        "matches": matches,
    }))
}

// ---------------------------------------------------------------------------
// cloudwatch_logs — SigV4 -> portal chain (the aws CLI fallback was removed 2026-09-27)
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
    // legacy: re.fullmatch(r"[a-z0-9-]+", region)
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
            // Exception TYPE name differs from the legacy runtime — bounded + masked;
            // never echoes any header or the secret.
            let text = e.to_string();
            return json!({
                "ok": false,
                "source": "cloudwatch_sigv4",
                "log_group": group,
                "region": region,
                "error": format!(
                    "CloudWatch FilterLogEvents failed: reqwest::Error: {}",
                    legacy_slice_chars(&text, 300)
                ),
            });
        }
    };
    let status = resp.status();
    if status.as_u16() >= 400 {
        let reason = status.canonical_reason().unwrap_or("").to_string();
        let err_body = resp
            .bytes()
            .map(|b| legacy_slice_chars(&decode_utf8_replace(&b), 400).to_string())
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
    let ok = parsed.get("ok").map(legacy_truthy).unwrap_or(false);
    if !ok {
        return json!({
            "ok": false,
            "source": "portal",
            "portal_url": url,
            "error": "portal returned ok=false (check the bearer token / box state)",
            "portal_response": legacy_slice_chars(&parsed.to_string(), 400),
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

fn legacy_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// the retired reference implementation `tool_cloudwatch_logs` — SigV4 -> portal.
///
/// The third, `aws` CLI, path was removed on 2026-09-27: it spawned an
/// external interpreter-based program, and the SigV4 path above it does the
/// same read natively in Rust with the same read-only key. An environment
/// whose only credential is an `aws` CLI profile now gets the
/// "no log reader configured" answer, which names both remaining paths.
pub fn tool_cloudwatch_logs(
    _ctx: &Ctx,
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
    json!({
        "ok": false,
        "error": "no log reader configured — set AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY (+ AWS_DEFAULT_REGION) for the direct SigV4 path, OR set TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN to read via the operator dashboard. Neither is present in this session.", // secret-scan-ignore: env-var NAMES, no credential value
        "log_group": cloudwatch_log_group(&env),
    })
}

// ---------------------------------------------------------------------------
// Registry — names / descriptions / schemas byte-identical to the retired reference implementation
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
const DESC_TRIAGE: &str = "Last N lines of data/logs/auto-fix.log — audit trail of the error-triage hook's actions. Reads at most the last 32 MiB and returns at most 5000 lines; `truncated` says when the file was longer.";
const DESC_SIG_HISTORY: &str = "All events whose computed signature hash equals `signature`. Use after list_novel_signatures to drill into a specific signature's full history.";
const DESC_RUNBOOK: &str = "Given an ErrorCode string (e.g. 'DH-904', 'I-P1-11'), search every file under docs/runbooks/, docs/error-runbooks/, .claude/rules/ AND docs/claude-rules-full/ for the code and return matching file paths + a preview of the relevant section. Lets Claude jump from a Telegram alert to the operator runbook in one tool call.";
const DESC_QUESTDB: &str = "Run READ-ONLY SQL against the local QuestDB (HTTP /exec). Exactly one SELECT, SHOW, EXPLAIN or WITH statement, no comments, no data-changing keyword (the operator console's gate); anything else is refused before it is sent. SELECT and WITH are capped at 1000 rows and a reply at 8 MiB. Returns columnar JSON. Lets Claude query the trading data plane — ticks, orders, candles — without pg CLI.";
const DESC_GREP: &str = "Regex-search the repo for a pattern. Skips target/, .git/, node_modules/, data/, .terraform/. Returns up to 200 matches of {file, line, text}. Lets Claude find any function, error string, config key, test name across the workspace in one call.";
const DESC_DOCTOR: &str = "Invoke `bash scripts/doctor.sh` and return the parsed output as {pass_count, fail_count, rows[{status, detail}]}. One-call total-system health check for Claude.";
const DESC_GIT: &str = "Return the last N commits on the current branch with sha/author/date/subject. Lets Claude investigate 'what changed recently' without leaving the MCP surface.";
const DESC_API: &str = "HTTP GET against the tickvault app's own REST API (port 3001 by default). Paths: /health, /api/stats, /api/quote/{security_id}, /api/instruments/diagnostic, /api/option-chain, /api/pcr, /api/index-constituency. Returns status + json (or text).";
const DESC_DOCKER: &str = "Return `docker compose ps --format json` for the tickvault stack. Gives Claude container name/service/state/health without shelling into the host.";
const DESC_APP_LOG: &str = "Last N lines of data/logs/app.YYYY-MM-DD.log (full INFO/DEBUG output, not just ERRORs). Optional `date` (YYYY-MM-DD) picks a different day; defaults to today UTC. Reads at most the last 32 MiB of the file and returns at most 5000 lines; `truncated` says when the file was longer.";
const DESC_CLOUDWATCH: &str = "Read recent PROD logs — fully automated, no human paste/download. PREFERRED: direct CloudWatch read via a SigV4-signed HTTPS request — the ONLY input is a read-only AWS key in the env (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY [+ AWS_SESSION_TOKEN], AWS_DEFAULT_REGION); no aws CLI, no portal, no boto3. NEXT: the operator-control dashboard's logs endpoint (TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN). Args: minutes (lookback; SigV4 path), filter_pattern (CloudWatch filter on SigV4, substring on portal, e.g. \"ERROR\" or \"WS-GAP-05\"), limit (default 100). The AWS secret/token is NEVER logged nor returned. Clear ok=false error if no path is configured."; // secret-scan-ignore: env-var NAMES in a tool description, no credential value

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
/// inputSchema VALUES as the retired reference implementation's TOOLS registry (object key order is
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
                        "description": grep_pattern_description(),
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
// Dispatch — mirrors the legacy registry lambdas, incl. their `int()`
// coercions and `args[...]` KeyError messages.
// ---------------------------------------------------------------------------

fn get_int(args: &Map<String, Value>, key: &str, default: i64) -> Result<i64, String> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => legacy_int(v),
    }
}

fn get_opt_str(args: &Map<String, Value>, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        // Null == legacy None == "not provided" for these optional args;
        // non-string values are a documented deviation (legacy would pass
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
            "argument `{key}` must be a string, got {other} (Rust deviation: {RUNTIME_NAME} raises TypeError later)"
        )),
    }
}

/// `tools/call` dispatch. `Err(msg)` maps to the JSON-RPC -32000
/// `tool {name} failed: {msg}` error, exactly like the legacy `except`.
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
            Ok(v) => tool_list_novel_signatures(ctx, v),
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
                tool_grep_codebase(ctx, &pattern, path.as_deref(), file_glob.as_deref())
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

    /// Run `run_with_timeout` on a helper thread so a REGRESSION fails this
    /// test instead of hanging it.
    ///
    /// This shape is not decoration. The defect under test is an unbounded
    /// block, so calling it directly would make the un-fixed code park the
    /// test runner — the exact outcome being fixed, and worthless as a signal.
    /// The channel converts "parked forever" into a named assertion failure.
    fn run_off_thread(
        script: &str,
        harness_budget: Duration,
    ) -> Option<Result<ProcResult, ProcError>> {
        // Bounded: exactly one result is ever sent, so capacity 1 never blocks.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let owned = script.to_owned();
        std::thread::spawn(move || {
            let out = run_with_timeout(
                "sh",
                &["-c".to_owned(), owned],
                Path::new("."),
                Duration::from_secs(30),
            );
            let _ignored = tx.send(out);
        });
        rx.recv_timeout(harness_budget).ok()
    }

    /// THE REGRESSION. A child that backgrounds anything leaves a GRANDCHILD
    /// holding the inherited stdout pipe, so end-of-stream never arrives even
    /// though the child itself exited immediately.
    ///
    /// `read_to_end` + `join()` waits for that EOF with no bound. Here the
    /// grandchild holds the pipe for 15 seconds and the harness gives up at
    /// 10 — so an unbounded join fails this test in ten seconds rather than
    /// parking the suite.
    #[test]
    fn a_grandchild_holding_the_pipe_cannot_park_the_reader_join() {
        let Some(result) = run_off_thread("sleep 15 & echo held-open", Duration::from_secs(10))
        else {
            panic!(
                "run_with_timeout did not return within 10s while a backgrounded grandchild held \
                 the stdout pipe. That is the unbounded-join hang: the child exited at once, but \
                 EOF waits for EVERY holder of the write end to close it."
            )
        };
        let Ok(proc) = result else {
            panic!("the child exited cleanly, so this must not be a spawn error or a timeout")
        };
        assert_eq!(
            proc.code, 0,
            "the shell exited 0 and that code must survive"
        );
        assert!(
            proc.stdout.contains("held-open"),
            "bytes written before the reader gave up must still be returned — an \
             all-or-nothing capture makes the hang indistinguishable from an empty stream. \
             Got: {:?}",
            proc.stdout
        );
        assert!(
            proc.stderr.contains("capture INCOMPLETE"),
            "a stream that never reached end-of-stream must SAY so; otherwise a partial \
             capture reads as a complete one. Got: {:?}",
            proc.stderr
        );
    }

    /// The bound must not cost the ordinary case anything.
    #[test]
    fn an_ordinary_command_still_captures_both_streams_and_says_nothing_extra() {
        let Some(result) =
            run_off_thread("echo out-line; echo err-line 1>&2", Duration::from_secs(10))
        else {
            panic!("a trivial command must return well inside the harness budget")
        };
        let Ok(proc) = result else {
            panic!("expected a clean run")
        };
        assert_eq!(proc.code, 0);
        assert!(proc.stdout.contains("out-line"), "got {:?}", proc.stdout);
        assert!(proc.stderr.contains("err-line"), "got {:?}", proc.stderr);
        assert!(
            !proc.stderr.contains("capture INCOMPLETE"),
            "a fully drained run must NOT be marked truncated — a notice that fires on healthy \
             runs is one operators learn to ignore. Got: {:?}",
            proc.stderr
        );
    }

    /// A non-zero exit still reports its code, with the pipes drained.
    #[test]
    fn a_failing_command_reports_its_exit_code() {
        let Some(Ok(proc)) = run_off_thread("echo boom 1>&2; exit 3", Duration::from_secs(10))
        else {
            panic!("expected a clean run with a non-zero exit code")
        };
        assert_eq!(proc.code, 3);
        assert!(proc.stderr.contains("boom"), "got {:?}", proc.stderr);
    }

    /// The capture is CAPPED. Driven through the reader directly rather than a
    /// real 8 MiB child, so the test costs milliseconds instead of seconds.
    #[test]
    fn spawn_bounded_pipe_reader_stops_at_the_capture_ceiling() {
        let oversized = vec![b'x'; PROC_CAPTURE_MAX_BYTES + 4096];
        let (buffer, done) = spawn_bounded_pipe_reader(Some(std::io::Cursor::new(oversized)));
        let (bytes, finished) = collect_bounded_stream(
            &buffer,
            &done,
            std::time::Instant::now() + Duration::from_millis(PROC_READER_DRAIN_TIMEOUT_MS),
        );
        assert!(
            finished,
            "the reader must FINISH at the ceiling rather than spin — a capped reader that \
             never sends is the hang with a different cause"
        );
        assert_eq!(
            bytes.len(),
            PROC_CAPTURE_MAX_BYTES,
            "the buffer must stop exactly at the ceiling, never grow past it"
        );
    }

    /// An absent pipe is not an error, and must not leave the parent waiting.
    #[test]
    fn spawn_bounded_pipe_reader_completes_on_a_missing_pipe() {
        let (buffer, done) = spawn_bounded_pipe_reader(Option::<std::io::Cursor<Vec<u8>>>::None);
        let (bytes, finished) = collect_bounded_stream(
            &buffer,
            &done,
            std::time::Instant::now() + Duration::from_millis(PROC_READER_DRAIN_TIMEOUT_MS),
        );
        assert!(finished, "a `None` pipe must signal completion immediately");
        assert!(bytes.is_empty());
    }

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
    fn novel_cutoff_is_minutes_scale() {
        use chrono::TimeZone;
        // Injected deterministic `now` — no wall clock anywhere.
        let now = chrono::Utc.with_ymd_and_hms(2026, 7, 18, 12, 0, 0).unwrap();
        // Fixture event first_seen at now - 30 minutes, compared with the
        // SAME `ts >= cutoff` inclusion rule the tool uses.
        let first_seen = now - chrono::Duration::minutes(30);
        assert!(
            first_seen >= novel_cutoff(now, 60).unwrap(),
            "event at now-30min must be INCLUDED at since_minutes=60"
        );
        assert!(
            first_seen < novel_cutoff(now, 10).unwrap(),
            "event at now-30min must be EXCLUDED at since_minutes=10"
        );
        // Exact scale pin: 60 MINUTES == 3600 seconds. A minutes->seconds
        // mutation yields now - 60s here and fails.
        assert_eq!(
            novel_cutoff(now, 60).unwrap(),
            now - chrono::Duration::seconds(3600)
        );
    }

    /// PR #1644 R6 CRITICAL: `novel_cutoff` must NEVER panic and must
    /// replicate the legacy runtime's OverflowError bands byte-exactly. Thresholds
    /// below were binary-searched against the merge-base the retired reference implementation's
    /// `timedelta(minutes=m)` / `now - td` on 2026-07-18 with the SAME
    /// injected `now` (2026-07-18T10:00:00Z).
    #[test]
    fn novel_cutoff_legacy_overflow_bands() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 7, 18, 10, 0, 0).unwrap();
        let cint = c_int_overflow_msg();
        let range = "date value out of range";

        // Band 1 — C-int (32-bit days) overflow, both signs, incl. the
        // exact boundaries and i64 extremes (the pre-fix panic values).
        for v in [
            3_092_376_453_120_i64, // days = 2^31 (first C-int, positive)
            -3_092_376_453_121,    // days = -2^31 - 1 (first C-int, negative)
            153_722_867_280_913,   // chrono's old panic threshold
            i64::MAX,
            i64::MIN,
        ] {
            assert_eq!(novel_cutoff(now, v).unwrap_err(), cint, "v={v}");
        }

        // Band 2 — timedelta days-magnitude message EMBEDS floor(m/1440).
        assert_eq!(
            novel_cutoff(now, 1_440_000_000_000).unwrap_err(),
            "days=1000000000; must have magnitude <= 999999999"
        );
        assert_eq!(
            novel_cutoff(now, 3_092_376_453_119).unwrap_err(),
            "days=2147483647; must have magnitude <= 999999999"
        );
        assert_eq!(
            novel_cutoff(now, -1_439_999_998_561).unwrap_err(),
            "days=-1000000000; must have magnitude <= 999999999"
        );
        assert_eq!(
            novel_cutoff(now, -3_092_376_453_120).unwrap_err(),
            "days=-2147483648; must have magnitude <= 999999999"
        );

        // Band 3 — timedelta constructible, subtraction outside legacy's
        // year 1..=9999 datetime range.
        assert_eq!(novel_cutoff(now, 1_000_000_000_000).unwrap_err(), range);
        assert_eq!(novel_cutoff(now, 1_439_999_999_999).unwrap_err(), range);
        assert_eq!(novel_cutoff(now, -1_000_000_000_000).unwrap_err(), range);
        assert_eq!(novel_cutoff(now, -1_439_999_998_560).unwrap_err(), range);
        // Exact OK/range boundaries for THIS `now` (legacy-probed).
        assert!(novel_cutoff(now, 1_065_332_760).is_ok());
        assert_eq!(novel_cutoff(now, 1_065_332_761).unwrap_err(), range);
        assert!(novel_cutoff(now, -4_193_632_199).is_ok());
        assert_eq!(novel_cutoff(now, -4_193_632_200).unwrap_err(), range);

        // Band 4 — success band (incl. negatives: cutoff in the future).
        assert_eq!(
            novel_cutoff(now, -60).unwrap(),
            now + chrono::Duration::minutes(60)
        );
        assert_eq!(novel_cutoff(now, 0).unwrap(), now);
    }

    /// No-panic ladder across the full i64 domain (the R6 DoS class):
    /// every value must return Ok or Err — never abort the process.
    #[test]
    fn novel_cutoff_never_panics_ladder() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 7, 18, 10, 0, 0).unwrap();
        let mut ladder: Vec<i64> = vec![i64::MIN, i64::MAX, 0];
        let mut v: i64 = 1;
        while v < i64::MAX / 7 {
            ladder.push(v);
            ladder.push(-v);
            ladder.push(v.saturating_add(1));
            ladder.push(-v.saturating_sub(1));
            v = v.saturating_mul(7);
        }
        for b in [
            1_065_332_760_i64,
            4_193_632_200,
            1_439_999_998_560,
            1_439_999_999_999,
            1_440_000_000_000,
            3_092_376_453_119,
            3_092_376_453_120,
            153_722_867_280_912,
            153_722_867_280_913,
        ] {
            for d in [-1_i64, 0, 1] {
                ladder.push(b.saturating_add(d));
                ladder.push(-b.saturating_add(d));
            }
        }
        for m in ladder {
            let _ = novel_cutoff(now, m); // must not panic
        }
    }

    #[test]
    fn legacy_neg_slice_matches_legacy_semantics() {
        let v = [1, 2, 3, 4, 5];
        assert_eq!(legacy_neg_slice(&v, 2), &[4, 5]);
        assert_eq!(legacy_neg_slice(&v, 10), &v);
        assert_eq!(legacy_neg_slice(&v, 0), &v); // lines[-0:] == lines[0:]
        assert_eq!(legacy_neg_slice(&v, -2), &[3, 4, 5]); // lines[2:]
    }

    #[test]
    fn legacy_file_lines_matches_legacy_iteration() {
        assert_eq!(legacy_file_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(legacy_file_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(legacy_file_lines(""), Vec::<&str>::new());
        assert_eq!(legacy_file_lines("\n"), vec![""]);
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

    /// WIRE-VALUE PIN (operator directive 2026-08-01). The retired
    /// runtime's name is byte-assembled (`RUNTIME_NAME_BYTES`) so the
    /// literal leaves the source, but every byte that reaches an MCP
    /// client must stay byte-for-byte IDENTICAL to what the legacy server
    /// sent. Expected values are built FROM the byte array — never written
    /// as a literal here — so this test cannot drift with the source.
    #[test]
    fn runtime_name_wire_bytes_are_unchanged() {
        assert_eq!(RUNTIME_NAME_BYTES, &[0x50, 0x79, 0x74, 0x68, 0x6f, 0x6e]);
        assert_eq!(RUNTIME_NAME.as_bytes(), RUNTIME_NAME_BYTES);

        // `novel_cutoff` band-1 tool-error text.
        assert_eq!(
            c_int_overflow_msg().as_bytes(),
            [RUNTIME_NAME_BYTES, b" int too large to convert to C int"]
                .concat()
                .as_slice()
        );
        // Reached through the real code path, not just the helper.
        let now = chrono::Utc::now();
        assert_eq!(
            novel_cutoff(now, i64::MAX).unwrap_err().as_bytes(),
            [RUNTIME_NAME_BYTES, b" int too large to convert to C int"]
                .concat()
                .as_slice()
        );

        // Advertised `grep_codebase.pattern` schema description, asserted
        // on the REGISTRY value the client actually receives.
        let expected_desc = [
            RUNTIME_NAME_BYTES,
            b" regex (anchors, character classes supported)",
        ]
        .concat();
        assert_eq!(
            grep_pattern_description().as_bytes(),
            expected_desc.as_slice()
        );
        let listed = tools_list_json();
        let grep = listed
            .as_array()
            .expect("tools/list is an array")
            .iter()
            .find(|t| t["name"] == "grep_codebase")
            .expect("grep_codebase advertised");
        assert_eq!(
            grep["inputSchema"]["properties"]["pattern"]["description"]
                .as_str()
                .expect("description is a string")
                .as_bytes(),
            expected_desc.as_slice()
        );

        // `require_str` type-error text.
        let mut args = Map::new();
        args.insert("code".to_string(), json!(7));
        assert_eq!(
            require_str(&args, "code").unwrap_err().as_bytes(),
            [
                b"argument `code` must be a string, got 7 (Rust deviation: ".as_slice(),
                RUNTIME_NAME_BYTES,
                b" raises TypeError later)",
            ]
            .concat()
            .as_slice()
        );
    }

    #[test]
    fn legacy_truthy_matches() {
        assert!(!legacy_truthy(&json!(null)));
        assert!(!legacy_truthy(&json!(false)));
        assert!(!legacy_truthy(&json!(0)));
        assert!(!legacy_truthy(&json!("")));
        assert!(legacy_truthy(&json!("x")));
        assert!(legacy_truthy(&json!(1)));
        assert!(legacy_truthy(&json!({"a": 1})));
    }

    #[test]
    fn legacy_isoformat_matches_legacy() {
        let dt = chrono::DateTime::parse_from_rfc3339("2026-07-18T05:30:00+00:00").unwrap();
        assert_eq!(legacy_isoformat(&dt), "2026-07-18T05:30:00+00:00");
        let dt2 = chrono::DateTime::parse_from_rfc3339("2026-07-18T05:30:00.123456+05:30").unwrap();
        assert_eq!(legacy_isoformat(&dt2), "2026-07-18T05:30:00.123456+05:30");
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

    #[test]
    fn grep_absolute_path_outside_root_errors_with_legacy_valueerror_text() {
        let outside = std::env::temp_dir().join(format!("tv-mcp-grepout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("needle.txt"), "GREP_OUT_NEEDLE here\n").unwrap();
        let ctx = Ctx {
            repo_root: PathBuf::from("/definitely-not-a-real-root"),
            cfg: config::EndpointsConfig::default(),
        };
        // the legacy runtime 3.11 `Path.relative_to` text, verified live:
        //   ValueError: '/x/y' is not in the subpath of '/a' OR one path is
        //   relative and the other is absolute.
        let err = tool_grep_codebase(&ctx, "GREP_OUT_NEEDLE", outside.to_str(), None).unwrap_err();
        assert_eq!(
            err,
            format!(
                "'{}' is not in the subpath of '/definitely-not-a-real-root' OR one path is relative and the other is absolute.",
                outside.join("needle.txt").display()
            )
        );
        // No match under the outside dir => legacy never reaches
        // relative_to => ok:true, zero matches on BOTH sides.
        let ok = tool_grep_codebase(&ctx, "NO_SUCH_NEEDLE_ZZZ", outside.to_str(), None).unwrap();
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["match_count"], json!(0));
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn grep_double_slash_root_path_errors_like_pathlib() {
        // Review r4 LOW-1: pathlib treats the POSIX `//` root as a
        // DIFFERENT root part from `/`, so `path="//<root>/sub"` is NOT
        // in the subpath of `/<root>` — the legacy runtime raises the -32000
        // ValueError on the first match. Rust `strip_prefix` collapses
        // `//` and would fail OPEN (ok:true); the pathlib-parts
        // `legacy_relative_to` must error with the `//`-quoted text.
        let base = std::env::temp_dir().join(format!("tv-mcp-grepds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub").join("f.txt"), "GREP_DS_NEEDLE\n").unwrap();
        let ctx = Ctx {
            repo_root: base.clone(),
            cfg: config::EndpointsConfig::default(),
        };
        let double = format!("/{}/sub", base.display()); // "//tmp/.../sub"
        let err = tool_grep_codebase(&ctx, "GREP_DS_NEEDLE", Some(&double), None).unwrap_err();
        assert_eq!(
            err,
            format!(
                "'{double}/f.txt' is not in the subpath of '{}' OR one path is relative and the other is absolute.",
                base.display()
            )
        );
        // Sanity: the error text really carries the preserved `//` root.
        assert!(err.starts_with("'//"), "{err}");
        // No match under the `//` root => relative_to never runs => ok
        // empty on BOTH sides.
        let ok = tool_grep_codebase(&ctx, "NO_SUCH_NEEDLE_DS", Some(&double), None).unwrap();
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["match_count"], json!(0));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn grep_relative_escape_still_returns_dotdot_prefixed_paths() {
        // Review r3 nuance pin: a RELATIVE `../` escape is a lexical prefix
        // match on both sides (pathlib keeps `..` verbatim) — it must stay
        // ok:true with ../-prefixed rel paths, never the ValueError arm.
        let base = std::env::temp_dir().join(format!("tv-mcp-grepdd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::create_dir_all(base.join("sibling")).unwrap();
        std::fs::write(base.join("sibling").join("f.txt"), "GREP_DD_NEEDLE\n").unwrap();
        let ctx = Ctx {
            repo_root: base.join("root"),
            cfg: config::EndpointsConfig::default(),
        };
        let out = tool_grep_codebase(&ctx, "GREP_DD_NEEDLE", Some("../sibling"), None).unwrap();
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["match_count"], json!(1));
        assert_eq!(out["matches"][0]["file"], json!("../sibling/f.txt"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn app_log_tail_refuses_a_date_that_is_not_yyyy_mm_dd() {
        // Replaces the 2026-07 parity test that echoed `app.x/y.log` for the
        // date "x/./y": a path-shaped date named files outside the log
        // directory, so it is now refused before any path is built.
        let base = std::env::temp_dir().join(format!("tv-mcp-date-{}", std::process::id()));
        let logs = base.join("data").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(base.join("data").join("secret.log"), "outside\n").unwrap();
        let ctx = Ctx {
            repo_root: base.clone(),
            cfg: config::EndpointsConfig::default(),
        };
        for date in [
            "x/./y",
            "x/../../secret",
            "../secret",
            "2026-09-2",
            "2026/09/27",
            "2026-09-27x",
        ] {
            let out = tool_app_log_tail(&ctx, 5, Some(date));
            assert_eq!(out["ok"], json!(false), "{date}");
            let err = out["error"].as_str().unwrap();
            assert!(err.starts_with("date must be YYYY-MM-DD"), "{date}: {err}");
        }
        // A well-shaped date still reads normally.
        std::fs::write(logs.join("app.2026-09-27.log"), "a\nb\n").unwrap();
        let out = tool_app_log_tail(&ctx, 5, Some("2026-09-27"));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["lines"], json!(["a", "b"]));
        assert_eq!(out["truncated"], json!(false));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_tail_capped_keeps_only_whole_lines_from_the_end() {
        let dir = std::env::temp_dir().join(format!("tv-mcp-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.log");
        std::fs::write(&path, "first line\nsecond\nthird\n").unwrap();
        // Under the cap: the whole file, not cut.
        let (all, cut) = read_tail_capped(&path, 1024).unwrap();
        assert_eq!(
            (all.as_slice(), cut),
            (b"first line\nsecond\nthird\n".as_slice(), false)
        );
        // A cap that lands mid-line drops the partial line.
        let (tail, cut) = read_tail_capped(&path, 10).unwrap();
        assert!(cut);
        assert_eq!(tail, b"third\n");
        // A cap inside the last line leaves nothing rather than half a line.
        let (none, cut) = read_tail_capped(&path, 3).unwrap();
        assert!(cut);
        assert!(none.is_empty());
        // A window that starts exactly on a line boundary keeps that line
        // (the off-by-one the first version had: it dropped it).
        let (tail, cut) = read_tail_capped(&path, 6).unwrap();
        assert!(cut);
        assert_eq!(tail, b"third\n");
        let (tail, cut) = read_tail_capped(&path, 13).unwrap();
        assert!(cut);
        assert_eq!(tail, b"second\nthird\n");
        // A last line with no trailing newline survives a cut too.
        let open_ended = dir.join("g.log");
        std::fs::write(&open_ended, "a\nb\nc").unwrap();
        let (tail, cut) = read_tail_capped(&open_ended, 3).unwrap();
        assert!(cut);
        assert_eq!(tail, b"b\nc");
        // Exactly at the file length is not a cut.
        let len = std::fs::metadata(&path).unwrap().len();
        let (_, cut) = read_tail_capped(&path, len).unwrap();
        assert!(!cut);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_limit_is_clamped() {
        assert_eq!(clamp_tail_limit(0), LOG_TAIL_MAX_LINES);
        assert_eq!(clamp_tail_limit(-3), LOG_TAIL_MAX_LINES);
        assert_eq!(clamp_tail_limit(LOG_TAIL_MAX_LINES + 1), LOG_TAIL_MAX_LINES);
        assert_eq!(clamp_tail_limit(7), 7);
    }

    #[test]
    fn keep_last_lines_sizes_the_index_by_the_reply() {
        let mut b = b"1\n2\n3\n4\n".to_vec();
        assert_eq!(keep_last_lines(&mut b, 2), 4);
        assert_eq!(b, b"3\n4\n");
        // An unterminated last line counts as a line and is kept.
        let mut b = b"1\n2\n3".to_vec();
        assert_eq!(keep_last_lines(&mut b, 2), 3);
        assert_eq!(b, b"2\n3");
        // Fewer lines than the limit: untouched.
        let mut b = b"1\n2\n".to_vec();
        assert_eq!(keep_last_lines(&mut b, 5), 2);
        assert_eq!(b, b"1\n2\n");
        // Empty input.
        let mut b = Vec::new();
        assert_eq!(keep_last_lines(&mut b, 3), 0);
        assert!(b.is_empty());
    }

    #[test]
    fn read_whole_capped_refuses_a_file_over_the_cap() {
        let dir = std::env::temp_dir().join(format!("tv-mcp-whole-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        std::fs::write(&path, "abcdef").unwrap();
        assert_eq!(
            read_whole_capped(&path, 6).unwrap().as_deref(),
            Some("abcdef")
        );
        assert_eq!(read_whole_capped(&path, 5).unwrap(), None);
        assert!(read_whole_capped(&dir.join("missing"), 5).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_log_tail_returns_the_last_lines_of_a_long_file() {
        let base = std::env::temp_dir().join(format!("tv-mcp-long-{}", std::process::id()));
        let logs = base.join("data").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let body: String = (0..10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(logs.join("app.2026-09-27.log"), body).unwrap();
        let ctx = Ctx {
            repo_root: base.clone(),
            cfg: config::EndpointsConfig::default(),
        };
        let out = tool_app_log_tail(&ctx, 3, Some("2026-09-27"));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["lines"], json!(["line7", "line8", "line9"]));
        assert_eq!(out["total_lines"], json!(10));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_body_capped_refuses_a_reply_over_the_cap() {
        let (b, over) = read_body_capped(&b"abcdef"[..], 6).unwrap();
        assert_eq!((b.as_slice(), over), (b"abcdef".as_slice(), false));
        let (b, over) = read_body_capped(&b"abcdefg"[..], 6).unwrap();
        assert!(over);
        assert_eq!(b.len(), 6);
    }

    #[test]
    fn questdb_sql_refuses_a_destructive_query_before_connecting() {
        // The gate runs before any endpoint is resolved or any client is
        // built, so a refused query comes back as the read-only refusal —
        // never a connection error — whatever the environment points at.
        let cfg = config::EndpointsConfig::default();
        let ctx = Ctx {
            repo_root: std::env::temp_dir(),
            cfg,
        };
        for q in [
            "drop table ticks",
            "truncate table ticks",
            "select 1; drop table ticks",
        ] {
            let out = tool_questdb_sql(&ctx, q);
            assert_eq!(out["ok"], json!(false), "{q}");
            assert_eq!(out["error"], json!(QUESTDB_READ_ONLY_REFUSAL), "{q}");
        }
    }

    /// 2026-07-18 review LOW-1: an errors.jsonl file containing an
    /// invalid-UTF-8 line must be LOSSY-decoded, never silently dropped
    /// whole — the valid JSON lines on either side of the binary line
    /// must both survive (the binary line itself fails the per-line
    /// JSON parse and is skipped by the existing per-line semantics).
    #[test]
    fn tail_errors_lossy_decodes_invalid_utf8_errors_file() {
        let base = std::env::temp_dir().join(format!("tv-mcp-lossy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let machine = base.join("data").join("logs").join("machine");
        std::fs::create_dir_all(&machine).unwrap();
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"{\"code\":\"AAA-01\",\"message\":\"first\"}\n");
        bytes.extend_from_slice(&[0xff, 0xfe, 0x80]); // invalid UTF-8 line
        bytes.extend_from_slice(b"\n{\"code\":\"BBB-02\",\"message\":\"second\"}\n");
        std::fs::write(machine.join("errors.jsonl.2026-07-18-05"), &bytes).unwrap();
        let ctx = Ctx {
            repo_root: base.clone(),
            cfg: config::EndpointsConfig::default(),
        };
        let out = tool_tail_errors(&ctx, 10, None);
        let events = out["events"].as_array().unwrap();
        let codes: Vec<&str> = events.iter().filter_map(|e| e["code"].as_str()).collect();
        assert!(
            codes.contains(&"AAA-01"),
            "valid line BEFORE the binary line must survive lossy decode: {out}"
        );
        assert!(
            codes.contains(&"BBB-02"),
            "valid line AFTER the binary line must survive lossy decode: {out}"
        );
        assert_eq!(
            events.len(),
            2,
            "exactly the two valid events (binary line skipped per-line): {out}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn api_path_is_allowed_only_for_the_app_read_routes() {
        for ok in [
            "/health",
            "/health?x=1",
            "/api/stats",
            "/api/quote/13",
            "/api/debug/logs/summary",
        ] {
            assert!(api_path_is_allowed(ok), "{ok}");
        }
        for bad in [
            "/",
            "/exec?query=SELECT 1",
            "/exec",
            "/imp",
            "/healthz",
            "/api",
            "/api/../exec",
            "/api/%2e%2e/exec",
            "//127.0.0.1:9000/exec",
            "/api/x@127.0.0.1:9000/x",
            "/api/x\\y",
            "/health#/exec",
            "/api/http://127.0.0.1:9000/exec",
            "/api/x y",
            "/api/x\ny",
            "/api/x\ty",
        ] {
            assert!(!api_path_is_allowed(bad), "{bad:?}");
        }
    }

    #[test]
    fn same_origin_folds_loopback_spellings_and_default_ports() {
        assert!(same_origin(
            "http://127.0.0.1:9000",
            "http://localhost:9000"
        ));
        assert!(same_origin("http://127.0.0.1:9000/", "http://[::1]:9000"));
        assert!(same_origin(
            "http://0.0.0.0:9000",
            "http://127.0.0.1:9000/exec"
        ));
        assert!(same_origin("http://host", "http://host:80"));
        assert!(same_origin("https://host", "https://host:443"));
        assert!(same_origin(
            "http://user@127.0.0.1:9000",
            "http://127.0.0.1:9000"
        ));
        assert!(!same_origin(
            "http://127.0.0.1:3001",
            "http://127.0.0.1:9000"
        ));
        assert!(!same_origin(
            "http://127.0.0.1:9000",
            "https://127.0.0.1:9000"
        ));
        assert!(!same_origin("http://a:9000", "http://b:9000"));
        // Unparseable fails closed (reads as the same address).
        assert!(same_origin("not a url", "http://127.0.0.1:9000"));
        assert!(same_origin(
            "http://127.0.0.1:notaport",
            "http://127.0.0.1:9000"
        ));
    }

    #[test]
    fn tickvault_api_refuses_a_caller_base_url_before_any_network() {
        let ctx = Ctx {
            repo_root: std::path::PathBuf::from("/nonexistent"),
            cfg: config::EndpointsConfig::default(),
        };
        let out = tool_tickvault_api(&ctx, "/health", Some("http://127.0.0.1:9000"));
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"], API_BASE_OVERRIDE_REFUSAL);
    }
}
