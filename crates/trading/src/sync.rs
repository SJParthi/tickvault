//! Atomic types that the loom model checker can see.
//!
//! With the crate's non-default `loom` feature these are loom's atomics, so a
//! `loom::model` test drives the REAL production struct through every
//! interleaving. Without it they are the std atomics, unchanged: no cost and
//! no behaviour change in a normal build.
//!
//! Audit H3 (2026-10-04): the circuit-breaker loom tests used to drive the
//! real struct with std atomics, which loom cannot intercept, so they proved
//! nothing about orderings. Only types that move behind this shim are
//! modelled; everything else in the crate keeps the std atomics.
//!
//! The `loom` feature is enabled only by the loom CI lane, and only together
//! with `--test loom_*` targets: loom atomics panic outside `loom::model`, so
//! the crate's ordinary unit tests must never run with it.

#[cfg(feature = "loom")]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[cfg(not(feature = "loom"))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
