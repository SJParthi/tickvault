# Implementation Plan: main.rs restriction-lint blanket hardening

**Status:** DRAFT
**Date:** 2026-07-17
**Approved by:** pending

## Plan Items

- [x] Add the three restriction-lint deny attributes to the `main` bin crate root
  - Files: `crates/app/src/main.rs`
  - Tests: `test_main_rs_carries_lib_restriction_lint_blanket`

- [x] Scope the single APPROVED bootstrap `.expect(` (rustls CryptoProvider install) with `#[allow(clippy::expect_used)]` + adjacent `// APPROVED:` justification
  - Files: `crates/app/src/main.rs`
  - Tests: `cargo clippy -p tickvault-app --all-targets` clean

- [x] Add a source-scan ratchet pinning the blanket on both crate roots
  - Files: `crates/app/tests/main_lint_blanket_guard.rs`
  - Tests: `test_main_rs_carries_lib_restriction_lint_blanket`, `test_lib_rs_still_carries_the_same_blanket`, `test_comment_stripper_removes_commented_attrs_keeps_real_ones`

## Design

`crates/app/src/lib.rs` carries a restriction-lint deny blanket at its crate
root (`#![cfg_attr(not(test), deny(clippy::unwrap_used))]`,
`#![cfg_attr(not(test), deny(clippy::expect_used))]`,
`#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]`).
The `tickvault-app` **binary** crate root (`crates/app/src/main.rs`) is a
SEPARATE compilation unit and did not inherit those attributes, so the boot
sequence code that lives directly in `main.rs` was un-linted for the exact
silent-panic / stray-print class the blanket exists to forbid. This change
copies the same three inner attributes into `main.rs`, scopes the one
legitimate bootstrap `expect` (rustls CryptoProvider install — TLS is
mandatory, install cannot fail-soft) with a justified `#[allow]`, and adds a
build-failing source-scan ratchet so the two crate roots stay in lockstep.

## Edge Cases

- A commented-out attribute line must not vacuously satisfy the ratchet — the
  guard strips `//`/`///` comment lines before matching while preserving real
  `#![...]` attribute lines (non-vacuity self-test included).
- `#![cfg_attr(not(test), ...)]` gates the unwrap/expect denies to non-test
  builds, so `main.rs`'s own inline `#[cfg(test)]` modules and the separate
  test targets are unaffected.
- The bootstrap `expect` is the ONLY prod-path expect/unwrap in `main.rs`
  (clippy blast-radius assessed: zero errors beyond that one site); it is
  scoped rather than papered over broadly.

## Failure Modes

- If a future edit adds an unwrap/expect/print/dbg to production `main.rs`
  code, the build now FAILS at clippy (the intended new guard rail).
- If a future PR deletes any of the three attributes from `main.rs` (or
  weakens the lib.rs blanket), the source-scan ratchet fails the build.
- No runtime behavior changes: lint attributes are compile-time only; the
  scoped `#[allow]` preserves the pre-existing fatal-on-failure semantics of
  the CryptoProvider install.

## Test Plan

- `cargo clippy -p tickvault-app --all-targets` — clean (0 errors).
- `cargo test -p tickvault-app test_main_rs_carries_lib_restriction_lint_blanket`
  and the two sibling ratchet tests — green.
- `cargo fmt --check` — clean.

## Rollback

Single-commit revert restores the prior `main.rs` (no attributes, no scoped
allow) and deletes the ratchet test file. No data, schema, config, or runtime
state is touched — the change is compile-time lint attributes plus one test
file, so rollback is a pure `git revert` with zero migration.

## Observability

No new ErrorCode, metric, Telegram event, or audit table — this is a
compile-time lint-hardening change with zero runtime surface. The
observability of the change itself is the CI clippy gate + the source-scan
ratchet failing the build on regression.

## Guarantee matrix cross-ref

Per `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row
matrices) and `operator-charter-forever.md` §C/§F: this item touches only the
`tickvault-app` crate root lint configuration + a test ratchet. Applicable
rows — 100% code checks (clippy deny blanket now covers the bin crate root),
100% extreme check (build-failing source-scan ratchet), 100% functionalities
covering (every new test fn has a call site via the test harness). Resilience
rows are N/A — no hot-path, tick, WS, or QuestDB code is touched; O(1) is
preserved (no runtime code changed). Honest 100% claim: 100% inside the tested
envelope — the ratchet fails the build if the blanket regresses on either
crate root.
