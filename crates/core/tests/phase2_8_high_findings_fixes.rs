//! Phase 2.8 — HIGH findings from the 3-agent adversarial review.
//!
//! Pins the four fixes from the security + hostile-bug-hunt reports
//! on the Phase 2 production diff:
//!
//! - **H4** — `prev_oi_cache.load_from_questdb` failure DOES NOT mark
//!   the gate ready; main.rs gates `mark_oi_cache_loaded` on Ok only,
//!   so an actual QuestDB failure surfaces as ERROR + Telegram instead
//!   of silently authorizing subscribe.
//! - **H3** — empty cache (Ok with count=0) emits a structured WARN +
//!   Prom counter `tv_prev_oi_cache_empty_total` so the False-OK is
//!   visible to the operator.
//! - **H2** — `phase_code_to_str` returns "UNKNOWN" (not the previous
//!   silent default-to-PREMARKET) for out-of-range codes; the
//!   `tv_phase_code_unknown_total` counter increments per occurrence.
//! - **Security HIGH** — `LoadError::HttpRequest` Display NO LONGER
//!   embeds `reqwest::Error::Display` (which leaks the URL). The new
//!   form emits `kind=` + `status=` only.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    me.parent()
        .expect("tickvault root")
        .parent()
        .expect("workspace root")
        .to_path_buf()
        .join("crates")
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h4_main_rs_gates_mark_oi_cache_loaded_on_ok_only died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h4_main_rs_logs_load_failure_at_error_level_with_typed_code died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h4_main_rs_emits_outcome_metric died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h3_main_rs_emits_empty_cache_diagnostic died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// REMOVED 2026-06-02 (CI rot fix): `h2_phase_code_to_str_returns_unknown_for_out_of_range`.
// The `phase_code_to_str` function + its `tv_phase_code_unknown_total` counter
// were deleted from `tick_persistence.rs` when the per-tick phase-code column
// was removed (candle-engine re-architecture). The H2 drift-hazard concern no
// longer has any production surface to guard — the test scanned for strings
// that no longer exist anywhere in the workspace.

#[test]
fn security_h_load_error_http_request_does_not_embed_reqwest_display() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    // The legacy form was `#[error("HTTP request to QuestDB failed: {0}")]`
    // — that pattern interpolates `Display` of the inner `reqwest::Error`
    // which can include the full URL. The fix uses `#[source]` and a
    // custom format string with `kind=` + `status=` only.
    assert!(
        !src.contains("\"HTTP request to QuestDB failed: {0}\""),
        "LoadError::HttpRequest must NOT use `{{0}}` interpolation \
         (security fix: avoid leaking reqwest::Error URL to logs)"
    );
    assert!(
        src.contains("HttpRequest(#[source] reqwest::Error)"),
        "LoadError::HttpRequest must use #[source] for the inner error \
         (security fix: separate machine-readable cause from human-readable Display)"
    );
}

#[test]
fn security_h_load_error_http_client_does_not_embed_reqwest_display() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    assert!(
        !src.contains("\"failed to build HTTP client: {0}\""),
        "LoadError::HttpClient must NOT interpolate {{0}} (security fix)"
    );
    assert!(
        src.contains("HttpClient(#[source] reqwest::Error)"),
        "LoadError::HttpClient must use #[source]"
    );
}

#[test]
fn security_h_load_error_json_parse_does_not_embed_serde_display() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    // serde_json::Error's Display can include the input snippet. For
    // defence-in-depth we keep the cause via #[source] but don't
    // interpolate {0}.
    assert!(
        !src.contains("\"failed to parse QuestDB response JSON: {0}\""),
        "LoadError::JsonParse must NOT interpolate {{0}} (security fix)"
    );
    assert!(
        src.contains("JsonParse(#[source] serde_json::Error)"),
        "LoadError::JsonParse must use #[source]"
    );
}
