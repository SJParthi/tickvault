//! Crate-wide test support utilities.
//!
//! # Why this module exists
//!
//! Rust 2024 promoted `std::env::set_var` and `std::env::remove_var` to
//! `unsafe` because every call races with any other thread that happens
//! to be reading the process env (libc's `getenv`/`setenv` share one
//! non-thread-safe global table). Under `cargo test` default
//! parallelism, multiple `#[test]` functions can interleave these
//! mutations, producing a data race that `ThreadSanitizer` flags.
//!
//! GitHub issue #304 documented the live TSan finding on 2026-04-20.
//! PR #354 added a local `ENV_MUTATION_LOCK` inside `middleware.rs`'s
//! test module — but that fix was incomplete: the `handlers/debug.rs`
//! test module **also** mutates `LOGS_DIR_ENV` via the same `unsafe
//! set_var` API. Both files compile into the same test binary
//! (`tickvault-api` unit tests), so their parallel-running tests can
//! race even across different env var *names* — the underlying libc
//! table is shared.
//!
//! # The fix
//!
//! This module exposes a single crate-wide `env_lock()` helper that
//! returns a `MutexGuard` held for the scope of the caller. Every
//! test that calls `unsafe { std::env::set_var(...) }` or
//! `unsafe { std::env::remove_var(...) }` **must** acquire this lock
//! first:
//!
//! ```rust,ignore
//! use crate::test_support::env_lock;
//!
//! #[test]
//! fn my_env_test() {
//!     let _guard = env_lock();
//!     unsafe { std::env::set_var("MY_VAR", "value") };
//!     // ... test body ...
//!     unsafe { std::env::remove_var("MY_VAR") };
//!     // `_guard` drops at end of scope, releasing the mutex.
//! }
//! ```
//!
//! Because the lock is a single `OnceLock<Mutex<()>>` at the crate
//! level, every env-mutating test across every module in this crate
//! acquires the same underlying mutex — so they serialise regardless
//! of which env var they're touching.
//!
//! # Poisoned-lock recovery
//!
//! If a test panics while holding the lock, the mutex becomes
//! poisoned. We recover via `PoisonError::into_inner` rather than
//! propagating the poison for two reasons:
//!
//! 1. The panicking test already reported its own failure to the CI
//!    runner; cascade-blocking every downstream env test hides
//!    additional findings.
//! 2. The env-var state may be whatever the panicked test left
//!    behind, but each downstream test either sets or removes the
//!    var it cares about before reading it — so a dirty env is not
//!    a correctness hazard for well-written tests.
//!
//! # Ratchet
//!
//! `crates/api/tests/env_lock_guard.rs` source-scans this crate for
//! any `unsafe { std::env::set_var` / `remove_var` sites that do
//! not also acquire `env_lock()`. A new site without the lock fails
//! the build.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Global env-mutation serialiser for this crate's tests. See module docs.
static ENV_MUTATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Acquires the crate-wide env-mutation lock. Hold the returned guard
/// for the duration of every `unsafe { std::env::set_var/remove_var }`
/// call chain inside a `#[test]`.
///
/// Returns a poison-recovered guard — one panicking test won't block
/// the rest of the suite.
#[must_use = "the lock is released when the returned guard is dropped"]
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    ENV_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::env_lock;

    #[test]
    fn env_lock_is_reentrant_safe_across_sequential_acquires() {
        // Two sequential acquires from the same thread must not
        // deadlock (Mutex is not reentrant, but sequential acquires
        // via drop-then-reacquire are fine).
        {
            let _g = env_lock();
        }
        let _g = env_lock();
    }

    #[test]
    fn env_lock_poisoned_lock_returns_recovered_guard() {
        // Poison the lock from a child thread, then acquire from this
        // thread — we must get a guard back (PoisonError recovery),
        // not a panic.
        let handle = std::thread::spawn(|| {
            let _g = env_lock();
            panic!("deliberate test poison");
        });
        let _ = handle.join(); // Swallow the expected panic.

        // If env_lock() propagated the poison, this call would panic.
        let _g = env_lock();
    }

    #[test]
    fn env_lock_returns_guard_type_that_holds_mutex() {
        // Compile-time check: the returned guard must have the
        // `MutexGuard<()>` type so lifetimes/borrow-checker enforce
        // scope-bound holding. If this ever becomes `()` (someone
        // "simplified" to no-op), the test tree loses its safety
        // property.
        let guard = env_lock();
        let _borrowed: &() = &*guard;
    }
}
