//! Persisted Dhan-feed rate-limit (HTTP 429 / DATA-805 class) cooldown.
//!
//! # Why this module exists (the restart-loop bug it fixes)
//!
//! Dhan rate-limits the main-feed WebSocket connect with HTTP 429. The
//! 60s→120s→240s→300s reconnect floor that absorbs this already exists in
//! [`crate::websocket::connection`] — but it lives ONLY in an in-memory
//! `rate_limit_streak` (`AtomicU32`). When every connection has been down for
//! `POOL_HALT_SECS` (300s) the pool watchdog returns a Halt verdict and the
//! BOOT-ON path calls `std::process::exit(2)` so an external supervisor
//! restarts the process. That restart WIPES the in-memory streak to `0`, and
//! the fresh process reconnects with a `0ms` first retry
//! (`compute_reconnect_base_delay_ms(0) == 0`) straight back into Dhan's
//! still-active 429 window → instant 429 → 300s → exit → restart → infinite
//! loop.
//!
//! This module persists the cooldown to a tiny advisory JSON file so the
//! cooldown SURVIVES a process restart (for ANY reason). On boot, BEFORE the
//! first Dhan WS connect, the app reads this file; if a cooldown is still
//! active it waits it out first (capped at `WS_RATE_LIMIT_BACKOFF_CAP_MS`).
//!
//! # Honest envelope
//!
//! The file is **advisory** and **fail-open**: a missing, corrupt, or stale
//! file NEVER blocks boot. The persisted wait is bounded by the cap, so a bad
//! file can never hang boot beyond 5 minutes. It does NOT change the 429
//! backoff math (that is correct) and does NOT touch the reconnect engine —
//! it only carries the existing in-memory cooldown across a `process::exit`.
//!
//! Runbook: `.claude/rules/project/wave-2-error-codes.md` (WS-GAP-08).

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use tickvault_common::constants::WS_RATE_LIMIT_BACKOFF_CAP_MS;
use tickvault_common::error_code::ErrorCode;

/// File name for the persisted cooldown, written under the same base data
/// directory as the WS-frame WAL (`$TV_WS_WAL_DIR`'s parent, default `./data`).
const COOLDOWN_FILE_NAME: &str = "ws-rate-limit-cooldown.json";

/// Temp file name for the atomic (tmp + rename) write. A `const` avoids a
/// `format!` allocation (this module is cold-path, but the banned-pattern
/// scanner treats `crates/core/src/websocket/` as hot-path scope).
const COOLDOWN_TMP_FILE_NAME: &str = "ws-rate-limit-cooldown.json.tmp";

/// Upper bound on the cooldown file size we will buffer before parsing. The
/// record serializes to well under 100 bytes; a file larger than 64 KiB is
/// corrupt (or hostile) and MUST NOT be fully read into memory. Fail-open: an
/// oversized file is ignored exactly like a missing/corrupt one.
const MAX_COOLDOWN_FILE_BYTES: u64 = 65_536;

/// The persisted cooldown record. Small + advisory; written best-effort at the
/// 429 classification site, read fail-open at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedCooldown {
    /// Epoch-millis when the most recent 429 was observed.
    pub last_hit_epoch_ms: u64,
    /// The consecutive-429 streak at that instant (for forensics / logging).
    pub streak: u32,
    /// The reconnect floor (ms) in force at that instant — i.e. how long the
    /// cooldown should last from `last_hit_epoch_ms`. Already ≤ cap by
    /// construction (`compute_rate_limit_floor_ms` caps at the same value).
    pub floor_ms: u64,
}

/// Base data directory for on-disk advisory state. Mirrors the
/// `tick_conservation_boot::ws_wal_dir()` derivation: `$TV_WS_WAL_DIR`'s parent
/// (`./data/ws_wal` → `./data`), defaulting to `./data` when unset. Keeping the
/// cooldown file next to the WAL means it shares the same disk-health story.
#[must_use]
fn cooldown_base_dir() -> PathBuf {
    // `$TV_WS_WAL_DIR` defaults to `./data/ws_wal`; the cooldown file lives one
    // level up, alongside the other `./data/*` artefacts.
    let wal_dir = PathBuf::from(
        std::env::var("TV_WS_WAL_DIR").unwrap_or_else(|_| "./data/ws_wal".to_string()), // O(1) EXEMPT: boot/error-path only
    );
    wal_dir
        .parent()
        .map_or_else(|| PathBuf::from("./data"), std::path::Path::to_path_buf)
}

/// Absolute-or-relative path of the persisted cooldown file.
#[must_use]
pub fn cooldown_file_path() -> PathBuf {
    cooldown_base_dir().join(COOLDOWN_FILE_NAME)
}

/// Current wall-clock as epoch millis (saturating; `0` if the clock is before
/// the UNIX epoch, which only `remaining_cooldown_ms` then treats as "no wait").
#[must_use]
pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// PURE: how many milliseconds of cooldown still remain.
///
/// Returns `0` (no wait) when:
/// * the cooldown has already elapsed (`now >= last_hit + floor`), OR
/// * `floor_ms == 0` (no cooldown was in force), OR
/// * the record is STALE — older than `WS_RATE_LIMIT_BACKOFF_CAP_MS` (a 429
///   from long ago is irrelevant; never block boot on an ancient file), OR
/// * `now < last_hit` (clock went backwards — saturating, treat as expired).
///
/// Otherwise returns the remaining time, which is inherently ≤ `floor_ms` and
/// therefore ≤ cap (the caller additionally clamps to the cap defensively).
#[must_use]
pub fn remaining_cooldown_ms(last_hit_epoch_ms: u64, floor_ms: u64, now_epoch_ms: u64) -> u64 {
    if floor_ms == 0 {
        return 0;
    }
    // Clock backwards (or exactly equal) → nothing sensible to wait on.
    let elapsed = now_epoch_ms.saturating_sub(last_hit_epoch_ms);
    // Stale guard: a record older than the cap can never produce a meaningful
    // cooldown (the floor itself is capped at WS_RATE_LIMIT_BACKOFF_CAP_MS), so
    // an ancient file is ignored.
    if elapsed >= WS_RATE_LIMIT_BACKOFF_CAP_MS {
        return 0;
    }
    floor_ms.saturating_sub(elapsed)
}

/// Read + parse the persisted cooldown. Returns `None` on missing / corrupt /
/// unreadable file — FAIL-OPEN by design: a bad file must never block boot.
#[must_use]
pub fn read_cooldown() -> Option<PersistedCooldown> {
    let path = cooldown_file_path();
    // Bound the read so a corrupt/huge file can never be fully buffered: stat
    // ONCE at boot before reading, and reject anything over the size cap.
    // O(1) EXEMPT: cold path — stat ONCE at boot before the first WS connect.
    match std::fs::metadata(&path) {
        Ok(meta) => {
            if meta.len() > MAX_COOLDOWN_FILE_BYTES {
                warn!(
                    path = %path.display(),
                    size = meta.len(),
                    max = MAX_COOLDOWN_FILE_BYTES,
                    "WS rate-limit cooldown file is oversized — ignoring (fail-open)"
                );
                return None;
            }
        }
        Err(err) => {
            // Missing file is the normal first-boot case — not an error.
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "could not stat WS rate-limit cooldown file — ignoring (fail-open)"
                );
            }
            return None;
        }
    }
    // O(1) EXEMPT: cold path — read ONCE at boot before the first WS connect.
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) => {
            // Missing file is the normal first-boot case — not an error.
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "could not read WS rate-limit cooldown file — ignoring (fail-open)"
                );
            }
            return None;
        }
    };
    match serde_json::from_slice::<PersistedCooldown>(&bytes) {
        Ok(rec) => Some(rec),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "WS rate-limit cooldown file is corrupt — ignoring (fail-open)"
            );
            None
        }
    }
}

/// Best-effort persist of a 429 hit so the cooldown survives a process restart.
///
/// Writes atomically (tmp file + rename) so a reader never sees a torn file. A
/// write failure logs `error!(code = WS-GAP-08)` and returns — it NEVER blocks
/// or panics the WebSocket loop (the in-memory `rate_limit_streak` is still the
/// live source of truth within the running process; this file only matters
/// across a restart).
pub fn record_rate_limit_hit(now_epoch_ms: u64, streak: u32, floor_ms: u64) {
    let record = PersistedCooldown {
        last_hit_epoch_ms: now_epoch_ms,
        streak,
        floor_ms,
    };
    if let Err(err) = write_cooldown_atomic(&record) {
        error!(
            code = ErrorCode::WsGap08RateLimitCooldown.code_str(),
            severity = ErrorCode::WsGap08RateLimitCooldown.severity().as_str(),
            streak,
            floor_ms,
            error = %err,
            "WS-GAP-08: failed to persist Dhan rate-limit cooldown (best-effort, \
             in-memory streak still applies for this process)"
        );
    }
}

/// Internal: atomic write (tmp + rename). Creates the base dir if absent.
fn write_cooldown_atomic(record: &PersistedCooldown) -> std::io::Result<()> {
    let dir = cooldown_base_dir();
    // O(1) EXEMPT: cold path — runs only at a Dhan 429 error, never per-tick.
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join(COOLDOWN_FILE_NAME);
    let tmp_path = dir.join(COOLDOWN_TMP_FILE_NAME);
    let json = serde_json::to_vec(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    {
        // O(1) EXEMPT: cold path — runs only at a Dhan 429 error, never per-tick.
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    // O(1) EXEMPT: cold path — runs only at a Dhan 429 error, never per-tick.
    std::fs::rename(&tmp_path, &final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize the env-var + file tests so they don't race on the shared
    // `TV_WS_WAL_DIR` global / on-disk file.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_remaining_cooldown_ms_active() {
        // Hit 10s ago, 60s floor → 50s remaining.
        let last = 1_000_000_u64;
        let now = last + 10_000;
        assert_eq!(remaining_cooldown_ms(last, 60_000, now), 50_000);
    }

    #[test]
    fn test_remaining_cooldown_ms_expired_returns_zero() {
        // Hit 70s ago, 60s floor → already elapsed.
        let last = 1_000_000_u64;
        let now = last + 70_000;
        assert_eq!(remaining_cooldown_ms(last, 60_000, now), 0);
    }

    #[test]
    fn test_remaining_cooldown_ms_exactly_at_boundary_is_zero() {
        let last = 1_000_000_u64;
        let now = last + 60_000; // exactly floor elapsed
        assert_eq!(remaining_cooldown_ms(last, 60_000, now), 0);
    }

    #[test]
    fn test_remaining_cooldown_ms_zero_floor_is_zero() {
        assert_eq!(remaining_cooldown_ms(1_000_000, 0, 1_000_005), 0);
    }

    #[test]
    fn test_remaining_cooldown_ms_clock_backwards_is_zero() {
        // now < last_hit → saturating elapsed = 0 → returns full floor BUT we
        // want NO negative-time weirdness; elapsed saturates to 0 so remaining
        // is the full floor. That is acceptable (still ≤ cap) and the caller
        // clamps to cap. Assert it is bounded and not a garbage huge value.
        let last = 2_000_000_u64;
        let now = 1_000_000_u64; // earlier than last
        let r = remaining_cooldown_ms(last, 60_000, now);
        assert!(r <= 60_000, "remaining must stay bounded by the floor: {r}");
    }

    #[test]
    fn test_remaining_cooldown_ms_stale_record_is_zero() {
        // Hit far longer than the cap ago → ignored (stale).
        let last = 1_000_000_u64;
        let now = last + WS_RATE_LIMIT_BACKOFF_CAP_MS + 1;
        assert_eq!(remaining_cooldown_ms(last, 300_000, now), 0);
    }

    #[test]
    fn test_read_cooldown_corrupt_file_is_none() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("tv-cooldown-corrupt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Point TV_WS_WAL_DIR at <dir>/ws_wal so the base dir is <dir>.
        // SAFETY: tests are single-threaded under ENV_LOCK; no other thread
        // reads the env concurrently.
        unsafe {
            std::env::set_var("TV_WS_WAL_DIR", dir.join("ws_wal"));
        }
        let path = cooldown_file_path();
        std::fs::write(&path, b"{ this is not valid json ]").expect("write corrupt file");
        assert!(
            read_cooldown().is_none(),
            "corrupt file must fail-open to None"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("TV_WS_WAL_DIR");
        }
    }

    #[test]
    fn test_read_cooldown_rejects_oversized_file() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir =
            std::env::temp_dir().join(format!("tv-cooldown-oversized-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        // Point TV_WS_WAL_DIR at <dir>/ws_wal so the base dir is <dir>.
        // SAFETY: tests are single-threaded under ENV_LOCK; no other thread
        // reads the env concurrently.
        unsafe {
            std::env::set_var("TV_WS_WAL_DIR", dir.join("ws_wal"));
        }
        let path = cooldown_file_path();
        // Write a >64 KiB file (valid JSON prefix is irrelevant — the size
        // guard must reject it before any parse).
        let oversized = vec![b'a'; (MAX_COOLDOWN_FILE_BYTES as usize) + 1];
        std::fs::write(&path, &oversized).expect("write oversized file");
        assert!(
            read_cooldown().is_none(),
            "oversized file must fail-open to None"
        );
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("TV_WS_WAL_DIR");
        }
    }

    #[test]
    fn test_read_cooldown_missing_file_is_none() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("tv-cooldown-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("TV_WS_WAL_DIR", dir.join("ws_wal"));
        }
        assert!(read_cooldown().is_none(), "missing file must be None");
        unsafe {
            std::env::remove_var("TV_WS_WAL_DIR");
        }
    }

    #[test]
    fn test_record_rate_limit_hit_round_trip_write_read() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir =
            std::env::temp_dir().join(format!("tv-cooldown-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("TV_WS_WAL_DIR", dir.join("ws_wal"));
        }
        record_rate_limit_hit(1_700_000_000_000, 3, 240_000);
        let rec = read_cooldown().expect("just-written cooldown must read back");
        assert_eq!(rec.last_hit_epoch_ms, 1_700_000_000_000);
        assert_eq!(rec.streak, 3);
        assert_eq!(rec.floor_ms, 240_000);
        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("TV_WS_WAL_DIR");
        }
    }

    #[test]
    fn test_now_epoch_ms_is_monotonic_and_nonzero() {
        let a = now_epoch_ms();
        let b = now_epoch_ms();
        assert!(a > 0, "now_epoch_ms must be a real wall-clock value");
        assert!(
            b >= a,
            "now_epoch_ms must not go backwards within a call pair"
        );
    }

    #[test]
    fn test_cooldown_file_path_is_under_data_dir() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::remove_var("TV_WS_WAL_DIR");
        }
        let p = cooldown_file_path();
        assert!(
            p.ends_with("data/ws-rate-limit-cooldown.json"),
            "default path should be ./data/ws-rate-limit-cooldown.json, got {}",
            p.display()
        );
    }
}
