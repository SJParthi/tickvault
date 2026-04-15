//! Fuzz target: WS frame WAL replay.
//!
//! Feeds random bytes into `WsFrameSpill::replay_all` via a
//! filesystem round-trip, exercising the binary WAL record decoder
//! against arbitrary input. If this target crashes, `replay_all`
//! has undefined behaviour on malformed segments — which would turn
//! a benign disk corruption into a boot-time crash loop.
//!
//! Why this matters: the WAL replay path is the single code path
//! between a SIGKILL'd previous process and the fresh one that must
//! drain its residual frames into QuestDB. A panic here loses every
//! in-flight tick the previous process had written to disk.

#![no_main]

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use libfuzzer_sys::fuzz_target;
use tickvault_storage::ws_frame_spill::replay_all;

/// Fuzz-unique temp dir so concurrent fuzz workers never collide.
fn fuzz_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("tv-wal-fuzz-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create fuzz temp dir");
    p
}

fuzz_target!(|data: &[u8]| {
    // Write the fuzzed bytes as a single .wal segment. `replay_all`
    // walks every .wal file in the directory, validates CRC32, and
    // returns surviving records. Any panic inside that path is a bug.
    let dir = fuzz_tmp();
    let seg = dir.join("fuzz.wal");
    {
        let mut f = match std::fs::File::create(&seg) {
            Ok(f) => f,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        if f.write_all(data).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    // replay_all must NEVER panic, regardless of segment contents.
    // Corrupt records must be skipped with a warn log; truncated
    // tails must stop replay cleanly at the boundary.
    let _ = replay_all(&dir);

    // Cleanup best-effort — a crashing fuzz run may leave the dir
    // behind, which is fine because they live under /tmp.
    let _ = std::fs::remove_dir_all(&dir);
});
