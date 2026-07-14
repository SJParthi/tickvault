//! Source-scan ratchet — the episode-mode-aware Groww reject-page cooldown
//! is WIRED at both runtime sites (FIX-1, hostile review round-2,
//! 2026-07-14 operator noise directive).
//!
//! The value contract (60s with the GrowwFeed episode fold ON, legacy
//! 1800s with `[notification] episode_mode = false`) is pinned by the
//! supervisor's own `test_should_page_reject_rising_edge_and_cooldown`
//! (==60 / ==1800 + the `reject_page_cooldown_secs` selector). What THAT
//! test cannot see is the PLUMBING: a mutation reverting either runtime
//! site to the unconditional constant/default would compile, pass the
//! value pins, and silently make the rollback path (episode_mode=false)
//! ~30× noisier — or the fold path 30× quieter. This guard pins the two
//! literals at their runtime sites so either mutation fails the build:
//!
//! (a) `run_groww_sidecar_supervisor` computes the window via
//!     `reject_page_cooldown_secs(opts.episode_mode)` — the selector IS
//!     called with the boot-time option, never a raw constant;
//! (b) the passthrough page gate consumes the PLUMBED variable (and NOT
//!     the raw `GROWW_REJECT_PAGE_COOLDOWN_SECS` constant);
//! (c) main.rs plumbs `episode_mode: config.notification.episode_mode`
//!     into `GrowwSidecarOptions` (a `..default()` revert would silently
//!     hardcode fold-ON semantics regardless of config).
//!
//! Self-test-safe by construction: the needles live in THIS file while the
//! scans read the src files (never themselves), and the supervisor scan is
//! additionally clipped to the PRODUCTION region (split at `#[cfg(test)]`)
//! so its own test module can never satisfy a needle. Whitespace is
//! normalized before matching so a rustfmt re-wrap can never produce a
//! false failure. House pattern: `ws_rate_limit_cooldown_wiring_guard.rs`.

use std::fs;
use std::path::PathBuf;

fn read_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Production region only — everything before the first `#[cfg(test)]`.
/// Panics loudly if the marker vanishes (a rename must update this guard,
/// never silently widen the scan to include test code).
fn production_region(src: &str) -> &str {
    let idx = src
        .find("#[cfg(test)]")
        .expect("expected a #[cfg(test)] marker — update this guard if the test module moved");
    &src[..idx]
}

/// Collapses every whitespace run to a single space so multi-line rustfmt
/// call shapes match single-line needles.
fn normalize_ws(src: &str) -> String {
    src.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn ratchet_supervisor_selects_cooldown_from_boot_time_episode_mode() {
    let src = read_src("src/groww_sidecar_supervisor.rs");
    let prod = normalize_ws(production_region(&src));

    // (a) The selector is called with the boot-time option — exactly once
    // (the run-loop compute site). A second call site (or zero) means the
    // plumbing changed; re-review before updating this count.
    let selector_call = "reject_page_cooldown_secs(opts.episode_mode)";
    let calls = prod.matches(selector_call).count();
    assert_eq!(
        calls, 1,
        "expected exactly 1 `{selector_call}` compute site in the supervisor's \
         production region (run_groww_sidecar_supervisor); found {calls} — a revert \
         to the unconditional constant makes the episode_mode=false rollback path \
         ~30× noisier (FIX-B, 2026-07-14)"
    );

    // (b) The passthrough page gate consumes the PLUMBED variable...
    let gate_plumbed = "should_page_reject(now_secs, last, reject_page_cooldown_secs)";
    assert!(
        prod.contains(gate_plumbed),
        "the passthrough page gate must consume the plumbed per-spawn cooldown: \
         `{gate_plumbed}` missing from the supervisor's production region"
    );
    // ...and NEVER the raw constant directly (that would ignore the mode).
    let gate_raw = "should_page_reject(now_secs, last, GROWW_REJECT_PAGE_COOLDOWN_SECS)";
    assert!(
        !prod.contains(gate_raw),
        "the passthrough page gate must NOT bypass the episode-mode selector \
         with the raw constant: found `{gate_raw}`"
    );
}

#[test]
fn ratchet_main_plumbs_config_episode_mode_into_sidecar_options() {
    let main_src = normalize_ws(&read_src("src/main.rs"));
    // (c) The boot-time config value reaches GrowwSidecarOptions. A revert
    // to `..default()` (which hardcodes fold-ON semantics) removes this
    // literal and fails the build here.
    let plumb = "episode_mode: config.notification.episode_mode";
    assert!(
        main_src.contains(plumb),
        "main.rs must plumb the boot-time `[notification] episode_mode` into \
         GrowwSidecarOptions: `{plumb}` missing — without it the supervisor \
         cooldown ignores the episode kill switch (FIX-B, 2026-07-14)"
    );
}
