//! G3 — percentage-change real-time-proof wiring guard.
//!
//! The percentage-change feature (`close_pct_from_prev_day`) is only
//! useful if it actually populates NON-ZERO during market hours. A
//! column that silently computes `0.0` for every seal (the Concern-C
//! regression class — see PR #861 `candle_pct_column_guard.rs`) would
//! pass the schema ratchet yet deliver no value, and the operator would
//! have NO positive signal it broke (audit-findings-2026-04-17.md
//! Rule 11 — no false-OK).
//!
//! G3 closes that gap by emitting a positive real-time signal whenever a
//! sealed candle carries a non-zero `close_pct_from_prev_day`:
//!   - Prometheus counter `tv_aggregator_close_pct_nonzero_total`
//!     (scraped into CloudWatch via the `user-data.sh.tftpl` selector),
//!   - the per-minute aggregator heartbeat field `close_pct_nonzero`
//!     (`AGGREGATOR-HB-01` log line).
//!
//! Operator/CloudWatch read: `seals_emitted > 0` while
//! `close_pct_nonzero == 0` sustained during market hours == the % is
//! silently broken.
//!
//! This is a source-scan guard (Rule 13: "a method defined + tested but
//! never called IS a bug"). It fails the build if the seal sites in
//! `main.rs` stop emitting the signal, mirroring the
//! `candle_pct_column_guard.rs` pattern for the persist chain.

#![cfg(test)]

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(|r| r.join(rel))
        .unwrap_or_else(|| PathBuf::from(rel));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("g3 guard: cannot read {} ({e})", path.display()))
}

const MAIN_RS: &str = "crates/app/src/main.rs";
// C2 (behavior-preserving): the per-seal routing body — including the G3
// real-time-proof emission — moved out of the two inline `main.rs` closures into
// the single shared `route_seal`. The signal is STILL emitted at every seal site
// (both Dhan paths route through `route_seal`); it is just emitted from ONE place
// now. The wiring scan therefore targets the shared module.
const SEAL_ROUTING_RS: &str = "crates/app/src/seal_routing.rs";
const METRIC: &str = "tv_aggregator_close_pct_nonzero_total";

#[test]
fn seal_site_emits_close_pct_nonzero_prometheus_counter() {
    // The metric is emitted once in the shared `route_seal` (Dhan-feed gate),
    // which BOTH Dhan seal sites (per-tick + IST-midnight) invoke. Confirm the
    // shared emit exists AND that both main.rs sites route through it.
    let routing = read(SEAL_ROUTING_RS);
    assert!(
        routing.contains(METRIC),
        "seal_routing.rs (route_seal) must emit `{METRIC}` — the G3 real-time \
         proof that close_pct_from_prev_day populates live. Without it a \
         silently-zero % column has no positive operator signal \
         (audit-findings Rule 11)."
    );
    let main_rs = read(MAIN_RS);
    let route_hits = main_rs.matches("seal_routing::route_seal").count();
    assert!(
        route_hits >= 2,
        "main.rs must route BOTH Dhan seal sites (per-tick + IST-midnight \
         force-seal) through `seal_routing::route_seal`, found {route_hits} — \
         so the shared G3 emit fires at both."
    );
}

#[test]
fn per_tick_seal_site_records_heartbeat_close_pct_nonzero() {
    // The heartbeat record now lives in route_seal (gated on the per-tick Dhan
    // heartbeat param). The per-minute AGGREGATOR-HB-01 `info!` line stays in
    // main.rs.
    let routing = read(SEAL_ROUTING_RS);
    assert!(
        routing.contains("record_close_pct_nonzero()"),
        "seal_routing.rs (route_seal) must call \
         `hb.record_close_pct_nonzero()` so the per-minute AGGREGATOR-HB-01 \
         heartbeat carries the positive signal for the per-tick Dhan path."
    );
    let main_rs = read(MAIN_RS);
    assert!(
        main_rs.contains("close_pct_nonzero = snap.close_pct_nonzero"),
        "the aggregator heartbeat `info!` line in main.rs must include \
         `close_pct_nonzero = snap.close_pct_nonzero` so the operator can \
         tail it (AGGREGATOR-HB-01)."
    );
}

#[test]
fn close_pct_nonzero_is_guarded_by_a_nonzero_check_not_unconditional() {
    let routing = read(SEAL_ROUTING_RS);
    // The signal must be conditional on a real non-zero %, not fired on
    // every seal (which would make it a meaningless duplicate of
    // seals_emitted and defeat the false-OK detection).
    assert!(
        routing.contains("state.close_pct_from_prev_day != 0.0"),
        "the G3 signal in seal_routing.rs (route_seal) must be gated on \
         `state.close_pct_from_prev_day != 0.0` (captured before the seal \
         move), not emitted unconditionally — otherwise it cannot distinguish \
         a populating % from a flat one."
    );
}

#[test]
fn metric_is_ingested_into_cloudwatch_selector() {
    let tftpl = read("deploy/aws/terraform/user-data.sh.tftpl");
    assert!(
        tftpl.contains(METRIC),
        "`{METRIC}` must be in the CloudWatch metric selector regex in \
         user-data.sh.tftpl so the false-OK signal reaches CloudWatch \
         (operator-charter 100% alerting / monitoring)."
    );
}
