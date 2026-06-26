//! Persist the per-feed runtime toggle across restarts (PR-3 of
//! feed-toggle-full-lifecycle, operator 2026-06-24: the webpage switch choice
//! must STICK across a restart).
//!
//! The live toggle is a runtime `AtomicBool` in [`crate::feed_state`]; it never
//! rewrites the git-tracked, operator-locked `config/base.toml`. Instead the
//! last toggle choice is mirrored to a SEPARATE overlay file
//! `data/feed-state.json`. At boot, `config.feeds` is the DEFAULT and this
//! overlay (if present + valid) wins — so the operator's last webpage choice
//! survives a restart.
//!
//! **Honest envelope:** this changes the *next-boot* effective enabled state,
//! which IS the durable path the operator asked for (persist ON → restart →
//! boots ON; persist OFF → restart → boots OFF). It does NOT change the
//! deferred residual that a feed persisted OFF then toggled ON *at runtime*
//! still can't cold-start the full inline Dhan boot spine from cold (that
//! restart path works precisely because persistence boots it ON).
//!
//! **Atomic + fail-safe:** writes go to `{path}.json.tmp` then `fs::rename`
//! (atomic on the same filesystem) — a crash mid-write never leaves a truncated
//! `feed-state.json`. A missing file → use the config default. A corrupt /
//! partial file → fall back to the config default (logged WARN by the boot
//! caller), NEVER a boot crash.
//!
//! **Path safety (defence-in-depth):** both the directory and the filename are
//! fixed `&'static str` constants — no runtime/operator input ever feeds the
//! path, so traversal is structurally impossible. [`validate_feed_state_path`]
//! pins that invariant so a future refactor that parameterises the path cannot
//! silently introduce a traversal.
//!
//! **GAP-SEC-01:** persistence is invoked INSIDE the existing
//! `POST /api/feeds/{feed}` handler, behind the unchanged bearer-auth
//! `protected_routes` — no new public route, no new attack surface.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::feed_state::FeedStatus;
use tickvault_common::config::FeedsConfig;

/// The pinned data directory the overlay file lives in. Shares the project
/// `data/` directory (alongside `data/spill`, `data/instrument-cache`).
pub const FEED_STATE_DIR: &str = "data";

/// The fixed overlay filename. Combined with [`FEED_STATE_DIR`] this is the
/// ONLY path this module ever reads or writes.
pub const FEED_STATE_FILENAME: &str = "feed-state.json";

/// The on-disk overlay payload — the operator's last per-feed toggle choice.
///
/// `updated_at_ist` is a human-readable IST wall-clock stamp of the write
/// (`"%Y-%m-%d %H:%M:%S"`), purely informational. `#[serde(default)]` so an
/// older file written without the field still parses cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedFeedState {
    pub dhan_enabled: bool,
    pub groww_enabled: bool,
    #[serde(default)]
    pub updated_at_ist: String,
}

/// The canonical overlay path: `data/feed-state.json`.
#[must_use]
pub fn feed_state_path() -> PathBuf {
    PathBuf::from(FEED_STATE_DIR).join(FEED_STATE_FILENAME)
}

/// Defence-in-depth path validation. The path MUST be exactly the canonical
/// `data/feed-state.json` — its file name is [`FEED_STATE_FILENAME`] and its
/// parent is exactly [`FEED_STATE_DIR`]. Anything else is rejected (no read, no
/// write) so a future refactor that parameterises the path cannot introduce a
/// traversal.
#[must_use]
pub fn validate_feed_state_path(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new(FEED_STATE_FILENAME))
        && path.parent() == Some(Path::new(FEED_STATE_DIR))
}

/// Current IST wall-clock as `"%Y-%m-%d %H:%M:%S"` for the `updated_at_ist`
/// stamp (informational only). Uses the project IST offset; never panics.
fn now_ist_stamp() -> String {
    let now_ist = chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64);
    now_ist.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Atomically persist the current per-feed enabled state to `path`.
///
/// The full [`FeedStatus`] snapshot is written (BOTH flags) so the file always
/// reflects the complete desired state — toggling one feed never drops the
/// other feed's persisted value (no lost update).
///
/// Write is `{path}.json.tmp` → `sync_all` → `fs::rename` (atomic on the same
/// filesystem), mirroring `instrument_snapshot::write_plan_snapshot`.
///
/// # Errors
///
/// Returns an error if the path fails validation, the directory can't be
/// created, or any I/O step fails. Callers log this at `error!` (persist
/// failures route to Telegram per audit Rule 5) and proceed — the runtime
/// toggle already applied; only the durable record failed.
pub fn persist_feed_state(status: &FeedStatus, path: &Path) -> std::io::Result<()> {
    if !validate_feed_state_path(path) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to write feed-state to a non-canonical path: {path:?}"),
        ));
    }
    let payload = PersistedFeedState {
        dhan_enabled: status.dhan_enabled,
        groww_enabled: status.groww_enabled,
        updated_at_ist: now_ist_stamp(),
    };
    let json = serde_json::to_vec_pretty(&payload)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        // Durably flush before the rename so a hard power-off can never promote
        // an unflushed tmp into place (matches summary_writer::atomic_write).
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Load the persisted overlay from `path`, if a valid one exists.
///
/// Returns `None` (NOT an error) when:
/// - the path fails validation,
/// - the file is missing (`NotFound`),
/// - the file is unreadable or the JSON is corrupt/partial.
///
/// A `None` makes the boot fall back to the `config/base.toml` default — the
/// fail-safe behaviour. The boot caller logs a WARN for the corrupt case.
#[must_use]
pub fn load_feed_state(path: &Path) -> Option<PersistedFeedState> {
    if !validate_feed_state_path(path) {
        return None;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == ErrorKind::NotFound => return None,
        // Unreadable (permission, IO) — fail-safe to config default.
        Err(_) => return None,
    };
    // Corrupt / partial JSON → None (fail-safe to config default).
    serde_json::from_slice::<PersistedFeedState>(&bytes).ok()
}

/// Overlay the persisted state onto the config default to compute the EFFECTIVE
/// per-feed enabled state the boot uses. Pure function (no I/O) so it is
/// directly unit-testable.
///
/// - `None` (no/corrupt overlay) → the config default is returned unchanged.
/// - `Some(p)` → the persisted per-feed flags win.
#[must_use]
pub fn overlay_feeds(config: FeedsConfig, persisted: Option<PersistedFeedState>) -> FeedsConfig {
    match persisted {
        None => config,
        Some(p) => FeedsConfig {
            dhan_enabled: p.dhan_enabled,
            groww_enabled: p.groww_enabled,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir for one test, mirroring the summary_writer test
    /// pattern — never touches the real `data/` dir.
    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-feed-state-{tag}-{}-{}",
            std::process::id(),
            // nanosecond suffix so two tests with the same tag never collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        dir
    }

    fn status(dhan: bool, groww: bool) -> FeedStatus {
        FeedStatus {
            dhan_enabled: dhan,
            groww_enabled: groww,
            groww_lane_running: false,
            dhan_lane_running: false,
            dhan_disable_allowed: true,
        }
    }

    /// The canonical path is `data/feed-state.json`.
    #[test]
    fn test_feed_state_path_is_canonical() {
        let p = feed_state_path();
        assert_eq!(p, PathBuf::from("data/feed-state.json"));
        assert!(validate_feed_state_path(&p));
    }

    /// Path validation rejects traversal / wrong name / wrong parent.
    #[test]
    fn test_validate_feed_state_path_rejects_traversal() {
        // canonical accepted
        assert!(validate_feed_state_path(Path::new("data/feed-state.json")));
        // wrong filename
        assert!(!validate_feed_state_path(Path::new("data/evil.json")));
        // wrong parent
        assert!(!validate_feed_state_path(Path::new("/etc/feed-state.json")));
        assert!(!validate_feed_state_path(Path::new(
            "data/sub/feed-state.json"
        )));
        // traversal attempts
        assert!(!validate_feed_state_path(Path::new(
            "data/../etc/feed-state.json"
        )));
        assert!(!validate_feed_state_path(Path::new("feed-state.json")));
    }

    /// Write then load returns the same flags (atomic write + load roundtrip).
    #[test]
    fn test_persist_feed_state_writes_atomically_and_round_trips() {
        let dir = unique_tmp_dir("rt");
        // Use a sub-path whose name+parent match the canonical layout so the
        // validation passes against the temp dir's `data` child.
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        persist_feed_state(&status(false, true), &path).expect("persist must succeed");
        let loaded = load_feed_state(&path).expect("load must return Some");
        assert!(!loaded.dhan_enabled);
        assert!(loaded.groww_enabled);
        assert!(
            !loaded.updated_at_ist.is_empty(),
            "updated_at_ist stamp written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// After a write no `.tmp` file is left behind, and the real file parses to
    /// the written flags.
    #[test]
    fn test_feed_state_overlay_atomic_write() {
        let dir = unique_tmp_dir("atomic");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        persist_feed_state(&status(true, true), &path).expect("persist");
        // No orphan tmp.
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "no .tmp file left after atomic rename");
        // Real file parses to the written flags.
        let loaded = load_feed_state(&path).expect("Some");
        assert!(loaded.dhan_enabled && loaded.groww_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot overlay-read: a persisted choice OVERRIDES the config default.
    #[test]
    fn test_boot_overlay_read_overrides_config() {
        // config says dhan ON / groww OFF; the persisted overlay says the
        // opposite — the persisted choice must win.
        let config = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
        };
        let persisted = Some(PersistedFeedState {
            dhan_enabled: false,
            groww_enabled: true,
            updated_at_ist: "2026-06-26 14:31:07".to_string(),
        });
        let effective = overlay_feeds(config, persisted);
        assert!(!effective.dhan_enabled, "persisted dhan-off wins");
        assert!(effective.groww_enabled, "persisted groww-on wins");
    }

    /// No overlay → the config default is returned unchanged.
    #[test]
    fn test_overlay_none_keeps_config_default() {
        let config = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
        };
        let effective = overlay_feeds(config.clone(), None);
        assert_eq!(effective.dhan_enabled, config.dhan_enabled);
        assert_eq!(effective.groww_enabled, config.groww_enabled);
    }

    /// A corrupt overlay file → load None → boot falls back to config default.
    #[test]
    fn test_corrupt_feed_state_falls_back_to_config() {
        let dir = unique_tmp_dir("corrupt");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        std::fs::write(&path, b"{ this is not valid json ").unwrap();
        assert!(
            load_feed_state(&path).is_none(),
            "corrupt JSON must load as None"
        );
        // The boot then uses the config default unchanged.
        let config = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
        };
        let effective = overlay_feeds(config.clone(), load_feed_state(&path));
        assert_eq!(effective.dhan_enabled, config.dhan_enabled);
        assert_eq!(effective.groww_enabled, config.groww_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing file is `None`, not an error (and not a crash).
    #[test]
    fn test_missing_feed_state_file_is_none_not_error() {
        let dir = unique_tmp_dir("missing");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        assert!(!path.exists());
        assert!(load_feed_state(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path that fails validation is rejected on BOTH read and write.
    #[test]
    fn test_non_canonical_path_rejected_on_read_and_write() {
        let dir = unique_tmp_dir("badpath");
        let bad = dir.join("evil.json");
        // write rejected
        let err = persist_feed_state(&status(true, true), &bad)
            .expect_err("write to non-canonical path must error");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        // read rejected → None even if the file exists
        std::fs::write(&bad, b"{\"dhan_enabled\":true,\"groww_enabled\":true}").unwrap();
        assert!(load_feed_state(&bad).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `PersistedFeedState` serde round-trips.
    #[test]
    fn test_persisted_feed_state_serde_round_trip() {
        let s = PersistedFeedState {
            dhan_enabled: false,
            groww_enabled: true,
            updated_at_ist: "2026-06-26 09:00:00".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: PersistedFeedState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    /// An older file without `updated_at_ist` still parses (serde default).
    #[test]
    fn test_persisted_state_without_updated_at_parses() {
        let json = r#"{ "dhan_enabled": true, "groww_enabled": false }"#;
        let s: PersistedFeedState = serde_json::from_str(json).unwrap();
        assert!(s.dhan_enabled);
        assert!(!s.groww_enabled);
        assert!(s.updated_at_ist.is_empty(), "missing field defaults empty");
    }
}
