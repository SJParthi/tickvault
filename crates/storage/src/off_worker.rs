//! Run one blocking step off the tokio worker, at the exact place it blocks.
//!
//! # Why this lives at the I/O site and not at the caller
//!
//! Until 2026-10-03 the frame drain wrapped EVERY tick and depth flush in
//! `block_in_place`, because some flushes block: the synchronous ILP arm (no
//! offload thread) and the inline spill a rescue falls back to. On the
//! production path neither runs. The flush is a `try_send` hand-off to the
//! writer thread, so the drain paid a worker hand-over (the runtime moves the
//! worker's core to another thread) about five times a second for a step that
//! never blocks. The caller could not do better, because only the writer
//! knows whether this particular flush will reach the network or the disk.
//!
//! So the writer marks its own blocking steps with [`off_worker`] and the
//! drain calls the flush bare. A flush that only hands off pays nothing; a
//! flush that writes a spill file or talks to QuestDB still moves off the
//! worker, exactly as before.
//!
//! # Rules
//!
//! * Multi-thread runtime: `tokio::task::block_in_place`, so the worker's
//!   other tasks keep running while this thread blocks.
//! * Current-thread runtime or no runtime at all: run inline. `block_in_place`
//!   panics on a current-thread runtime, and the writer threads (plain OS
//!   threads) have no runtime.
//! * Nested call (already inside `block_in_place` or `spawn_blocking`): runs
//!   inline, which tokio does on its own. Pinned by
//!   `a_nested_call_runs_inline_and_does_not_panic`.
//!
//! The caller still waits for the step. This moves the WORKER, not the
//! wait: the drain cannot continue until its own spill write returns.

/// Runs `step`, moving the current tokio worker aside first when that is
/// possible. See the module docs for when it is and is not.
///
/// O(1) apart from `step` itself: one thread-local read and one compare.
pub fn off_worker<T>(step: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(step)
        }
        _ => step(),
    }
}

#[cfg(test)]
mod tests {
    use super::off_worker;

    #[test]
    fn runs_inline_with_no_runtime() {
        assert_eq!(off_worker(|| 7_u32), 7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_inline_on_a_current_thread_runtime_without_panicking() {
        assert_eq!(off_worker(|| 8_u32), 8);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_on_a_multi_thread_runtime() {
        assert_eq!(off_worker(|| 9_u32), 9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_nested_call_runs_inline_and_does_not_panic() {
        // The app's refold paths still wrap a whole flush in their own
        // `block_in_place`, and the flush reaches this helper inside it.
        let got = tokio::task::block_in_place(|| off_worker(|| off_worker(|| 10_u32)));
        assert_eq!(got, 10);
    }

    /// Wall-clock harness, `#[ignore]`d so a timing never gates CI. It
    /// prices what the drain stopped paying on every hand-off-only flush:
    /// `cargo test -p tickvault-storage --lib off_worker -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "wall-clock timing harness; a timing must never gate CI"]
    async fn block_in_place_cost_against_a_bare_call() {
        const ROUNDS: usize = 10_000;
        let mut hand_over = Vec::with_capacity(ROUNDS);
        let mut bare = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let started = std::time::Instant::now();
            tokio::task::block_in_place(|| std::hint::black_box(1_u32));
            hand_over.push(started.elapsed().as_nanos());
            let started = std::time::Instant::now();
            std::hint::black_box(1_u32);
            bare.push(started.elapsed().as_nanos());
            tokio::task::yield_now().await;
        }
        hand_over.sort_unstable();
        bare.sort_unstable();
        let pct = |v: &[u128], p: usize| v[(v.len() * p / 100).min(v.len() - 1)];
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "block_in_place: p50 {} ns, p99 {} ns; bare call: p50 {} ns, p99 {} ns",
            pct(&hand_over, 50),
            pct(&hand_over, 99),
            pct(&bare, 50),
            pct(&bare, 99),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_inline_inside_spawn_blocking() {
        let got = tokio::task::spawn_blocking(|| off_worker(|| 11_u32))
            .await
            .unwrap_or(0);
        assert_eq!(got, 11);
    }
}
