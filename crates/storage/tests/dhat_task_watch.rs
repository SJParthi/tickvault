//! DHAT zero-allocation ratchet for plan item 4 (2026-10-30): the
//! background-task heartbeat beat, the once-a-second heartbeat publish, and
//! one memory-ladder poll (read `VmRSS`, decide, publish).
//!
//! `beat` runs once per pass of every watched task (the seal writer beats
//! every 100 ms); the publish and the poll run forever on the
//! `tv-task-watch` thread. An allocation in any of them would be a slow leak
//! on a thread nobody watches.
//!
//! Honest scope: no metrics recorder is installed here (this crate has none
//! as a dependency), so gauge and counter handles are the no-op kind. What
//! this proves is that the code around them allocates nothing; the
//! production recorder's own per-sample cost is not measured here.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::path::Path;
use std::time::Instant;

use tickvault_common::config::{MemoryWatchdogConfig, MemoryWatchdogMode};
use tickvault_storage::memory_watchdog::{MemoryWatchdog, PressureLevel, read_vmrss_bytes};
use tickvault_storage::resource_monitor::DEFAULT_PROC_SELF_STATUS_PATH;
use tickvault_storage::task_heartbeat::{
    ALL_WATCHED_TASKS, HeartbeatPublisher, beat, publish_once,
};

mod dhat_support;

const GIB: u64 = 1 << 30;

#[test]
fn dhat_heartbeat_beat_publish_and_memory_poll_are_zero_allocation() {
    // Warm up: fix the anchor, resolve every handle, and take one read so
    // the first-use costs are outside the measured window.
    for task in ALL_WATCHED_TASKS {
        beat(task);
    }
    let mut publisher = HeartbeatPublisher::resolve();
    let _ = publish_once(&mut publisher, Instant::now());
    let status = Path::new(DEFAULT_PROC_SELF_STATUS_PATH);
    let _ = read_vmrss_bytes(status);
    // Shadow mode, steady at `shed`: every poll decides and counts, and the
    // level never changes, so no log line is written in the window.
    let mut dog = MemoryWatchdog::new(
        MemoryWatchdogConfig {
            mode: MemoryWatchdogMode::Shadow,
            ..MemoryWatchdogConfig::default()
        },
        Some(10 * GIB),
    );
    let _ = dog.step(Some(8 * GIB + GIB / 2), 0, true);
    assert_eq!(dog.level(), PressureLevel::Shed);

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for i in 0..50_000_usize {
                beat(ALL_WATCHED_TASKS[i % ALL_WATCHED_TASKS.len()]);
            }
        },
    );
    assert_eq!((bytes, blocks), (0, 0), "beat allocated");

    let (p_bytes, p_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for _ in 0..10_000 {
                let stuck = publish_once(&mut publisher, Instant::now());
                assert_eq!(stuck, 0);
            }
        },
    );
    assert_eq!((p_bytes, p_blocks), (0, 0), "publish_once allocated");

    let (m_bytes, m_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for i in 0..10_000_u64 {
                // Every 5 s of watchdog time, steady at shed.
                let d = dog.step(Some(8 * GIB + GIB / 2), (i + 1) * 5, true);
                assert_eq!(d.level, PressureLevel::Shed);
            }
        },
    );
    assert_eq!(
        (m_bytes, m_blocks),
        (0, 0),
        "MemoryWatchdog::step allocated"
    );

    #[cfg(target_os = "linux")]
    {
        let (r_bytes, r_blocks) = dhat_support::measure_with_phantom_retry(
            0,
            0,
            || {},
            || {
                for _ in 0..1_000 {
                    assert!(read_vmrss_bytes(status).is_some());
                }
            },
        );
        assert_eq!((r_bytes, r_blocks), (0, 0), "read_vmrss_bytes allocated");
    }
}
