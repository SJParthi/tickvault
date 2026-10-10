//! `main` leaks the tokio runtime instead of dropping it (2026-10-10).
//!
//! Dropping the runtime at exit caused two measured failures on the box:
//! a core dump on 6 Oct 2026 at 15:46:51 IST ("A Tokio 1.x context was found,
//! but it is being shutdown", from a blocking-pool thread still waiting on a
//! timer), and an 85 s exit wait on 9 and 10 Oct while a worker finished the
//! lane's synchronous WAL refold. See the RUNTIME-NOT-DROPPED-AT-EXIT note in
//! `main.rs`.
//!
//! The first test pins the source shape. The second shows the mechanism on a
//! real runtime: a blocking thread that reaches a timer after a dropped
//! runtime has shut its drivers down fails, and the same thread under a
//! leaked runtime completes.

use std::sync::mpsc;
use std::time::Duration;

fn main_fn_body() -> &'static str {
    let src = include_str!("../src/main.rs");
    let rest = src
        .split_once("\nfn main() -> Result<()> {")
        .expect("main.rs must define `fn main() -> Result<()>`")
        .1;
    &rest[..rest.find("\n}\n").expect("end of fn main")]
}

#[test]
fn main_leaks_the_runtime_after_block_on_and_never_drops_it() {
    let body = main_fn_body();
    let block_on = body
        .find("let outcome = runtime.block_on(async_main());")
        .expect("main must bind the outcome of block_on, not return it as the tail expression");
    let forget = body
        .find("std::mem::forget(runtime);")
        .expect("main must leak the runtime");
    assert!(
        block_on < forget,
        "the runtime is leaked after block_on returns"
    );
    assert!(
        body[forget..].trim_end().ends_with("outcome"),
        "nothing may run between the leak and the return"
    );
    for dropping in [
        "drop(runtime)",
        "runtime.shutdown_timeout(",
        "runtime.shutdown_background(",
    ] {
        assert!(
            !body.contains(dropping),
            "`{dropping}` shuts the drivers down under live blocking threads"
        );
    }
}

/// Runs one blocking task that, once the exit begins, keeps sleeping on a
/// tokio timer through `Handle::block_on` (the shape of the tick spill
/// replay round) until a sleep fails or `WATCH` passes. Returns whether
/// every sleep worked.
///
/// The task does not guess when the drop has shut the drivers down: it
/// retries until it sees the failure, so a slow runner only makes the loop
/// longer, never the verdict different. `WATCH` bounds a broken test.
fn blocking_timer_after_exit(leak: bool) -> bool {
    const WATCH: Duration = Duration::from_secs(5);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime builds");
    let handle = runtime.handle().clone();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    runtime.spawn_blocking(move || {
        let _ = go_rx.recv_timeout(WATCH);
        let deadline = std::time::Instant::now() + WATCH;
        let mut worked = true;
        while worked && std::time::Instant::now() < deadline {
            worked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle.block_on(tokio::time::sleep(Duration::from_millis(5)));
            }))
            .is_ok();
            if leak {
                // One clean sleep after the leak is the whole claim; a few
                // more show it holds without running for the full watch.
                std::thread::sleep(Duration::from_millis(20));
                if deadline.saturating_duration_since(std::time::Instant::now())
                    < WATCH - Duration::from_millis(200)
                {
                    break;
                }
            }
        }
        let _ = done_tx.send(worked);
    });
    let _ = go_tx.send(());
    if leak {
        std::mem::forget(runtime);
    } else {
        // The drop shuts the drivers down, then waits for the blocking pool,
        // so the task's retries see the shutdown before the drop returns.
        drop(runtime);
    }
    done_rx
        .recv_timeout(WATCH * 2)
        .expect("the blocking task reports")
}

#[test]
fn a_blocking_timer_fails_after_a_dropped_runtime_and_works_after_a_leaked_one() {
    assert!(
        !blocking_timer_after_exit(false),
        "bite check: dropping the runtime must break a blocking thread's timer, \
         which is the 6 Oct core dump under panic = \"abort\""
    );
    assert!(
        blocking_timer_after_exit(true),
        "a leaked runtime keeps its drivers, so the same timer completes"
    );
}
