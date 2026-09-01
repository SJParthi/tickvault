//! Clippy local-gate parity guard.
//!
//! # Why this guard exists (2026-09-01)
//!
//! On 2026-09-01 commit `4e7ef501` was pushed with a `clippy::too_many_arguments`
//! violation and CI's `Build & Verify` job went red. The root cause was not the
//! code — it was that the session believed clippy **could not be run locally**,
//! because `cargo clippy` fails on this box.
//!
//! It fails for a narrow, fixable reason: the pinned toolchain
//! (`rust-toolchain.toml` -> 1.95.0) carries a PARTIAL clippy install —
//! `clippy-driver` is present, the `cargo-clippy` subcommand shim is not, and
//! `~/.cargo/bin/cargo-clippy` is a rustup proxy that resolves to the active
//! toolchain and finds nothing. `rustup component add clippy` reports
//! "up to date" and `rustup component remove clippy` panics, so the component
//! cannot be repaired in place.
//!
//! `scripts/clippy-local.sh` is the standing answer: it drives `clippy-driver`
//! as the workspace wrapper, which is exactly what the missing shim does.
//!
//! # What this guard pins
//!
//! The local gate is worth nothing if it drifts from CI's scope — a local run
//! that checks LESS than CI reads green and still ships red. So this test pins
//! the two invocations against each other, in both directions.
//!
//! # The calibration lesson, pinned deliberately
//!
//! The first attempt to bite-test the local gate used an 8-argument function
//! and saw no warning, which looked like "the tooling does not work". It was
//! the opposite: `clippy.toml` raises `too-many-arguments-threshold` to **8**,
//! so 8 arguments are legal in this project and the silence was correct. A
//! 9-argument probe fires `this function has too many arguments (9/8)`.
//! The threshold is pinned below so a future change to `clippy.toml` cannot
//! silently invalidate that reasoning.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // crates/common -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("clippy_local_parity_guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("clippy_local_parity_guard: cannot read {rel}: {e}"))
}

/// CI's clippy invocation, verbatim. If this string leaves `ci.yml`, the local
/// script is no longer a stand-in for the gate and the parity claim is void.
const CI_CLIPPY_INVOCATION: &str = "cargo clippy --workspace --no-deps -- -D warnings";

#[test]
fn ci_still_runs_the_invocation_the_local_script_mirrors() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains(CI_CLIPPY_INVOCATION),
        "clippy_local_parity_guard: ci.yml no longer contains the pinned clippy \
         invocation `{CI_CLIPPY_INVOCATION}`.\n\
         scripts/clippy-local.sh exists to mirror THAT command. If CI's scope \
         changed, change the script and this constant in the SAME commit — a \
         local gate that checks a different scope than CI reads green and still \
         ships red, which is the exact failure this guard was created for."
    );
}

#[test]
fn the_local_script_exists_and_is_a_bash_program() {
    let script = read("scripts/clippy-local.sh");
    assert!(
        script.starts_with("#!/usr/bin/env bash") || script.starts_with("#!/bin/bash"),
        "clippy_local_parity_guard: scripts/clippy-local.sh must be a bash program \
         (the repo is Rust-only for RUNTIME components; shell is the sanctioned \
         tooling language per rust-only-forever-lock-2026-07-19.md)."
    );
}

#[test]
fn the_local_script_mirrors_ci_scope_and_never_widens_or_narrows_it() {
    let script = read("scripts/clippy-local.sh");

    // Same breadth as CI.
    assert!(
        script.contains("--workspace"),
        "clippy_local_parity_guard: the local script must check the WHOLE workspace, \
         as CI does. A single-crate local run misses cross-crate lints."
    );
    // Same depth as CI: workspace members only.
    assert!(
        script.contains("--no-deps"),
        "clippy_local_parity_guard: the local script must record `--no-deps` parity \
         with CI. RUSTC_WORKSPACE_WRAPPER already restricts linting to workspace \
         members, but the flag must stay visible so the parity is readable."
    );
    // NOT wider than CI. `--all-targets` would surface lints CI never runs,
    // training the reader to ignore local output. Comments are stripped first:
    // the script DOCUMENTS why it avoids --all-targets, and a naive substring
    // scan would trip on that explanation.
    let code_only: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code_only.contains("--all-targets"),
        "clippy_local_parity_guard: the local script must NOT use --all-targets. \
         CI checks libs + bins only; a wider local gate produces findings CI will \
         never enforce, and a gate people learn to ignore is worse than no gate."
    );
    // The fallback must be the wrapper, not a silent skip.
    assert!(
        script.contains("RUSTC_WORKSPACE_WRAPPER"),
        "clippy_local_parity_guard: the local script must drive clippy-driver via \
         RUSTC_WORKSPACE_WRAPPER when the cargo-clippy shim is absent."
    );
}

#[test]
fn the_local_script_fails_loudly_when_it_cannot_lint_at_all() {
    let script = read("scripts/clippy-local.sh");
    assert!(
        script.contains("do NOT claim a clean clippy run"),
        "clippy_local_parity_guard: when neither the shim nor clippy-driver exists, \
         the script must EXIT NON-ZERO and say so. Silently succeeding would let a \
         session report `clippy clean` having linted nothing — the false-OK class \
         this repository forbids (operator-charter-forever.md rule 11)."
    );
    assert!(
        script.contains("set -euo pipefail"),
        "clippy_local_parity_guard: the script must abort on any unhandled error."
    );
}

#[test]
fn make_exposes_the_local_gate_so_it_is_discoverable_without_reading_this_test() {
    let makefile = read("Makefile");
    assert!(
        makefile.contains("clippy-ci:"),
        "clippy_local_parity_guard: `make clippy-ci` must exist. A script nobody \
         knows about is not automation."
    );
    assert!(
        makefile.contains("bash scripts/clippy-local.sh"),
        "clippy_local_parity_guard: the `clippy-ci` target must invoke the script."
    );
    assert!(
        makefile.contains(
            ".PHONY: help run run-supervised stop build test check fmt clippy clippy-ci clean"
        ),
        "clippy_local_parity_guard: `clippy-ci` must be declared .PHONY, or a file \
         named `clippy-ci` would silently disable the target."
    );
}

#[test]
fn the_too_many_arguments_threshold_that_calibrates_the_bite_test_is_pinned() {
    let cfg = read("clippy.toml");
    assert!(
        cfg.contains("too-many-arguments-threshold = 8"),
        "clippy_local_parity_guard: clippy.toml's too-many-arguments-threshold moved \
         away from 8.\n\
         This is pinned because the threshold is what makes a bite test of the local \
         clippy gate meaningful: at 8, an 8-argument probe is CORRECTLY silent and a \
         9-argument probe fires `(9/8)`. A session that bite-tests with the wrong \
         arity will conclude the tooling is broken when it is working — which is \
         exactly what happened on 2026-09-01. If the threshold changes, update the \
         calibration note in scripts/clippy-local.sh in the same commit."
    );
}
