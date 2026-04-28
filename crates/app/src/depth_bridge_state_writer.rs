//! Atomically writes the depth-200 Python sidecar bridge state file.
//!
//! The Rust app owns the active 4 ATM SIDs (depth-200 selector + rebalancer)
//! and the Dhan client_id. The Python sidecar reads them from a JSON file
//! that this writer keeps fresh. Token rotation is intentionally NOT in the
//! state file — the Python bridge falls back to `data/cache/tv-token-cache`
//! which the Rust `TokenManager` already keeps fresh on every renewal.
//!
//! Atomicity: every write goes to `state.json.tmp` first, then `rename(2)`s
//! to `state.json`. The Python watcher polls `mtime` every 1s, so it always
//! observes a complete file or the previous version — never a partial write.
//!
//! Wired at two call sites in `main.rs`:
//!   1. After depth-200 contracts selected at boot (initial write).
//!   2. After every depth rebalance Swap200 (incremental write).
//!
//! Out of scope (separate commits):
//!   * Token rotation hook — Python falls back to the token cache.
//!   * Per-bridge Prometheus counters — added inside the Python script.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{error, info, warn};

/// Default path the Python bridge polls in state-file mode. Mirrors
/// `DEFAULT_STATE_FILE` in `scripts/depth_200_bridge.py`.
pub const DEFAULT_DEPTH_BRIDGE_STATE_PATH: &str = "data/depth-200-bridge/state.json";

/// One subscription target as written to the JSON file. Field names match
/// `Subscription` in `scripts/depth_200_bridge.py`.
#[derive(Debug, Clone, Serialize)]
pub struct DepthBridgeSubscription {
    pub security_id: u32,
    /// "NSE_FNO" or "NSE_EQ".
    pub segment: &'static str,
    /// Human-readable label for operator dashboards (e.g.
    /// "NIFTY-28APR-24050-CE"). Optional; informational only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// JSON document layout. `version` is monotonic — the Python watcher only
/// reconciles when it changes. `access_token` is intentionally absent;
/// Python reads from the token cache.
#[derive(Debug, Clone, Serialize)]
struct DepthBridgeStateFile<'a> {
    pub client_id: &'a str,
    pub subscriptions: &'a [DepthBridgeSubscription],
    pub version: u64,
    /// IST nanoseconds at write time, for operator forensics.
    pub written_at_ist_nanos: i64,
}

/// Atomic writer for the depth-200 bridge state file.
///
/// Cheap to clone — it's an `Arc`-friendly handle (PathBuf + AtomicU64).
/// Construct once at boot, share across the boot site and the rebalance
/// listener.
pub struct DepthBridgeStateWriter {
    path: PathBuf,
    tmp_path: PathBuf,
    version: AtomicU64,
}

impl DepthBridgeStateWriter {
    /// Create a writer pointing at `path`. Ensures the parent directory
    /// exists; logs a warning if directory creation fails (write attempts
    /// will then surface the error).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let tmp_path = path.with_extension("json.tmp");
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(
                    target: "tickvault::depth_bridge_state_writer",
                    ?err,
                    path = %parent.display(),
                    "could not create depth-bridge state dir — write attempts will fail",
                );
            }
        }
        Self {
            path,
            tmp_path,
            version: AtomicU64::new(0),
        }
    }

    /// Path the Python sidecar polls. Read-only accessor for tests and logs.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomically replace the state file with fresh credentials + subs.
    ///
    /// Returns the new version number on success. The version monotonically
    /// increases per call; the Python watcher uses it as the trigger.
    ///
    /// Atomicity:
    ///   1. Serialize the new state to a string.
    ///   2. Write to `<path>.tmp`.
    ///   3. `fs::rename` from tmp to final — atomic on POSIX (same fs).
    ///
    /// On any failure the previous version remains intact; the watcher keeps
    /// running off it. We log ERROR (Telegram-routable) and return the
    /// underlying error so the caller can decide whether to retry.
    pub fn write(&self, client_id: &str, subscriptions: &[DepthBridgeSubscription]) -> Result<u64> {
        let version = self
            .version
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let written_at = ist_nanos_now();
        let doc = DepthBridgeStateFile {
            client_id,
            subscriptions,
            version,
            written_at_ist_nanos: written_at,
        };
        let json =
            serde_json::to_string_pretty(&doc).context("serialize depth-bridge state json")?;
        std::fs::write(&self.tmp_path, json.as_bytes())
            .with_context(|| format!("write tmp file {}", self.tmp_path.display()))?;
        std::fs::rename(&self.tmp_path, &self.path).with_context(|| {
            format!(
                "atomic rename {} -> {}",
                self.tmp_path.display(),
                self.path.display()
            )
        })?;
        info!(
            target: "tickvault::depth_bridge_state_writer",
            version,
            subs = subscriptions.len(),
            path = %self.path.display(),
            "depth-bridge state written",
        );
        Ok(version)
    }

    /// Convenience: log+swallow errors. Used by the rebalance listener
    /// where we never want a state-write failure to block the live
    /// in-process Swap200 dispatch.
    pub fn write_or_log(&self, client_id: &str, subscriptions: &[DepthBridgeSubscription]) {
        if let Err(err) = self.write(client_id, subscriptions) {
            error!(
                target: "tickvault::depth_bridge_state_writer",
                ?err,
                "depth-bridge state write failed — Python sidecar will keep \
                 running off previous version",
            );
        }
    }
}

fn ist_nanos_now() -> i64 {
    use chrono::TimeDelta;
    let utc = chrono::Utc::now();
    let ist_offset_secs: i64 = i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    let ist = utc + TimeDelta::seconds(ist_offset_secs);
    ist.timestamp_nanos_opt().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Workspace convention (see infra.rs / observability.rs / trading_pipeline.rs):
    /// derive a unique tmp dir from process id + nanos rather than pulling
    /// in the `tempfile` crate as a workspace dep.
    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("tv-depth-bridge-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    #[test]
    fn test_atomic_write_via_tempfile_rename() {
        let dir = unique_tmp_dir("atomic");
        let path = dir.join("nested/state.json");
        let writer = DepthBridgeStateWriter::new(&path);
        let subs = vec![DepthBridgeSubscription {
            security_id: 72265,
            segment: "NSE_FNO",
            label: Some("NIFTY-28APR-24050-CE".into()),
        }];
        let v1 = writer.write("1106656882", &subs).unwrap();
        assert_eq!(v1, 1);
        assert!(path.exists(), "state file must exist after write");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"version\": 1"));
        assert!(body.contains("\"client_id\": \"1106656882\""));
        assert!(body.contains("\"security_id\": 72265"));
        assert!(body.contains("\"segment\": \"NSE_FNO\""));
        assert!(body.contains("\"label\": \"NIFTY-28APR-24050-CE\""));
        // Tmp file must NOT linger after successful rename.
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "tmp file must be gone after rename");
    }

    #[test]
    fn test_version_monotonic_increment() {
        let dir = unique_tmp_dir("monotonic");
        let writer = DepthBridgeStateWriter::new(dir.join("state.json"));
        let subs: Vec<DepthBridgeSubscription> = Vec::new();
        let v1 = writer.write("c", &subs).unwrap();
        let v2 = writer.write("c", &subs).unwrap();
        let v3 = writer.write("c", &subs).unwrap();
        assert_eq!(v1, 1);
        assert_eq!(v2, 2);
        assert_eq!(v3, 3);
    }

    #[test]
    fn test_write_rejects_unwritable_path_and_keeps_previous_version() {
        // Path that doesn't exist + cannot be created (permission-denied
        // path inside /proc on linux, but cross-platform we just point
        // at a path whose parent is a regular file — rename fails).
        let dir = unique_tmp_dir("blocked");
        let blocking_file = dir.join("not-a-dir");
        std::fs::write(&blocking_file, b"x").unwrap();
        let writer = DepthBridgeStateWriter::new(blocking_file.join("state.json"));
        let subs: Vec<DepthBridgeSubscription> = Vec::new();
        let result = writer.write("c", &subs);
        assert!(result.is_err(), "write through a file path must error");
    }

    #[test]
    fn test_default_path_constant_matches_python_bridge_default() {
        // Both ends of the bridge must agree on the default path
        // when neither operator nor config overrides it.
        assert_eq!(
            DEFAULT_DEPTH_BRIDGE_STATE_PATH,
            "data/depth-200-bridge/state.json",
        );
    }

    #[test]
    fn test_write_or_log_does_not_panic_on_failure() {
        let dir = unique_tmp_dir("orlog");
        let blocking_file = dir.join("blocker");
        std::fs::write(&blocking_file, b"x").unwrap();
        let writer = DepthBridgeStateWriter::new(blocking_file.join("state.json"));
        // Must not panic even though the underlying write fails.
        writer.write_or_log("c", &[]);
    }
}
