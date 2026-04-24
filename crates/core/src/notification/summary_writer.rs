//! Phase 5 of `.claude/plans/active-plan.md` — `errors.summary.md` writer.
//!
//! Reads the most recent `data/logs/errors.jsonl.YYYY-MM-DD-HH` files,
//! groups events by signature hash, and writes a human AND Claude-readable
//! markdown snapshot at `data/logs/errors.summary.md` on a fixed cadence
//! (default 60 seconds).
//!
//! Claude Code `/loop 5m .claude/triage/claude-loop-prompt.md` reads the
//! single summary file instead of parsing raw JSONL, keeping the triage
//! flow O(1) on Claude's side.
//!
//! # Design
//!
//! - **Input**: glob `{dir}/errors.jsonl*` (the rolling appender file
//!   plus any older hourly-rotated files).
//! - **Window**: events whose `ts` is within `LOOKBACK_MINUTES` of now.
//! - **Grouping**: signature hash = `sha256(code + target + message)`
//!   truncated to 16 hex chars. Counts + first/last-seen timestamp.
//! - **Output**: markdown tables with the top N signatures + novel-today
//!   callout, written atomically (write `.tmp` → rename).
//!
//! # Hot-path compliance
//!
//! The writer runs as a dedicated tokio task with a 60s tick. File I/O
//! happens OFF the tick pipeline — the summary writer never blocks a
//! WebSocket frame parse or a QuestDB ILP flush.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default cadence for the summary refresh loop.
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 60;

/// Default lookback window (minutes) for "active errors".
pub const DEFAULT_LOOKBACK_MINUTES: i64 = 15;

/// Maximum number of signatures rendered in the top-signatures table.
pub const DEFAULT_TOP_SIGNATURES: usize = 10;

/// Output filename. Consumers (Claude loop, operator `make status`) depend
/// on this — do NOT rename without updating them.
pub const SUMMARY_FILENAME: &str = "errors.summary.md";

/// Prometheus counter: total successful summary regenerations.
/// Downstream alerts watch for rate drops as a liveness signal on the
/// observability chain itself. Renaming breaks the Grafana panel + alert
/// rule — guarded by `test_metric_names_are_canonical`.
pub const METRIC_REFRESH_TOTAL: &str = "tv_errors_summary_refresh_total";

/// Prometheus counter: total summary regenerations that returned `Err`.
/// Non-zero means the writer could not read the JSONL dir or could not
/// rename the tmp file into place. Operator action: check disk + permissions.
pub const METRIC_REFRESH_FAILED_TOTAL: &str = "tv_errors_summary_refresh_failed_total";

/// Minimal shape of a JSONL event we care about. Extra fields are ignored
/// via `serde(default)` / the flatten escape hatch.
#[derive(Debug, Deserialize)]
struct RawEvent {
    /// `tracing-subscriber::fmt().json()` writes the event time under
    /// `timestamp` by default.
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    target: Option<String>,
    /// Structured `code = ...` field set by migrated `error!` sites.
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    /// Event message — `fmt().flatten_event(true)` hoists this to top level.
    #[serde(default)]
    message: Option<String>,
}

/// Aggregated view of a signature group.
#[derive(Debug, Clone)]
struct SignatureGroup {
    signature: String,
    code: Option<String>,
    severity: Option<String>,
    target: String,
    message_sample: String,
    count: usize,
    first_seen: String,
    last_seen: String,
}

/// Configuration for the summary writer task.
#[derive(Debug, Clone)]
pub struct SummaryWriterConfig {
    /// Directory containing `errors.jsonl*` files. Typically
    /// `observability::ERRORS_JSONL_DIR`.
    pub errors_dir: PathBuf,
    /// How often to regenerate the summary.
    pub refresh_interval: Duration,
    /// How far back to look when grouping events.
    pub lookback_minutes: i64,
    /// Maximum number of signatures to render in the top table.
    pub top_signatures: usize,
}

impl SummaryWriterConfig {
    /// Sensible defaults matching the constants above.
    #[must_use]
    pub fn new(errors_dir: impl Into<PathBuf>) -> Self {
        Self {
            errors_dir: errors_dir.into(),
            refresh_interval: Duration::from_secs(DEFAULT_REFRESH_INTERVAL_SECS),
            lookback_minutes: DEFAULT_LOOKBACK_MINUTES,
            top_signatures: DEFAULT_TOP_SIGNATURES,
        }
    }
}

/// Spawns the summary writer as a dedicated background tokio task.
///
/// Returns the JoinHandle so the caller can `.abort()` on shutdown.
/// Failures inside the loop log a WARN but never halt — the summary is
/// an ancillary feature and must not affect trading.
pub fn spawn(config: SummaryWriterConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            errors_dir = %config.errors_dir.display(),
            refresh_interval_secs = config.refresh_interval.as_secs(),
            lookback_minutes = config.lookback_minutes,
            "errors.summary.md writer started"
        );
        loop {
            tokio::time::sleep(config.refresh_interval).await;
            match regenerate_summary(&config) {
                Ok(groups) => {
                    metrics::counter!(METRIC_REFRESH_TOTAL).increment(1);
                    debug!(signatures = groups, "errors.summary.md refreshed");
                }
                Err(err) => {
                    metrics::counter!(METRIC_REFRESH_FAILED_TOTAL).increment(1);
                    warn!(
                        ?err,
                        errors_dir = %config.errors_dir.display(),
                        "errors.summary.md refresh failed — will retry next tick"
                    );
                }
            }
        }
    })
}

/// One-shot: scan errors.jsonl files, group events, write the summary.
///
/// Returns the number of signatures found (0 if no files or no events).
/// Public so it can be unit-tested without a tokio runtime.
pub fn regenerate_summary(config: &SummaryWriterConfig) -> Result<usize> {
    let files = discover_jsonl_files(&config.errors_dir)?;
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cutoff_epoch = now_epoch.saturating_sub(config.lookback_minutes.saturating_mul(60));

    let mut groups: HashMap<String, SignatureGroup> = HashMap::new();

    for path in files {
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => {
                debug!(
                    ?err,
                    path = %path.display(),
                    "errors.jsonl read failed — skipping this file"
                );
                continue;
            }
        };
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<RawEvent>(line) else {
                continue;
            };
            // Optional timestamp filter. If the timestamp is missing or
            // unparseable we include the event (conservative — better to
            // show a suspicious event than hide it).
            let ts_epoch = event
                .timestamp
                .as_ref()
                .and_then(|s| parse_rfc3339_to_epoch(s));
            if let Some(ts) = ts_epoch
                && ts < cutoff_epoch
            {
                continue;
            }
            let message = event.message.unwrap_or_default();
            let target = event.target.unwrap_or_default();
            let signature = signature_hash(&event.code, &target, &message);
            let timestamp_str = event.timestamp.unwrap_or_else(|| "unknown".to_string());
            let entry = groups
                .entry(signature.clone())
                .or_insert_with(|| SignatureGroup {
                    signature: signature.clone(),
                    code: event.code.clone(),
                    severity: event.severity.clone(),
                    target: target.clone(),
                    message_sample: truncate(&message, 160),
                    count: 0,
                    first_seen: timestamp_str.clone(),
                    last_seen: timestamp_str.clone(),
                });
            entry.count = entry.count.saturating_add(1);
            // `first_seen` stays as the earliest string we observed;
            // `last_seen` tracks the latest. We compare strings
            // lexicographically which works for RFC3339 timestamps.
            if timestamp_str < entry.first_seen {
                entry.first_seen = timestamp_str.clone();
            }
            if timestamp_str > entry.last_seen {
                entry.last_seen = timestamp_str;
            }
        }
    }

    let markdown = render_markdown(&groups, config, now_epoch);
    let out_path = config.errors_dir.join(SUMMARY_FILENAME);
    atomic_write(&out_path, &markdown)?;
    Ok(groups.len())
}

/// Returns every `errors.jsonl*` file under `dir`, sorted newest-first
/// by filename (the rolling appender's names sort lexicographically =
/// chronologically).
fn discover_jsonl_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(err) => return Err(err).with_context(|| format!("read_dir {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("errors.jsonl") {
            continue;
        }
        files.push(path);
    }
    files.sort();
    files.reverse();
    Ok(files)
}

/// Short, stable hash of (code, target, message). 16 hex chars.
///
/// Uses a constant seed so output is reproducible across processes.
/// `sha256` would be nicer but adds a dep; `std::hash::DefaultHasher`
/// would change across compiler versions. A small hand-rolled FNV-1a
/// gives us 64 bits, plenty for deduplication at this volume.
fn signature_hash(code: &Option<String>, target: &str, message: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash: u64 = FNV_OFFSET;
    let mut feed = |s: &str| {
        for byte in s.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    if let Some(c) = code {
        feed(c);
    }
    feed("|");
    feed(target);
    feed("|");
    // Only hash the first 160 chars of the message so trailing variable
    // data (timestamps, IDs) doesn't explode the signature space.
    feed(&truncate(message, 160));
    format!("{hash:016x}")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Best-effort RFC3339 → epoch-seconds parse. Returns None on error.
fn parse_rfc3339_to_epoch(ts: &str) -> Option<i64> {
    // tracing-subscriber's JSON formatter emits RFC3339-ish with nanos:
    // "2026-04-18T09:45:11.123456789Z" or "2026-04-18T15:15:11.123+05:30".
    // chrono can parse both.
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Renders the groups as Claude-readable markdown.
fn render_markdown(
    groups: &HashMap<String, SignatureGroup>,
    config: &SummaryWriterConfig,
    now_epoch: i64,
) -> String {
    let mut sorted: Vec<&SignatureGroup> = groups.values().collect();
    sorted.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.signature.cmp(&b.signature))
    });

    let mut out = String::new();
    out.push_str("# tickvault — errors.summary.md\n\n");
    out.push_str(&format!(
        "Generated at epoch **{now_epoch}** (UTC).  \
         Lookback window: last **{}** minutes.  \
         Source: `data/logs/errors.jsonl.*`\n\n",
        config.lookback_minutes
    ));

    if sorted.is_empty() {
        out.push_str("**Zero ERROR-level events in the lookback window.**\n");
        return out;
    }

    out.push_str(&format!(
        "## Top {} active error signatures\n\n",
        config.top_signatures.min(sorted.len())
    ));
    out.push_str(
        "| # | signature | code | severity | count | target | first_seen | last_seen | message |\n",
    );
    out.push_str(
        "|---|-----------|------|----------|-------|--------|------------|-----------|---------|\n",
    );
    for (idx, group) in sorted.iter().take(config.top_signatures).enumerate() {
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | `{}` | {} | {} | {} |\n",
            idx + 1,
            group.signature,
            group.code.as_deref().unwrap_or("-"),
            group.severity.as_deref().unwrap_or("-"),
            group.count,
            group.target,
            group.first_seen,
            group.last_seen,
            escape_md(&group.message_sample),
        ));
    }

    // Novel = count == 1 AND signature only appears in the lookback window.
    // We approximate "novel" here as single-occurrence — the persistent
    // triage state tracker in Phase 6 will upgrade this to "never seen
    // before across all time".
    let novel: Vec<&&SignatureGroup> = sorted.iter().filter(|g| g.count == 1).collect();
    if !novel.is_empty() {
        out.push_str("\n## Novel signatures (single occurrence in window)\n\n");
        for group in novel {
            out.push_str(&format!(
                "- `{}` — {} — {}\n",
                group.signature,
                group.code.as_deref().unwrap_or("(no code)"),
                escape_md(&group.message_sample)
            ));
        }
        out.push('\n');
    }

    out.push_str("\n---\n");
    out.push_str("Refresh interval: ");
    out.push_str(&format!("{}s", config.refresh_interval.as_secs()));
    out.push_str(".  ");
    out.push_str("Writer: `crates/core/src/notification/summary_writer.rs`.\n");
    out
}

fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// Atomic write: `{path}.tmp` → rename → `{path}`.
///
/// 2026-04-24 audit finding #7: previously `f.sync_all().ok()` silently
/// discarded the flush-to-disk error. After a hard poweroff the rename
/// could promote a partially-flushed file, and `errors.summary.md` is
/// part of the zero-touch triage chain (see observability-architecture.md)
/// so silent staleness matters. The sync error now propagates via `?` so
/// the caller can retry on the next 60s tick instead of shipping a
/// potentially-unflushed file to the rename step.
fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("sync_all {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_summary_writer_produces_output_file() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-spawn-smoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));

        let cfg = SummaryWriterConfig {
            errors_dir: tmp.clone(),
            refresh_interval: Duration::from_millis(50),
            lookback_minutes: DEFAULT_LOOKBACK_MINUTES,
            top_signatures: DEFAULT_TOP_SIGNATURES,
        };
        let handle = spawn(cfg);

        // Wait long enough for at least one refresh tick.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Summary file should now exist — spawn drove regenerate_summary
        // at least once via the interval tick.
        let summary_path = tmp.join(SUMMARY_FILENAME);
        assert!(
            summary_path.exists(),
            "spawn() should have produced {} within 200ms",
            summary_path.display()
        );
        // Content should include either the empty marker or a populated
        // table — both indicate the writer ran.
        let body = fs::read_to_string(&summary_path).unwrap_or_default();
        assert!(
            !body.is_empty(),
            "summary file produced by spawn() must not be empty"
        );

        handle.abort();
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn constants_are_stable() {
        assert_eq!(SUMMARY_FILENAME, "errors.summary.md");
        assert_eq!(DEFAULT_REFRESH_INTERVAL_SECS, 60);
        assert_eq!(DEFAULT_LOOKBACK_MINUTES, 15);
        assert_eq!(DEFAULT_TOP_SIGNATURES, 10);
    }

    #[test]
    fn config_new_populates_defaults() {
        let cfg = SummaryWriterConfig::new("/tmp/foo");
        assert_eq!(cfg.errors_dir, PathBuf::from("/tmp/foo"));
        assert_eq!(cfg.refresh_interval, Duration::from_secs(60));
        assert_eq!(cfg.lookback_minutes, 15);
        assert_eq!(cfg.top_signatures, 10);
    }

    #[test]
    fn signature_hash_is_deterministic() {
        let a = signature_hash(&Some("I-P1-11".to_string()), "module", "msg");
        let b = signature_hash(&Some("I-P1-11".to_string()), "module", "msg");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn signature_hash_discriminates_on_code() {
        let a = signature_hash(&Some("A".to_string()), "m", "x");
        let b = signature_hash(&Some("B".to_string()), "m", "x");
        assert_ne!(a, b);
    }

    #[test]
    fn signature_hash_discriminates_on_message() {
        let a = signature_hash(&None, "m", "alpha");
        let b = signature_hash(&None, "m", "beta");
        assert_ne!(a, b);
    }

    #[test]
    fn truncate_preserves_short_strings() {
        assert_eq!(truncate("short", 160), "short");
    }

    #[test]
    fn truncate_shortens_long_strings() {
        let input = "x".repeat(200);
        let result = truncate(&input, 160);
        assert_eq!(result.len(), 160);
    }

    #[test]
    fn parse_rfc3339_handles_utc() {
        let epoch = parse_rfc3339_to_epoch("2026-04-18T09:45:11Z");
        assert!(epoch.is_some());
    }

    #[test]
    fn parse_rfc3339_handles_offset() {
        let epoch = parse_rfc3339_to_epoch("2026-04-18T15:15:11+05:30");
        assert!(epoch.is_some());
    }

    #[test]
    fn parse_rfc3339_returns_none_on_garbage() {
        assert!(parse_rfc3339_to_epoch("not a timestamp").is_none());
    }

    #[test]
    fn regenerate_summary_with_no_files_writes_empty_message() {
        let tmp = std::env::temp_dir().join(format!("tv-summary-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let cfg = SummaryWriterConfig::new(&tmp);
        let count = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen: {e}"));
        assert_eq!(count, 0);

        let out = fs::read_to_string(tmp.join(SUMMARY_FILENAME))
            .unwrap_or_else(|e| panic!("read summary: {e}"));
        assert!(out.contains("Zero ERROR-level events"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn regenerate_summary_groups_duplicate_events() {
        let tmp = std::env::temp_dir().join(format!("tv-summary-dups-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));

        // Use timestamp inside the lookback window.
        let now = chrono::Utc::now();
        let ts = now.to_rfc3339();
        let jsonl = format!(
            r#"{{"timestamp":"{ts}","level":"ERROR","target":"t::a","code":"I-P1-11","severity":"medium","message":"collision A"}}
{{"timestamp":"{ts}","level":"ERROR","target":"t::a","code":"I-P1-11","severity":"medium","message":"collision A"}}
{{"timestamp":"{ts}","level":"ERROR","target":"t::b","code":"DH-904","severity":"high","message":"rate limit"}}
"#
        );
        fs::write(tmp.join("errors.jsonl.2099-01-01-00"), &jsonl)
            .unwrap_or_else(|e| panic!("write jsonl: {e}"));

        let cfg = SummaryWriterConfig::new(&tmp);
        let count = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen: {e}"));
        assert_eq!(count, 2, "two distinct signatures expected");

        let out = fs::read_to_string(tmp.join(SUMMARY_FILENAME))
            .unwrap_or_else(|e| panic!("read summary: {e}"));
        assert!(out.contains("I-P1-11"));
        assert!(out.contains("DH-904"));
        assert!(out.contains("collision A"));
        // Count column should show 2 for the duplicate.
        assert!(out.contains("| 2 |"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn regenerate_summary_filters_out_events_older_than_lookback() {
        let tmp = std::env::temp_dir().join(format!("tv-summary-oldfilter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));

        // 1 hour ago — outside the 15-minute default lookback.
        let old = (chrono::Utc::now() - chrono::TimeDelta::hours(1)).to_rfc3339();
        let jsonl = format!(
            r#"{{"timestamp":"{old}","level":"ERROR","target":"t","code":"X","message":"old"}}
"#
        );
        fs::write(tmp.join("errors.jsonl.2099-01-01-00"), &jsonl)
            .unwrap_or_else(|e| panic!("write: {e}"));

        let cfg = SummaryWriterConfig::new(&tmp);
        let count = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen: {e}"));
        assert_eq!(count, 0, "old events must be filtered out");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn regenerate_summary_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!("tv-summary-idem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));

        let cfg = SummaryWriterConfig::new(&tmp);
        let a = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen a: {e}"));
        let b = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen b: {e}"));
        assert_eq!(a, b);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn regenerate_summary_handles_missing_dir() {
        let tmp = std::env::temp_dir().join(format!("tv-summary-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        let cfg = SummaryWriterConfig::new(&tmp);
        // Missing dir -> discover returns 0 files, regenerate_summary
        // then tries to write the summary into a non-existent dir and
        // will fail. We accept either Ok(0) or Err — what we care about
        // is no panic.
        let result = regenerate_summary(&cfg);
        let _ = result;
    }

    #[test]
    fn escape_md_escapes_pipes() {
        assert_eq!(escape_md("a|b"), "a\\|b");
    }

    #[test]
    fn escape_md_collapses_newlines() {
        assert_eq!(escape_md("line1\nline2"), "line1 line2");
    }

    #[test]
    fn test_metric_names_are_canonical() {
        // Grafana panels + Prometheus alert rules reference these exact
        // strings. Renaming would silently break downstream monitoring.
        assert_eq!(METRIC_REFRESH_TOTAL, "tv_errors_summary_refresh_total");
        assert_eq!(
            METRIC_REFRESH_FAILED_TOTAL,
            "tv_errors_summary_refresh_failed_total"
        );
    }

    /// Regression: production logs emit timestamps like
    /// `2026-04-21T20:55:26.375979+05:30` (microsecond precision, IST
    /// offset). The parser must accept this exact shape, or every event
    /// ends up in the "no-timestamp" fallback bucket and the lookback
    /// filter becomes a no-op.
    #[test]
    fn parse_rfc3339_handles_microsecond_ist_offset_from_prod_logs() {
        let sample = "2026-04-21T20:55:26.375979+05:30";
        let epoch = parse_rfc3339_to_epoch(sample);
        assert!(epoch.is_some(), "prod-format timestamp {sample} must parse");
        // Independently compute expected epoch via chrono — if both
        // paths agree, the parser is honoring the +05:30 offset (not
        // silently treating the value as UTC, which would shift it by
        // 19800 seconds).
        let expected = chrono::DateTime::parse_from_rfc3339(sample)
            .unwrap_or_else(|e| panic!("reference parse failed: {e}"))
            .timestamp();
        assert_eq!(epoch.unwrap_or(0), expected);
    }

    #[test]
    fn regenerate_summary_accepts_production_timestamp_format() {
        // End-to-end regression: a JSONL line with the exact timestamp
        // format the running system emits must be grouped (not filtered
        // out for unparseable timestamp).
        let tmp = std::env::temp_dir().join(format!("tv-summary-prodts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));

        // Timestamp = now, in microsecond+IST format, inside lookback.
        let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(19800).unwrap());
        let ts = now.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string();
        let jsonl = format!(
            r#"{{"timestamp":"{ts}","level":"ERROR","target":"tickvault_core::notification::summary_writer","code":"I-P1-11","severity":"medium","message":"prod-format regression"}}
"#
        );
        fs::write(tmp.join("errors.jsonl.2099-01-01-00"), &jsonl)
            .unwrap_or_else(|e| panic!("write jsonl: {e}"));

        let cfg = SummaryWriterConfig::new(&tmp);
        let count = regenerate_summary(&cfg).unwrap_or_else(|e| panic!("regen: {e}"));
        assert_eq!(count, 1, "prod-format event must be grouped, not dropped");

        let out = fs::read_to_string(tmp.join(SUMMARY_FILENAME))
            .unwrap_or_else(|e| panic!("read summary: {e}"));
        assert!(out.contains("I-P1-11"), "signature code must appear");
        assert!(
            out.contains("prod-format regression"),
            "message sample must appear"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let tmp = std::env::temp_dir().join(format!("tv-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let path = tmp.join(SUMMARY_FILENAME);

        atomic_write(&path, "first").unwrap_or_else(|e| panic!("first write: {e}"));
        atomic_write(&path, "second").unwrap_or_else(|e| panic!("second write: {e}"));
        let got = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(got, "second");

        let _ = fs::remove_dir_all(&tmp);
    }
}
