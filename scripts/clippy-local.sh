#!/usr/bin/env bash
# =============================================================================
# clippy-local.sh — run the CI clippy gate locally on a toolchain whose
#                   `cargo-clippy` shim is missing.
# =============================================================================
#
# WHY THIS EXISTS (2026-09-01)
# ----------------------------
# The pinned toolchain (rust-toolchain.toml -> 1.95.0) has a PARTIAL clippy
# install: `clippy-driver` is present, the `cargo-clippy` subcommand shim is
# NOT. `~/.cargo/bin/cargo-clippy` is a rustup proxy that resolves to the
# ACTIVE toolchain, finds no shim there, and fails -- so plain `cargo clippy`
# is unavailable while clippy itself is perfectly functional. `rustup component
# add clippy` reports "up to date" (its manifest believes the component is
# installed) and `rustup component remove clippy` panics, so the component
# cannot be repaired from here.
#
# A session that reads "cargo clippy: command not found" and concludes clippy
# CANNOT be run locally will ship lint failures to CI. That happened on
# 2026-09-01 (commit 4e7ef501 -> Build & Verify red on
# clippy::too_many_arguments). This script is the standing answer.
#
# HOW IT WORKS
# ------------
# `cargo-clippy` is only a thin shim: it sets RUSTC_WORKSPACE_WRAPPER to
# clippy-driver plus CLIPPY_ARGS, then calls `cargo check`. We do that
# directly. clippy-driver is invoked ONLY for workspace members (that is what
# WORKSPACE_WRAPPER means), which is the same scope as `--no-deps`.
#
# SCOPE PARITY WITH CI (two steps in ci.yml "Build & Verify", both here):
#     cargo clippy --workspace --no-deps -- -D warnings          (libs + bins)
#     cargo clippy <CLEAN_CRATES> --all-targets --no-deps -- -D warnings
# The second (2026-10-30, plan 12b) also lints tests and benches of the crates
# that measured clean. Pinned by crates/common/tests/clippy_local_parity_guard.rs.
#
# HONEST LIMITS
#   * clippy.toml is honoured (it is found by walking up from the crate dir),
#     so thresholds are the project's, not clippy's defaults. Verified
#     2026-09-01: too-many-arguments-threshold = 8, and a 9-argument probe
#     fires "this function has too many arguments (9/8)".
#   * A warning replayed from cargo's cache still counts -- the grep below
#     sees replayed diagnostics, so a second run cannot pass vacuously.
# =============================================================================
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
CLEAN_CRATES=(-p tickvault-common -p tickvault-storage -p tickvault-api -p tickvault-aws-lambdas -p tickvault-logs-mcp)

# Prefer the real subcommand whenever the toolchain actually has it.
if cargo clippy --version >/dev/null 2>&1; then
    echo "clippy-local: cargo-clippy shim present -- using it directly"
    cargo clippy --workspace --no-deps -- -D warnings
    exec cargo clippy "${CLEAN_CRATES[@]}" --all-targets --no-deps -- -D warnings
fi

DRIVER="$(rustup which clippy-driver 2>/dev/null || true)"
if [[ -z "${DRIVER}" || ! -x "${DRIVER}" ]]; then
    echo "clippy-local: FATAL -- no cargo-clippy shim AND no clippy-driver." >&2
    echo "clippy-local: cannot lint locally; do NOT claim a clean clippy run." >&2
    exit 2
fi

echo "clippy-local: no cargo-clippy shim; driving ${DRIVER} as the workspace wrapper"

OUT="$(mktemp)"
trap 'rm -f "${OUT}"' EXIT

set +e
export CLIPPY_ARGS="--no-deps" RUSTC_WORKSPACE_WRAPPER="${DRIVER}"
{ cargo check --workspace && cargo check "${CLEAN_CRATES[@]}" --all-targets; } >"${OUT}" 2>&1
CARGO_STATUS=$?
set -e

cat "${OUT}"

if [[ ${CARGO_STATUS} -ne 0 ]]; then
    echo "clippy-local: FAILED -- cargo exited ${CARGO_STATUS}" >&2
    exit "${CARGO_STATUS}"
fi

# `cargo check` accepts no trailing rustc args, so `-D warnings` cannot be
# passed the way CI passes it. Equivalent gate: CI fails on any warning, so
# require zero diagnostics.
if grep -qE '^(warning|error)' "${OUT}"; then
    echo "clippy-local: FAILED -- diagnostics present; CI runs with -D warnings" >&2
    exit 1
fi

echo "clippy-local: PASS -- zero diagnostics (workspace libs + bins; all targets of the clean crates)"
