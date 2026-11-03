//! The own-drain launcher must register the actual OS thread before its
//! future runs and release that registration before reporting completion.
//! A separate integration binary isolates the process-wide registry from
//! the unit tests that intentionally launch several drains concurrently.

use tickvault_app::drain_thread::{DrainJoin, DrainThreadPlan, spawn_frame_drain_with};
use tickvault_storage::hot_thread_sched::{SchedPublisher, registered_drain_thread_id};

#[test]
fn test_spawn_frame_drain_with_registers_scheduler_until_normal_or_panicking_exit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        assert_eq!(registered_drain_thread_id(), 0);
        let shared = spawn_frame_drain_with(
            async { registered_drain_thread_id() },
            DrainThreadPlan::Shared,
            None,
            None,
        );
        assert_eq!(shared.join().await.expect("shared drain"), 0);

        let own = spawn_frame_drain_with(
            async {
                let own_id = std::fs::read_link("/proc/thread-self")
                    .ok()
                    .and_then(|path| path.file_name()?.to_str()?.parse::<i64>().ok());
                let registered = registered_drain_thread_id();
                let mut scheduler = SchedPublisher::new();
                let _read = scheduler.publish();
                (
                    own_id,
                    registered,
                    scheduler.last_second_drain_run_delay_ns(),
                )
            },
            DrainThreadPlan::Own(None),
            None,
            None,
        );
        assert!(own.is_own_thread());
        let (own_id, registered, wait) = own.join().await.expect("own drain");
        match own_id {
            Some(tid) => {
                assert_eq!(registered, tid, "the exact OS thread must be measured");
                assert_eq!(wait, Some(0), "first read establishes its baseline");
            }
            None => {
                assert_eq!(registered, -1, "own mode remains explicit without /proc");
                assert_eq!(wait, None);
            }
        }
        assert_eq!(
            registered_drain_thread_id(),
            0,
            "released before join completes"
        );

        let panicking: DrainJoin<()> = spawn_frame_drain_with(
            async { panic!("simulate own drain failure") },
            DrainThreadPlan::Own(None),
            None,
            None,
        );
        assert!(panicking.join().await.is_err());
        assert_eq!(registered_drain_thread_id(), 0, "released on unwind");
    });
}
