//! Source-scan ratchet — the aggregation boundary tasks stay SPAWNED
//! (2026-07-14, PR-3 of the automation-gaps series; audit gaps 1+2 of
//! `automation-map.md` 2026-07-10).
//!
//! The 2026-07-10 automation-coverage audit found the two HIGHEST-value
//! silent-regression windows in the aggregation chain:
//!
//! 1. The BOUNDARY-01 watermark catch-up seal DRIVERS — main.rs
//!    `spawn_engine_b_aggregator` Task 4 (Dhan) and
//!    `groww_bridge.rs::spawn_groww_catchup_seal` (Groww) — had NO spawn
//!    wiring ratchet: `feed_consumer_convergence_guard.rs` pins only that
//!    catch-up ROUTES THROUGH `catch_up_seal_all`, not that the driver is
//!    spawned. A refactor dropping either `tokio::spawn` compiles green,
//!    passes every behavioral test, and silently stalls catch-up sealing —
//!    and the only alarm (`boundary_catchup_storm_dhan`) is a CEILING
//!    (too many seals), never a floor.
//! 2. The IST-midnight force-seal (main.rs Task 3; Groww variant) spawn
//!    was unpinned for Dhan (only behavioral function tests existed), and
//!    NEITHER midnight task's BOUNDARY-01-mandated ordering —
//!    `force_seal_all(…)` THEN `reset_watermark()` — was pinned anywhere.
//!    Losing the reset means a POISONED watermark never self-heals
//!    (catch-up disabled forever); reordering it before the seal would
//!    reset a watermark the seal pass still needs.
//!
//! LIVENESS-FLOOR DECISION (GAP-1b, recorded with evidence in
//! `wave-6-error-codes.md` BOUNDARY-01 "2026-07-14 Update"): NO CloudWatch
//! floor alarm is added. `tv_boundary_catchup_total{feed}` increments only
//! per ROUTED catch-up seal and is legitimately ~0 all day when the feed
//! is healthy (sparse — silence is ambiguous, a floor would false-page),
//! and a registered counter keeps emitting per-scrape samples from the
//! metrics recorder regardless of the driver task's liveness, so metric
//! presence can never prove the task is alive. This build-failing spawn
//! ratchet is the honest mechanical fix; the runtime residual is
//! documented, never camouflaged.
//!
//! House precedents: `seal_drop_paging_wiring_guard.rs` (main.rs source
//! scan from an app integration test, `://`-aware comment stripping),
//! `boot_helpers.rs::ratchet_main_rs_spawns_close_time_force_seal` (the
//! Task 3b close-time sibling this extends to Tasks 3 + 4),
//! `error_code_paging_filter_drift_guard.rs` (the module-aware
//! production-region splitter — groww_bridge.rs carries an EARLY
//! `#[cfg(test)]`-gated fn at line ~908 that a naive first-token split
//! would truncate on, hiding the real production spawn sites — the
//! WS-GAP-07 false-dead class).

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code,
/// the `http_client_fallback_guard.rs` precedent) so a comment mentioning a
/// needle can never satisfy the scan. Block anchors are located in the RAW
/// text (the `// --- Task N:` markers ARE comments); needles are asserted
/// against the STRIPPED window.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Everything strictly before the first test-`cfg`-gated MODULE — the
/// production region (module-aware, the `error_code_paging_filter_drift_guard`
/// pattern). Deliberately NOT a naive split at the first `#[cfg(test)]`
/// token: groww_bridge.rs gates a test-only convenience fn near the TOP of
/// the file (`with_aggregator`, ~line 908) — a naive split there would hide
/// the real production spawn sites further down (the WS-GAP-07 false-dead
/// class the drift guard documented on 2026-07-10).
fn production_region(body: &str) -> &str {
    fn is_test_cfg(attr: &str) -> bool {
        attr.starts_with("#[cfg(") && attr.contains("test") && !attr.contains("not(test)")
    }
    let mut offset = 0usize;
    let mut lines = body.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if is_test_cfg(trimmed) {
            // Same-line form: `#[cfg(test)] mod tests {`
            if let Some(close) = trimmed.find(']')
                && trimmed[close + 1..].trim_start().starts_with("mod ")
            {
                return &body[..offset];
            }
            // Next-non-attribute-line form.
            let mut ahead = lines.clone();
            let mut next_code = None;
            for l in ahead.by_ref() {
                let t = l.trim();
                if t.is_empty() || t.starts_with("#[") {
                    continue;
                }
                next_code = Some(t);
                break;
            }
            if let Some(t) = next_code
                && (t.starts_with("mod ")
                    || t.starts_with("pub mod ")
                    || t.starts_with("pub(") && t.contains(" mod "))
            {
                return &body[..offset];
            }
        }
        offset += line.len();
    }
    body
}

/// Slice the RAW window between a start anchor and (when found) an end
/// anchor; panics with a named message when the start anchor is missing.
fn raw_window<'a>(src: &'a str, start_anchor: &str, end_anchor: Option<&str>) -> &'a str {
    let start = src.find(start_anchor).unwrap_or_else(|| {
        panic!("anchor `{start_anchor}` must exist — the task block was removed or renamed")
    }); // APPROVED: test
    let rest = &src[start..];
    match end_anchor.and_then(|e| rest[start_anchor.len()..].find(e)) {
        Some(rel_end) => &rest[..start_anchor.len() + rel_end],
        None => rest,
    }
}

/// True iff BOTH needles are present in `window` and the FIRST occurrence
/// of `first` precedes the FIRST occurrence of `second`.
fn ordered(window: &str, first: &str, second: &str) -> bool {
    match (window.find(first), window.find(second)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

// ---------------------------------------------------------------------
// GAP-2: main.rs Task 3 — IST-midnight force-seal spawn + watermark reset
// ---------------------------------------------------------------------

#[test]
fn test_main_rs_spawns_ist_midnight_force_seal_with_watermark_reset() {
    let full = read_repo_file("src/main.rs");
    let prod = production_region(&full);
    // Anchor on the raw block markers (they are comments — located BEFORE
    // stripping); assert needles against the STRIPPED window so a comment
    // (e.g. Task 3b's literal "no `reset_watermark()` here" note) can never
    // satisfy or pollute the check.
    let window = strip_line_comments(raw_window(
        prod,
        "Task 3: IST-midnight boundary force-seal",
        Some("Task 3b:"),
    ));
    assert!(
        window.contains("tokio::spawn("),
        "the Task 3 IST-midnight force-seal block must still spawn its task \
         — dropping the spawn silently fuses day-N candle state into \
         day-(N+1)'s first bar"
    );
    assert!(
        window.contains("is_trading_day_today()"),
        "the Task 3 midnight force-seal must gate on \
         TradingCalendar::is_trading_day_today() (weekends/holidays skip)"
    );
    assert!(
        ordered(&window, "force_seal_all(", ".reset_watermark()"),
        "the Task 3 midnight force-seal must call force_seal_all( and THEN \
         reset_watermark() (BOUNDARY-01 contract: both midnight tasks reset \
         the watermark right after force_seal_all so a POISONED watermark \
         self-heals within one day; resetting BEFORE the seal — or not at \
         all — breaks the catch-up driver's self-heal)"
    );
}

// ---------------------------------------------------------------------
// GAP-1a: main.rs Task 4 — the Dhan watermark catch-up seal driver
// ---------------------------------------------------------------------

#[test]
fn test_main_rs_spawns_watermark_catchup_driver() {
    let full = read_repo_file("src/main.rs");
    let prod = production_region(&full);
    let window = strip_line_comments(raw_window(
        prod,
        "Task 4: watermark-aware per-minute catch-up seal",
        None,
    ));
    assert!(
        window.contains("tokio::spawn("),
        "the Task 4 catch-up seal driver must still be spawned — a dropped \
         spawn silently stalls BOUNDARY-01 catch-up sealing for Dhan (the \
         boundary_catchup_storm alarm is a CEILING; nothing pages on zero)"
    );
    for needle in [
        "CATCHUP_SEAL_POLL_INTERVAL_SECS",
        "compute_catchup_cutoff(",
        "catch_up_seal_all(",
        "is_trading_day_today()",
        "tv_boundary_catchup_total",
    ] {
        assert!(
            window.contains(needle),
            "the Task 4 catch-up driver block must still carry `{needle}` — \
             the 5s poll cadence, the watermark/poison gate, the seal pass, \
             the trading-day gate and the per-seal counter (which feeds the \
             storm alarm + dashboard) are all load-bearing"
        );
    }
}

// ---------------------------------------------------------------------
// GAP-1a: groww_bridge.rs — the Groww catch-up seal driver
// ---------------------------------------------------------------------

#[test]
fn test_groww_bridge_spawns_catchup_driver() {
    let full = read_repo_file("src/groww_bridge.rs");
    let prod = strip_line_comments(production_region(&full));
    // The driver must be DEFINED …
    assert!(
        prod.contains("fn spawn_groww_catchup_seal("),
        "groww_bridge.rs must define the spawn_groww_catchup_seal driver"
    );
    // … AND CALLED at least once in the production region (definition
    // occurrences carry the `fn ` prefix; whitespace-normalize so rustfmt
    // re-wrapping cannot split `fn` from the name).
    let compacted: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
    let total = compacted.matches("spawn_groww_catchup_seal(").count();
    let defs = compacted.matches("fnspawn_groww_catchup_seal(").count();
    assert!(
        total > defs,
        "groww_bridge.rs must CALL spawn_groww_catchup_seal( in its \
         production region (found {defs} definition(s), {total} total \
         occurrence(s)) — without the call the Groww BOUNDARY-01 catch-up \
         driver silently never runs"
    );
    // The driver body must keep its load-bearing pieces.
    let body = strip_line_comments(raw_window(
        production_region(&full),
        "fn spawn_groww_catchup_seal(",
        None,
    ));
    for needle in [
        "CATCHUP_SEAL_POLL_INTERVAL_SECS",
        "compute_catchup_cutoff(",
        "catch_up_seal_all(",
        "tv_boundary_catchup_total",
    ] {
        assert!(
            body.contains(needle),
            "spawn_groww_catchup_seal must still carry `{needle}` (poll \
             cadence / watermark gate / seal pass / per-seal counter)"
        );
    }
}

// ---------------------------------------------------------------------
// GAP-2: groww_bridge.rs — midnight force-seal watermark-reset ordering
// (the existing test_run_groww_bridge_spawns_ist_midnight_force_seal pins
// the SPAWN; this pins the BOUNDARY-01 reset ordering it does not cover.)
// ---------------------------------------------------------------------

#[test]
fn test_groww_midnight_force_seal_resets_watermark_after_seal() {
    let full = read_repo_file("src/groww_bridge.rs");
    let window = strip_line_comments(raw_window(
        production_region(&full),
        "fn spawn_groww_ist_midnight_force_seal(",
        Some("fn spawn_groww_catchup_seal("),
    ));
    assert!(
        ordered(&window, "force_seal_all(", ".reset_watermark()"),
        "the Groww IST-midnight force-seal must call force_seal_all( and \
         THEN reset_watermark() (BOUNDARY-01: the midnight reset is the \
         ONLY self-heal for a poisoned Groww watermark)"
    );
}

// ---------------------------------------------------------------------
// Anti-vacuity: the helpers must DETECT planted drift on synthetic input
// (a guard that cannot fail is a false-OK, audit Rule 11).
// ---------------------------------------------------------------------

#[test]
fn test_synthetic_ordering_helper_detects_planted_drift() {
    // Correct order passes.
    assert!(ordered(
        "a.force_seal_all(|x| {});\na.reset_watermark();",
        "force_seal_all(",
        ".reset_watermark()"
    ));
    // Planted drift: reset BEFORE the seal must fail.
    assert!(!ordered(
        "a.reset_watermark();\na.force_seal_all(|x| {});",
        "force_seal_all(",
        ".reset_watermark()"
    ));
    // Planted drift: missing reset must fail.
    assert!(!ordered(
        "a.force_seal_all(|x| {});",
        "force_seal_all(",
        ".reset_watermark()"
    ));
    // A comment-only mention must NOT satisfy the stripped scan (the Task
    // 3b "no `reset_watermark()` here" note is exactly this shape).
    let commented =
        strip_line_comments("a.force_seal_all(|x| {});\n// NOTE: no `.reset_watermark()` here\n");
    assert!(!ordered(
        &commented,
        "force_seal_all(",
        ".reset_watermark()"
    ));
    // `://` inside a string survives stripping (URL-aware stripper).
    let url_kept = strip_line_comments("let u = \"wss://api-feed.dhan.co\"; // trailing\n");
    assert!(url_kept.contains("wss://api-feed.dhan.co") && !url_kept.contains("trailing"));
}

#[test]
fn test_synthetic_production_region_is_module_aware() {
    // An EARLY #[cfg(test)] gating a NON-module item must NOT truncate the
    // region (the groww_bridge.rs ~line-908 shape); the trailing
    // #[cfg(test)] mod tests must.
    let src = "#[cfg(test)]\n\
               fn test_only_helper() {}\n\
               fn real() { spawn_groww_catchup_seal(x); }\n\
               #[cfg(test)]\n\
               mod tests { fn f() { spawn_groww_catchup_seal(y); } }\n";
    let prod = production_region(src);
    assert!(
        prod.contains("fn real()"),
        "an early cfg(test)-gated fn must not hide later production code"
    );
    assert!(
        !prod.contains("mod tests"),
        "the trailing cfg(test) mod tests must be excluded"
    );
    // cfg(not(test)) gates PRODUCTION code and must never truncate.
    let not_test = "#[cfg(not(test))]\nmod prod { fn f() {} }\nfn tail() {}\n";
    assert!(production_region(not_test).contains("fn tail()"));
}
