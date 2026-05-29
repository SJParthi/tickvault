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
const METRIC: &str = "tv_aggregator_close_pct_nonzero_total";

#[test]
fn seal_site_emits_close_pct_nonzero_prometheus_counter() {
    let src = read(MAIN_RS);
    let hits = src.matches(METRIC).count();
    assert!(
        hits >= 2,
        "main.rs must emit `{METRIC}` at BOTH seal sites (per-tick + \
         IST-midnight force-seal), found {hits}. This is the G3 real-time \
         proof that close_pct_from_prev_day populates live — without it a \
         silently-zero % column has no positive operator signal \
         (audit-findings Rule 11)."
    );
}

#[test]
fn per_tick_seal_site_records_heartbeat_close_pct_nonzero() {
    let src = read(MAIN_RS);
    assert!(
        src.contains("record_close_pct_nonzero()"),
        "main.rs per-tick seal site must call \
         `heartbeat_writer.record_close_pct_nonzero()` so the per-minute \
         AGGREGATOR-HB-01 heartbeat carries the positive signal."
    );
    assert!(
        src.contains("close_pct_nonzero = snap.close_pct_nonzero"),
        "the aggregator heartbeat `info!` line must include \
         `close_pct_nonzero = snap.close_pct_nonzero` so the operator can \
         tail it (AGGREGATOR-HB-01)."
    );
}

#[test]
fn close_pct_nonzero_is_guarded_by_a_nonzero_check_not_unconditional() {
    let src = read(MAIN_RS);
    // The signal must be conditional on a real non-zero %, not fired on
    // every seal (which would make it a meaningless duplicate of
    // seals_emitted and defeat the false-OK detection).
    assert!(
        src.contains("state.close_pct_from_prev_day != 0.0"),
        "the G3 signal must be gated on `state.close_pct_from_prev_day != 0.0` \
         (captured before the seal move), not emitted unconditionally — \
         otherwise it cannot distinguish a populating % from a flat one."
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
