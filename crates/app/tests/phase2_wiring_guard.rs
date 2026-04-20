//! Phase 2 wiring source-scan guard (slice 3 — 2026-04-20).
//!
//! The Phase 2 boot wiring went from `phase2_instruments: None`
//! (alert-only) to `Some(Phase2InstrumentsSource::Dynamic { .. })`
//! (real subscribe). This guard ratchets that transition: if a
//! future change reverts to `None` or removes the Dynamic variant
//! at the call site, this test fails the build before merge.
//!
//! Source-scan is the right tool here because the call lives in
//! `crates/app/src/main.rs` — a binary entry point that is hard
//! to test via behavioural assertions without spinning up the
//! whole boot sequence (Docker, secrets, calendar). The scan
//! is mechanical and exact.

use std::path::PathBuf;

const APP_MAIN_RS: &str = "src/main.rs";

fn read_main() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(APP_MAIN_RS);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

#[test]
fn phase2_call_site_uses_dynamic_source_not_static_none() {
    let src = read_main();

    // Negative ratchet — the legacy `None` literal as a Phase 2
    // instrument source must NOT appear. We look for the canonical
    // shape that was present pre-slice-3.
    assert!(
        !src.contains("phase2_instruments: None"),
        "Phase 2 boot wiring regressed: `phase2_instruments: None` \
             must NOT appear in main.rs — slice 3 (2026-04-20) replaced \
             this with Phase2InstrumentsSource::Dynamic. See \
             .claude/rules/project/audit-findings-2026-04-17.md Rule 3 \
             (background workers must be market-hours aware) and \
             docs/architecture/websocket-complete-reference.md gap O1-B."
    );

    // Positive ratchet — the Dynamic variant is wired at boot.
    assert!(
        src.contains("Phase2InstrumentsSource::Dynamic"),
        "Phase 2 boot wiring regressed: `Phase2InstrumentsSource::Dynamic` \
             literal MUST appear in main.rs — without it, the scheduler \
             falls back to alert-only mode and stock F&O is silently \
             never subscribed. Slice 3 wired this in 2026-04-20."
    );
}
