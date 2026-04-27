//! Ratchet guards for the 2026-04-26 "fix three Grafana NO-DATA tiles"
//! pull request. Pins the three mechanical fixes so a future revert
//! fails the build instead of silently regressing the operator-facing
//! dashboards.
//!
//! ## What this guards
//!
//! 1. `tv-health.json` Cross-Segment Security-ID Collisions threshold —
//!    must allow baseline=2 (NIFTY id=13 + BANKNIFTY id=25 colliding
//!    with NSE_EQ stocks under the cash-equity-for-F&O-underlyings
//!    universe) without firing RED. Aligned with `operator-health.json`
//!    triple-step: green / orange@5 / red@10. Before this fix the panel
//!    used `green @ null, red @ 1` which painted the panel RED on every
//!    boot with the new universe.
//! 2. `set_pipeline_active()` in `crates/api/src/state.rs` MUST emit
//!    `tv_pipeline_active` gauge. Before this fix the metric was never
//!    emitted → System Overview "Pipeline Status" tile read RED
//!    "STOPPED" forever even when ticks were flowing.
//! 3. `TickPersistenceWriter::new()` MUST emit `tv_questdb_connected`
//!    gauge eagerly on first connect (and `new_disconnected()` MUST emit
//!    0.0). Before this fix the operator-health "QuestDB" tile read
//!    "No data" RED for ~30s on every boot while the
//!    `QuestDbHealthPoller` (which lives inside
//!    `run_tick_persistence_consumer`, started later in `main.rs`)
//!    waited for its first tick.
//!
//! Each guard scans the on-disk source/JSON. None of them spin up a
//! Prometheus server or runtime — they are pure compile-time/source
//! invariants and run in milliseconds.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Guard 1 — tv-health.json cross-segment collision threshold
// ---------------------------------------------------------------------------

#[test]
fn tv_health_cross_segment_threshold_allows_baseline_two() {
    let text = read("deploy/docker/grafana/dashboards/tv-health.json");

    let panel_marker = "\"title\": \"Cross-Segment Security-ID Collisions\"";
    let panel_pos = text
        .find(panel_marker)
        .expect("Cross-Segment Security-ID Collisions panel must exist in tv-health.json");

    // Scope the assertion to JUST this panel's body. The panel layout in
    // tv-health.json puts the threshold steps within ~6 lines BEFORE the
    // title and the closing brace ~10 lines after, so a tight window of
    // 400 chars before / 600 chars after captures everything in this panel
    // without leaking into the previous panel (which legitimately keeps
    // `red @ 1` for its own counter semantics).
    let window_start = panel_pos.saturating_sub(400);
    let window_end = (panel_pos + 600).min(text.len());
    let panel_body = &text[window_start..window_end];

    // The fixed threshold MUST be the triple step that matches
    // operator-health.json: green / orange@5 / red@10. The legacy
    // two-step `red @ 1` is REJECTED for this panel only.
    assert!(
        panel_body.contains("\"orange\", \"value\": 5"),
        "tv-health.json Cross-Segment threshold MUST include orange@5; \
         current panel body:\n{panel_body}"
    );
    assert!(
        panel_body.contains("\"red\", \"value\": 10"),
        "tv-health.json Cross-Segment threshold MUST include red@10; \
         current panel body:\n{panel_body}"
    );
    assert!(
        !panel_body.contains("\"red\", \"value\": 1 }"),
        "tv-health.json Cross-Segment panel MUST NOT regress to red@1 \
         (would alarm on the documented NIFTY+BANKNIFTY baseline of 2)"
    );
    assert!(
        !panel_body.contains("\"red\", \"value\": 0"),
        "tv-health.json Cross-Segment panel MUST NOT regress to red@0"
    );
}

#[test]
fn tv_health_cross_segment_description_acknowledges_baseline() {
    let text = read("deploy/docker/grafana/dashboards/tv-health.json");
    // The description must mention the baseline so a future engineer
    // reading the panel understands why a non-zero value is OK.
    assert!(
        text.contains("Baseline = 2"),
        "tv-health.json Cross-Segment description MUST mention 'Baseline = 2' \
         so operators know NIFTY/BANKNIFTY collisions are documented + benign"
    );
    assert!(
        text.contains("security-id-uniqueness.md"),
        "tv-health.json Cross-Segment description MUST cross-reference \
         security-id-uniqueness.md so the rule file is one click away"
    );
}

// ---------------------------------------------------------------------------
// Guard 2 — set_pipeline_active() MUST emit tv_pipeline_active gauge
// ---------------------------------------------------------------------------

#[test]
fn set_pipeline_active_emits_tv_pipeline_active_gauge() {
    let text = read("crates/api/src/state.rs");

    // Locate the function body.
    let fn_marker = "pub fn set_pipeline_active(&self, active: bool)";
    let fn_pos = text
        .find(fn_marker)
        .expect("set_pipeline_active(&self, active: bool) must exist in crates/api/src/state.rs");
    // Walk to the matching closing brace.
    let body_start = text[fn_pos..]
        .find('{')
        .map(|p| fn_pos + p)
        .expect("opening brace of set_pipeline_active body");
    // Crude balanced-brace scan — the body is small and contains no string
    // literals that include '{' or '}'.
    let body_end = balanced_brace_end(&text, body_start)
        .expect("balanced closing brace for set_pipeline_active");
    let body = &text[body_start..=body_end];

    assert!(
        body.contains("metrics::gauge!(\"tv_pipeline_active\")"),
        "set_pipeline_active body MUST emit the `tv_pipeline_active` Prometheus \
         gauge so the System Overview dashboard 'Pipeline Status' tile reflects \
         the live flag. Current body:\n{body}"
    );
    // The gauge value MUST be conditional on `active` so a `false` flip
    // also drives the dashboard back to RED.
    assert!(
        body.contains("if active"),
        "set_pipeline_active body MUST conditional-set the gauge based on `active` \
         (1.0 when true, 0.0 when false). Current body:\n{body}"
    );
}

#[test]
fn main_rs_calls_set_pipeline_active_in_both_boot_paths() {
    let text = read("crates/app/src/main.rs");
    let count = text.matches("set_pipeline_active(true)").count();
    // Fast boot + slow boot = 2 production call sites at minimum.
    assert!(
        count >= 2,
        "main.rs MUST call health_status.set_pipeline_active(true) in BOTH \
         the fast-boot and slow-boot paths so the gauge flips to RUNNING \
         regardless of which boot mode the operator chose. Current call count: {count}"
    );
}

fn balanced_brace_end(text: &str, open_pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open_pos) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open_pos) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Guard 3 — TickPersistenceWriter::new* must emit tv_questdb_connected eagerly
// ---------------------------------------------------------------------------

#[test]
fn tick_persistence_writer_new_emits_questdb_connected_one() {
    let text = read("crates/storage/src/tick_persistence.rs");

    // Locate the first `pub fn new(config: &QuestDbConfig)` signature.
    // Multiple writers (tick + depth) live in this file; the FIRST
    // occurrence belongs to TickPersistenceWriter (line ~244 as of
    // 2026-04-26). The depth writer's `new` follows the same pattern,
    // so we additionally assert ANY `new(` body contains the emission
    // — i.e. we expect ≥ 1 emission of `tv_questdb_connected").set(1.0)`
    // in the file overall.
    let connect_emissions = text
        .matches("metrics::gauge!(\"tv_questdb_connected\").set(1.0)")
        .count();
    assert!(
        connect_emissions >= 1,
        "tick_persistence.rs MUST emit `tv_questdb_connected=1.0` from the \
         connected constructor so the operator-health 'QuestDB' tile is GREEN \
         from boot instead of 'No data' RED for ~30s. Current count: {connect_emissions}"
    );

    let disconnected_emissions = text
        .matches("metrics::gauge!(\"tv_questdb_connected\").set(0.0)")
        .count();
    assert!(
        disconnected_emissions >= 1,
        "tick_persistence.rs MUST emit `tv_questdb_connected=0.0` from the \
         `new_disconnected()` constructor so the dashboard tile correctly \
         reads RED 'DOWN' instead of 'No data' on a fast-boot path that \
         starts QuestDB-disconnected. Current count: {disconnected_emissions}"
    );
}
