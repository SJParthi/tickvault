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

/// Runs one blocking task that waits until the runtime is gone (or would
/// be), then sleeps on a tokio timer through `Handle::block_on`, the shape of
/// the tick spill replay round. Returns whether that timer worked.
fn blocking_timer_after_exit(leak: bool) -> bool {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime builds");
    let handle = runtime.handle().clone();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    runtime.spawn_blocking(move || {
        // Wait for the exit to begin, bounded so a broken test cannot hang.
        let _ = go_rx.recv_timeout(Duration::from_secs(1));
        let worked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.block_on(tokio::time::sleep(Duration::from_millis(5)));
        }))
        .is_ok();
        let _ = done_tx.send(worked);
    });
    if leak {
        std::mem::forget(runtime);
        let _ = go_tx.send(());
    } else {
        // The drop shuts the drivers down, then waits for the blocking pool;
        // the task is released only once the drivers are already gone.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = go_tx.send(());
        });
        drop(runtime);
        let _ = release.join();
    }
    done_rx
        .recv_timeout(Duration::from_secs(5))
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
