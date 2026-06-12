//! Source-scan ratchet: EVERY WebSocket disconnect + reconnect must be
//! structured-logged (so it lands in CloudWatch / app.log), not just sent to
//! Telegram, and must carry the precise best-guess source from the classifier.
//!
//! Operator directive 2026-06-12: "whenever disconnect/reconnect it should
//! always alert AND everything be logged, tracked, captured." Before this, the
//! disconnect/reconnect sites only called `notify()` (Telegram) with NO tracing
//! log — so disconnects never reached CloudWatch. This guard fails the build if
//! that regresses.

use std::fs;
use std::path::PathBuf;

fn connection_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/websocket/connection.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_every_disconnect_is_structured_logged() {
    // record_disconnect is the single choke point for all 4 disconnect emit
    // sites — its warn! guarantees every disconnect reaches CloudWatch.
    let src = connection_src();
    assert!(
        src.contains("\"WebSocket disconnected\""),
        "record_disconnect must warn!(\"WebSocket disconnected\") so EVERY disconnect lands in \
         CloudWatch/app.log — not just Telegram."
    );
}

#[test]
fn test_every_reconnect_is_structured_logged() {
    let src = connection_src();
    assert!(
        src.contains("\"WebSocket reconnected\""),
        "the reconnect emit site must info!(\"WebSocket reconnected\") so EVERY reconnect lands \
         in CloudWatch/app.log."
    );
}

#[test]
fn test_disconnect_and_reconnect_logs_carry_precise_source() {
    // Both the disconnect and reconnect logs must classify the precise source
    // (Dhan / network / token) via the classifier — so the CloudWatch line is
    // as informative as the Telegram message.
    let src = connection_src();
    let n = src.matches("classify_disconnect_cause").count();
    assert!(
        n >= 2,
        "both the disconnect log AND the reconnect log must call \
         classify_disconnect_cause to attach the precise source; found {n}."
    );

    // PIN EACH LOG TO ITS OWN CLASSIFIER (hardening vs a regression where the
    // reconnect site loses its source while a 2nd classify is added elsewhere):
    // the reconnect site must classify with the THREADED Dhan code (`dhan_code`),
    // and the disconnect choke point must classify the recorded `reason`.
    assert!(
        src.contains("classify_disconnect_cause(&reason, dhan_code)"),
        "the disconnect choke point (record_disconnect) must classify the \
         recorded reason WITH the threaded Dhan code (&reason, dhan_code)."
    );
    // The reconnect block threads the SAME Dhan code captured at disconnect so
    // its source matches the disconnect's. `take_disconnect_context` must return
    // the code (4th tuple element) and the reconnect classify must consume it.
    assert!(
        src.contains("dhan_code) = self.take_disconnect_context()")
            || src.contains(
                "dhan_code) =\n                            self.take_disconnect_context()"
            ),
        "the reconnect site must take the Dhan code out of disconnect-context \
         so its source matches the disconnect's precise source."
    );
}
