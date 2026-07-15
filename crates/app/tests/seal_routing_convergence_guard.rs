//! C2 convergence guard — exactly ONE seal-routing body.
//!
//! Before C2 the per-seal routing logic (build a `BufferedSeal` + route it
//! through `global_seal_sender`, with the per-feed D1-drop / pct-stamp /
//! observability policy) was duplicated as inline closures in `main.rs` (Dhan
//! per-tick + Dhan IST-midnight) and `groww_bridge.rs` (Groww). C2 converged all
//! three onto the single shared `seal_routing::route_seal`.
//!
//! This is a source-scan ratchet (Rule 13 — a method defined but a call site
//! silently re-inlined IS a regression). It fails the build if either call site
//! stops calling `route_seal`, OR if the inline routing pattern (`BufferedSeal::new`
//! + `global_seal_sender` + the D1-drop) reappears OUTSIDE `seal_routing.rs`.
//! Mirrors `test_run_groww_bridge_routes_seals_through_shared_writer`.

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(|r| r.join(rel))
        .unwrap_or_else(|| PathBuf::from(rel));
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "seal-routing convergence guard: cannot read {} ({e})",
            path.display()
        )
    })
}

const MAIN_RS: &str = "crates/app/src/main.rs";
const SEAL_ROUTING_RS: &str = "crates/app/src/seal_routing.rs";

/// The shared `route_seal` body exists in `seal_routing.rs`.
#[test]
fn test_shared_route_seal_exists() {
    let src = read(SEAL_ROUTING_RS);
    assert!(
        src.contains("pub fn route_seal("),
        "seal_routing.rs must define the shared `pub fn route_seal(...)` — the \
         single seal-routing body both feeds invoke."
    );
}

/// The Dhan call sites (per-tick + IST-midnight force-seal) route through
/// `route_seal`. There are exactly TWO Dhan seal sites; both must call it.
#[test]
fn test_dhan_call_sites_use_route_seal() {
    let src = read(MAIN_RS);
    let hits = src.matches("seal_routing::route_seal").count();
    assert!(
        hits >= 2,
        "main.rs must route BOTH Dhan seal sites (per-tick + IST-midnight \
         force-seal) through `seal_routing::route_seal`, found {hits}. A site \
         that re-inlines the routing diverges from the shared body."
    );
}

/// The Groww call site routes through `route_seal`.
// RETIRED 2026-07-15 (Groww live-feed deletion):
// test_groww_call_site_uses_route_seal died with groww_bridge.rs — the Dhan
// call-site + anti-inline pins below remain the C2 convergence contract.

/// No call site hand-builds the seal-routing inline OUTSIDE `seal_routing.rs`.
/// The signature of the inline routing is `BufferedSeal::new(` immediately
/// fed to `global_seal_sender()`'s `try_send` — the convergence target. Today
/// the ONLY file that may contain `BufferedSeal::new(` + `try_send` together is
/// `seal_routing.rs`; the call sites pass `state` to `route_seal`.
#[test]
fn test_no_inline_seal_routing_outside_seal_routing_module() {
    for rel in [MAIN_RS] {
        let src = read(rel);
        assert!(
            !src.contains("BufferedSeal::new("),
            "{rel} hand-builds a `BufferedSeal::new(...)` inline — the per-seal \
             routing body MUST live ONLY in seal_routing.rs::route_seal (C2 \
             convergence). Route through `seal_routing::route_seal` instead."
        );
    }
    // The shared module is allowed (and required) to build it.
    let routing = read(SEAL_ROUTING_RS);
    assert!(
        routing.contains("BufferedSeal::new("),
        "seal_routing.rs must build the `BufferedSeal` (the one shared routing body)."
    );
}
