//! Groww NATIVE-RUST shadow client — app-side supervised runner (PR-R1,
//! operator "go" 2026-07-04; `groww-second-feed-scope-2026-06-19.md` §35).
//!
//! DEFAULT-OFF behind `[feeds] groww_native_shadow`. When enabled, this task
//! runs ALONGSIDE the Python sidecar (never instead of it):
//!
//! 1. Wait for TODAY's Rust-built watch file
//!    (`data/groww/groww-watch-<date>.json` — the same single source the
//!    sidecar subscribes from), polling every
//!    `GROWW_NATIVE_WATCH_POLL_SECS`.
//! 2. Parse it → subject map; spawn the shadow NDJSON writer; run the native
//!    client (`tickvault_core::feed::groww::native::client`).
//! 3. At the IST day boundary (+ a grace period for the new watch build) the
//!    session recycles so tomorrow streams tomorrow's universe.
//!
//! Supervision mirrors WS-GAP-05 / FEED-SUPERVISOR-01: the runner task is
//! respawned on death with a bounded backoff (`tv_groww_native_respawn_total`)
//! so a panic can never silently end the shadow capture. Shadow-only: nothing
//! here touches the production Dhan or Groww-sidecar chains.

use std::path::PathBuf;
use std::time::Duration;

use tickvault_common::constants::{
    GROWW_NATIVE_SHADOW_NDJSON_PATH, GROWW_NATIVE_WATCH_POLL_SECS,
    GROWW_NATIVE_WRITER_CHANNEL_CAPACITY, IST_UTC_OFFSET_SECONDS_I64,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_core::feed::groww::native::client::run_native_shadow_client;
use tickvault_core::feed::groww::native::shadow_writer::run_shadow_writer;
use tickvault_core::feed::groww::watch_reader::{build_subject_map, parse_watch_file};
use tracing::{error, info};

/// Backoff between supervisor respawns of a dead runner task.
const RESPAWN_BACKOFF_SECS: u64 = 5;

// Re-homed 2026-07-15 (Groww live-feed retirement): the pure watch-path /
// day-roll primitives now live in `crate::groww_watch_paths` so the KEEP
// spot-1m REST leg does not depend on this shadow module.
pub use crate::groww_watch_paths::{secs_until_day_roll, watch_file_path_for};

/// Today's IST date as `YYYY-MM-DD` (computed per loop turn, never frozen at
/// boot — an overnight run must roll to the new day's watch file).
fn today_ist_date() -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64))
        .format("%Y-%m-%d")
        .to_string()
}

/// Spawn the SUPERVISED native-shadow runner. Call ONLY when
/// `feeds.groww_native_shadow` is true — the flag is the kill switch and the
/// default is OFF.
/// The pure primitives (watch_file_path_for / secs_until_day_roll) and the
/// core client/writer/reader surfaces are unit-tested in their own modules.
// TEST-EXEMPT: spawn wrapper over the supervised loop; primitives unit-tested.
pub fn spawn_supervised_groww_native_shadow(cache_dir: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let inner = tokio::spawn(run_native_shadow_forever(cache_dir.clone()));
            let reason = match inner.await {
                Ok(()) => "clean_exit",
                Err(e) if e.is_panic() => "panic",
                Err(_) => "cancelled",
            };
            metrics::counter!("tv_groww_native_respawn_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::GrowwNative01ConnectFailed.code_str(),
                reason,
                "GROWW-NATIVE-01: shadow runner task ended — respawning \
                 (shadow-only; production capture unaffected)"
            );
            tokio::time::sleep(Duration::from_secs(RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// The runner loop: one session per IST trading day's watch file.
// TEST-EXEMPT: file-polling + task-orchestration loop over unit-tested
// primitives (parse_watch_file / build_subject_map / run_shadow_writer /
// run_native_shadow_client / secs_until_day_roll).
async fn run_native_shadow_forever(cache_dir: PathBuf) {
    loop {
        // ── 1. Wait for TODAY's watch file ─────────────────────────────
        let date = today_ist_date();
        let watch_path = watch_file_path_for(&cache_dir, &date);
        let json = match tokio::fs::read_to_string(&watch_path).await {
            Ok(json) => json,
            Err(_) => {
                // Not built yet (feed off / pre-activation / holiday) — poll.
                tokio::time::sleep(Duration::from_secs(GROWW_NATIVE_WATCH_POLL_SECS)).await;
                continue;
            }
        };
        let subjects = match parse_watch_file(&json).and_then(|doc| build_subject_map(&doc)) {
            Ok((map, skipped)) => {
                info!(
                    subjects = map.len(),
                    skipped,
                    watch = %watch_path.display(),
                    "groww native shadow: watch file loaded"
                );
                map
            }
            Err(e) => {
                error!(
                    code = ErrorCode::GrowwNative03DecodeFailed.code_str(),
                    error = %e,
                    watch = %watch_path.display(),
                    "GROWW-NATIVE-03: watch file unusable — retrying after poll \
                     interval (fail-closed: never subscribing a guessed universe)"
                );
                tokio::time::sleep(Duration::from_secs(GROWW_NATIVE_WATCH_POLL_SECS)).await;
                continue;
            }
        };

        // ── 2. Writer + client for this trading day ────────────────────
        let (tick_tx, tick_rx) = tokio::sync::mpsc::channel(GROWW_NATIVE_WRITER_CHANNEL_CAPACITY);
        let writer = tokio::spawn(run_shadow_writer(
            tick_rx,
            PathBuf::from(GROWW_NATIVE_SHADOW_NDJSON_PATH),
        ));

        let roll_secs = secs_until_day_roll(chrono::Utc::now().timestamp());
        tokio::select! {
            () = run_native_shadow_client(subjects, tick_tx) => {
                // Client exited (writer gone) — the writer task is already
                // draining to completion below.
            }
            () = tokio::time::sleep(Duration::from_secs(roll_secs)) => {
                info!("groww native shadow: IST day rolled — recycling the session");
                // Dropping the select arm cancels the client (its tick_tx
                // drops with it), which closes the writer channel.
            }
        }
        // Let the writer drain + exit before the next session (best-effort).
        if writer.await.is_err() {
            error!(
                code = ErrorCode::GrowwNative04WriterFailed.code_str(),
                "GROWW-NATIVE-04: shadow writer task panicked — a fresh writer \
                 spawns with the next session"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // watch_file_path_for / secs_until_day_roll tests moved to
    // `crate::groww_watch_paths` with the fns (2026-07-15 re-home).
}
