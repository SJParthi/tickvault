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
    // No traversal segment anywhere in the path.
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    // The file name MUST be exactly the fixed overlay filename ...
    if path.file_name() != Some(std::ffi::OsStr::new(FEED_STATE_FILENAME)) {
        return false;
    }
    // ... and its immediate parent directory MUST be named exactly
    // `FEED_STATE_DIR` (the `data` dir). The parent may be relative
    // (`data/feed-state.json`, production) or absolute under a test temp dir
    // (`/tmp/.../data/feed-state.json`) — in both cases the LAST component of
    // the parent is `data`, which is the invariant that matters for traversal
    // safety; production always uses the relative canonical path.
    match path.parent().and_then(Path::file_name) {
        Some(parent_name) => parent_name == std::ffi::OsStr::new(FEED_STATE_DIR),
        None => false,
    }
}

/// Current IST wall-clock as `"%Y-%m-%d %H:%M:%S"` for the `updated_at_ist`
/// stamp (informational only). Uses the project IST offset; never panics.
fn now_ist_stamp() -> String {
    let now_ist = chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64);
    now_ist.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// A UNIQUE temp path next to `path`, e.g. `data/feed-state.json.<pid>.<nanos>.tmp`.
///
/// Per-write uniqueness (process id + nanosecond clock) is the fix for the
/// 3-agent hostile finding that a FIXED `.tmp` name lets two concurrent toggles
/// truncate each other's tmp and `rename` a torn file into place. With a unique
/// tmp per write, each writer fully writes + fsyncs ITS OWN tmp, then atomically
/// renames — so concurrent toggles are correct last-writer-wins and the on-disk
/// `feed-state.json` is ALWAYS a complete, valid snapshot (never torn). Mirrors
/// the unique-tmp-dir pattern already used by this module's tests + summary_writer.
fn unique_tmp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from(FEED_STATE_FILENAME));
    name.push(format!(".{}.{}.tmp", std::process::id(), nanos));
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Create the temp file write-only, truncating, with mode `0o600` on Unix
/// (owner read/write only — defence-in-depth; the file carries no secrets but
/// least-privilege is good practice). On non-Unix the mode is not set (the
/// `OpenOptions` still creates+truncates), keeping the code cross-platform.
fn create_tmp_file(tmp_path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(tmp_path)
}

/// Atomically persist the current per-feed enabled state to `path`.
///
/// The full [`FeedStatus`] snapshot is written (BOTH flags) so the file always
/// reflects the complete desired state — toggling one feed never drops the
/// other feed's persisted value (no lost update).
///
/// Write is a UNIQUE `{path}.<pid>.<nanos>.tmp` → `sync_all` → `fs::rename`
/// (atomic on the same filesystem). The unique tmp name (vs the shared
/// `instrument_snapshot` `.json.tmp`) is the 3-agent fix so two concurrent
/// toggles can't truncate each other's tmp and promote a torn file —
/// concurrent same-feed toggles are eventually-consistent last-writer-wins,
/// and the on-disk file is ALWAYS a complete valid snapshot. The unique tmp is
/// removed on any error path so a failed write never leaks a stray tmp.
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
    let tmp_path = unique_tmp_path(path);
    // Write+fsync the unique tmp, then atomically rename. On ANY failure clean
    // up the unique tmp (a stray would never be promoted, but leave no litter).
    let write_then_rename = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = create_tmp_file(&tmp_path)?;
        f.write_all(&json)?;
        // Durably flush before the rename so a hard power-off can never promote
        // an unflushed tmp into place (matches summary_writer::atomic_write).
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp_path, path)
    };
    write_then_rename().inspect_err(|_| {
        // Best-effort cleanup — a stray tmp is harmless (never promoted).
        // Explicit Err arm satisfies clippy::let_underscore_must_use.
        if let Err(cleanup_err) = std::fs::remove_file(&tmp_path) {
            tracing::debug!(?cleanup_err, "failed to remove stray feed-state tmp file");
        }
    })
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
/// - `Some(p)` → the persisted GROWW flag wins (both directions, unchanged);
///   the persisted DHAN flag can only ever NARROW the config, never WIDEN it:
///   effective dhan = `config.dhan_enabled && p.dhan_enabled`.
///
/// **Why the Dhan AND-gate (operator directive 2026-07-13):** the Dhan live
/// WS feed was removed from the runtime ("now remove this entire Dhan live
/// websocket feed instruments subscription even entire live websocket feed
/// itself") — Dhan is REST-only, Groww is the live feed. Config-off is
/// AUTHORITATIVE for Dhan: a stale `data/feed-state.json` written by a
/// webpage toggle BEFORE the directive (`dhan_enabled: true`) must not
/// resurrect the retired lane over the operator-locked config. Use
/// [`dhan_overlay_suppressed`] at the application site to log the
/// suppression (this fn stays pure).
#[must_use]
pub fn overlay_feeds(config: FeedsConfig, persisted: Option<PersistedFeedState>) -> FeedsConfig {
    match persisted {
        None => config,
        // The persisted overlay carries ONLY the runtime toggles; the
        // `[feeds.groww]` tuning (auto-scale §34) always comes from config.
        Some(p) => FeedsConfig {
            // 2026-07-13: narrow-only for Dhan — config-off wins over any
            // persisted-on (the retired live WS lane can never be
            // overlay-resurrected); config-on + persisted-off still honors
            // the operator's last disable (pre-Phase-A behaviour).
            dhan_enabled: config.dhan_enabled && p.dhan_enabled,
            groww_enabled: p.groww_enabled,
            groww: config.groww,
            // PR-R1 (2026-07-04): the native-shadow flag is CONFIG-ONLY (no
            // runtime toggle in R1) — always from config, never persisted.
            groww_native_shadow: config.groww_native_shadow,
        },
    }
}

/// True when [`overlay_feeds`] would SUPPRESS a widening Dhan overlay —
/// i.e. the persisted `data/feed-state.json` says `dhan_enabled: true` while
/// the config says `false`. Pure companion predicate so the boot site can
/// log ONE `warn!` naming the 2026-07-13 operator directive without this
/// module doing any I/O or logging.
#[must_use]
pub fn dhan_overlay_suppressed(
    config: &FeedsConfig,
    persisted: Option<&PersistedFeedState>,
) -> bool {
    matches!(persisted, Some(p) if p.dhan_enabled && !config.dhan_enabled)
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

    /// After a write NO `.tmp` file (unique-named or otherwise) is left behind in
    /// the directory, and the real file parses to the written flags.
    #[test]
    fn test_feed_state_overlay_atomic_write() {
        let dir = unique_tmp_dir("atomic");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        persist_feed_state(&status(true, true), &path).expect("persist");
        // No orphan tmp of ANY name (the unique `.<pid>.<nanos>.tmp` is renamed
        // away; scan the whole data dir so a unique-named leftover is caught too).
        let leftover_tmp = std::fs::read_dir(&data)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".tmp"));
        assert!(!leftover_tmp, "no .tmp file left after atomic rename");
        // Real file parses to the written flags.
        let loaded = load_feed_state(&path).expect("Some");
        assert!(loaded.dhan_enabled && loaded.groww_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temp path is UNIQUE per write (pid + nanos), so two concurrent writers
    /// never share a tmp name and cannot truncate each other into a torn file.
    /// Two back-to-back writes use distinct tmp names AND the on-disk file is
    /// always a complete valid snapshot reflecting the last write (FIX 1).
    #[test]
    fn test_unique_tmp_path_differs_per_call_and_last_write_wins() {
        let dir = unique_tmp_dir("unique-tmp");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        // Two unique tmp names for the same final path differ (pid same, nanos differ).
        let a = unique_tmp_path(&path);
        let b = unique_tmp_path(&path);
        assert_ne!(a, b, "unique tmp path must differ between calls");
        assert!(
            a.to_string_lossy().ends_with(".tmp")
                && a.parent() == Some(data.as_path())
                && a.to_string_lossy().contains(FEED_STATE_FILENAME),
            "unique tmp sits next to the final file and ends with .tmp"
        );
        // Last-writer-wins: write two different snapshots; the final file is the
        // last one, always a complete valid snapshot.
        persist_feed_state(&status(true, false), &path).expect("first persist");
        persist_feed_state(&status(false, true), &path).expect("second persist");
        let loaded = load_feed_state(&path).expect("Some");
        assert!(
            !loaded.dhan_enabled && loaded.groww_enabled,
            "last write wins"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On Unix the overlay file is created with mode `0o600` (owner-only) —
    /// defence-in-depth least privilege (FIX 2).
    #[cfg(unix)]
    #[test]
    fn test_persisted_file_has_owner_only_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = unique_tmp_dir("perms");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        persist_feed_state(&status(true, true), &path).expect("persist");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "feed-state file must be owner read/write only");
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    /// `load_feed_state` returns `None` for a missing canonical-shaped path
    /// (named so the pub-fn-test-guard maps it directly to `load_feed_state`).
    #[test]
    fn test_load_feed_state_none_when_absent() {
        let dir = unique_tmp_dir("loadnone");
        let data = dir.join(FEED_STATE_DIR);
        std::fs::create_dir_all(&data).unwrap();
        let path = data.join(FEED_STATE_FILENAME);
        assert!(load_feed_state(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `overlay_feeds` lets a persisted choice win and is identity for `None`
    /// (named so the pub-fn-test-guard maps it directly to `overlay_feeds`).
    #[test]
    fn test_overlay_feeds_persisted_wins_and_none_is_identity() {
        let cfg = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
            ..Default::default()
        };
        let identity = overlay_feeds(cfg.clone(), None);
        assert_eq!(identity.dhan_enabled, cfg.dhan_enabled);
        assert_eq!(identity.groww_enabled, cfg.groww_enabled);
        let eff = overlay_feeds(
            cfg,
            Some(PersistedFeedState {
                dhan_enabled: false,
                groww_enabled: true,
                updated_at_ist: String::new(),
            }),
        );
        assert!(!eff.dhan_enabled && eff.groww_enabled);
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

    /// 2026-07-13 operator directive ("now remove this entire Dhan live
    /// websocket feed... even entire live websocket feed itself"): a STALE
    /// overlay written before the directive (`dhan_enabled: true`) must NOT
    /// widen a config-off Dhan — config-off is authoritative for the retired
    /// live WS lane. Effective dhan = config && persisted.
    #[test]
    fn test_overlay_feeds_dhan_and_gate_suppresses_stale_widen() {
        let config = FeedsConfig {
            dhan_enabled: false,
            groww_enabled: true,
            ..Default::default()
        };
        let persisted = Some(PersistedFeedState {
            dhan_enabled: true, // stale pre-directive webpage toggle
            groww_enabled: true,
            updated_at_ist: "2026-07-12 15:00:00".to_string(),
        });
        let effective = overlay_feeds(config, persisted);
        assert!(
            !effective.dhan_enabled,
            "config dhan-off must win over a stale persisted dhan-on \
             (2026-07-13 directive: the live WS lane is retired)"
        );
        assert!(effective.groww_enabled, "groww overlay unaffected");
    }

    /// The narrow direction is unchanged: config dhan-on + persisted
    /// dhan-off → OFF (the operator's last runtime disable still sticks,
    /// exactly as before Phase A).
    #[test]
    fn test_overlay_feeds_dhan_narrow_still_wins() {
        let config = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
            ..Default::default()
        };
        let persisted = Some(PersistedFeedState {
            dhan_enabled: false,
            groww_enabled: false,
            updated_at_ist: String::new(),
        });
        let effective = overlay_feeds(config, persisted);
        assert!(
            !effective.dhan_enabled,
            "persisted dhan-off must still narrow a config dhan-on"
        );
    }

    /// Groww overlay semantics are UNCHANGED by the 2026-07-13 Dhan
    /// AND-gate: the persisted Groww choice wins in BOTH directions
    /// (widen and narrow).
    #[test]
    fn test_overlay_feeds_groww_semantics_unchanged() {
        // Widen: config groww-off + persisted groww-on → ON.
        let widened = overlay_feeds(
            FeedsConfig {
                dhan_enabled: false,
                groww_enabled: false,
                ..Default::default()
            },
            Some(PersistedFeedState {
                dhan_enabled: false,
                groww_enabled: true,
                updated_at_ist: String::new(),
            }),
        );
        assert!(widened.groww_enabled, "persisted groww-on must still widen");
        // Narrow: config groww-on + persisted groww-off → OFF.
        let narrowed = overlay_feeds(
            FeedsConfig {
                dhan_enabled: false,
                groww_enabled: true,
                ..Default::default()
            },
            Some(PersistedFeedState {
                dhan_enabled: false,
                groww_enabled: false,
                updated_at_ist: String::new(),
            }),
        );
        assert!(
            !narrowed.groww_enabled,
            "persisted groww-off must still narrow"
        );
    }

    /// `dhan_overlay_suppressed` fires ONLY on the widening combination
    /// (config off + persisted on) — the one the 2026-07-13 AND-gate
    /// suppresses — so the boot warn can never false-fire.
    #[test]
    fn test_dhan_overlay_suppressed_predicate() {
        let cfg_off = FeedsConfig {
            dhan_enabled: false,
            groww_enabled: true,
            ..Default::default()
        };
        let cfg_on = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: true,
            ..Default::default()
        };
        let p = |dhan: bool| {
            Some(PersistedFeedState {
                dhan_enabled: dhan,
                groww_enabled: true,
                updated_at_ist: String::new(),
            })
        };
        assert!(
            dhan_overlay_suppressed(&cfg_off, p(true).as_ref()),
            "config off + persisted on = suppressed (warn fires)"
        );
        assert!(!dhan_overlay_suppressed(&cfg_off, p(false).as_ref()));
        assert!(!dhan_overlay_suppressed(&cfg_on, p(true).as_ref()));
        assert!(!dhan_overlay_suppressed(&cfg_on, p(false).as_ref()));
        assert!(!dhan_overlay_suppressed(&cfg_off, None));
    }
}
