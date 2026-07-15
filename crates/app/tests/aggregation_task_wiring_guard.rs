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
///
/// 2026-07-14 mutation-review FIX-2: the lookahead SKIPS `//` comment
/// lines exactly like attribute lines. main.rs's test-module header
/// stacks `#[cfg(test)]`, `#[allow(…)]`, an interleaved `// APPROVED:`
/// comment, another `#[allow(…)]`, then `mod tests {` — the earlier
/// lookahead treated the comment line as "next code, not a module" and
/// silently returned the WHOLE file, so any future needle-like string
/// literal inside main.rs's mod tests would have made a scan permanently
/// vacuous. Pinned live by
/// `test_production_region_truncates_real_test_modules` and synthetically
/// below.
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
            // Next-non-attribute, non-comment-line form (FIX-2: comment
            // lines interleaved between attributes must not end the scan).
            let mut ahead = lines.clone();
            let mut next_code = None;
            for l in ahead.by_ref() {
                let t = l.trim();
                if t.is_empty() || t.starts_with("#[") || t.starts_with("//") {
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

/// Slice the RAW window between a start anchor and an end anchor.
/// PANICS with a named message when EITHER anchor is missing (2026-07-14
/// mutation-review LOW residual: a missing END anchor previously fell
/// back SILENTLY to the open-ended rest of the file — so renaming the
/// end-anchor marker AND de-spawning the block in one change re-opened
/// the FIX-1 escape). `end_anchor = None` means deliberately open-ended.
fn raw_window<'a>(src: &'a str, start_anchor: &str, end_anchor: Option<&str>) -> &'a str {
    let start = src.find(start_anchor).unwrap_or_else(|| {
        panic!("anchor `{start_anchor}` must exist — the task block was removed or renamed")
    }); // APPROVED: test
    let rest = &src[start..];
    match end_anchor {
        None => rest,
        Some(e) => {
            let rel_end = rest[start_anchor.len()..].find(e).unwrap_or_else(|| {
                panic!(
                    "end anchor `{e}` must exist after `{start_anchor}` — the \
                     window boundary marker was removed or renamed (a silent \
                     open-ended fallback would let a later needle rescue a \
                     gutted block)"
                )
            }); // APPROVED: test
            &rest[..start_anchor.len() + rel_end]
        }
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
    // 2026-07-14 mutation-review FIX-1: the window MUST be end-anchored at
    // the next section marker ("D2-pre:") — with an open-ended window the
    // `tokio::spawn(` needle was satisfiable by ANY later spawn in main.rs
    // (mutation `tokio::spawn(async move {` -> `let _dead = async move {`
    // inside Task 4 passed the guard). The spawn needle must be INSIDE the
    // Task 4 block.
    let window = strip_line_comments(raw_window(
        prod,
        "Task 4: watermark-aware per-minute catch-up seal",
        Some("D2-pre:"),
    ));
    assert!(
        window.contains("tokio::spawn("),
        "the Task 4 catch-up seal driver must still be spawned (the spawn \
         must be INSIDE the Task 4 block, before the D2-pre section) — a \
         dropped/de-spawned driver silently stalls BOUNDARY-01 catch-up \
         sealing for Dhan (the boundary_catchup_storm alarm is a CEILING; \
         nothing pages on zero)"
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

// RETIRED 2026-07-15 (Groww live-feed deletion): the GAP-1a Groww catch-up
// driver test (test_groww_bridge_spawns_catchup_driver) and the GAP-2 Groww
// midnight force-seal reset-ordering test
// (test_groww_midnight_force_seal_resets_watermark_after_seal) died with
// groww_bridge.rs — the Groww live lane (and its aggregator instance) is
// deleted; the Dhan Task 3/3b/4 pins above are the surviving contract.

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
    // FIX-2 regression pin (2026-07-14 mutation review): main.rs's real
    // test-module header interleaves a `// APPROVED:` comment between
    // #[allow] attributes and `mod tests {` — the lookahead must skip
    // comment lines too, or the splitter silently degrades to whole-file
    // and any needle-like literal inside mod tests becomes a permanent
    // false-satisfier.
    let interleaved = "fn real() {}\n\
                       #[cfg(test)]\n\
                       #[allow(clippy::items_after_test_module)]\n\
                       // APPROVED: comment between attributes and the module\n\
                       #[allow(clippy::assertions_on_constants)]\n\
                       mod tests { fn f() { spawn_groww_catchup_seal(x); } }\n";
    let prod = production_region(interleaved);
    assert!(
        prod.contains("fn real()") && !prod.contains("spawn_groww_catchup_seal("),
        "an attribute+comment+`mod tests {{` header must still truncate — \
         a planted needle inside such a module must NOT be visible: {prod}"
    );
}

/// FIX-2 live self-check (2026-07-14 mutation review): the splitter must
/// ACTUALLY truncate both scanned files' trailing test modules — before
/// the comment-skip fix it silently returned main.rs WHOLE (prod len ==
/// full len), so every whole-region scan was one needle-like test literal
/// away from permanent vacuity.
#[test]
fn test_production_region_truncates_real_test_modules() {
    // (2026-07-15: groww_bridge.rs retired from the fixture list with the
    // Groww live-feed deletion — main.rs remains the live scanned file.)
    for rel in ["src/main.rs"] {
        let full = read_repo_file(rel);
        let prod = production_region(&full);
        assert!(
            prod.len() < full.len(),
            "{rel}: production_region returned the WHOLE file ({} bytes) — \
             the trailing #[cfg(test)] mod tests module was not truncated \
             (the FIX-2 silent-degrade class)",
            full.len()
        );
    }
}

/// FIX-1 synthetic pin (2026-07-14 mutation review): the de-spawn shape —
/// `tokio::spawn(async move {` replaced by `let _dead = async move {`
/// inside the anchored block, with a LATER spawn elsewhere in the file —
/// must fail the end-anchored window check (with an open-ended window the
/// later spawn satisfied the needle).
#[test]
fn test_synthetic_end_anchored_window_catches_de_spawn() {
    let src = "// --- Task 4: watermark-aware per-minute catch-up seal ---\n\
               let _dead = async move { catch_up_seal_all(cutoff); };\n\
               // D2-pre: next section\n\
               fn later() { tokio::spawn(async move {}); }\n";
    let window = strip_line_comments(raw_window(
        src,
        "Task 4: watermark-aware per-minute catch-up seal",
        Some("D2-pre:"),
    ));
    assert!(
        !window.contains("tokio::spawn("),
        "a de-spawned block must NOT be satisfied by a spawn AFTER the end \
         anchor: {window}"
    );
    // And the healthy shape passes:
    let healthy = "// --- Task 4: watermark-aware per-minute catch-up seal ---\n\
                   tokio::spawn(async move { catch_up_seal_all(cutoff); });\n\
                   // D2-pre: next section\n";
    let window = strip_line_comments(raw_window(
        healthy,
        "Task 4: watermark-aware per-minute catch-up seal",
        Some("D2-pre:"),
    ));
    assert!(window.contains("tokio::spawn("));
}

/// End-anchor rename pin (2026-07-14 mutation-review LOW residual): a
/// RENAMED end-anchor marker must PANIC loudly, never silently widen the
/// window to the rest of the file (which would let a later spawn rescue
/// a de-spawned block — the FIX-1 escape through the back door).
#[test]
fn test_synthetic_missing_end_anchor_panics_instead_of_widening() {
    let renamed = "// --- Task 4: watermark-aware per-minute catch-up seal ---\n\
                   let _dead = async move { catch_up_seal_all(cutoff); };\n\
                   // D2-RENAMED: next section\n\
                   fn later() { tokio::spawn(async move {}); }\n";
    let result = std::panic::catch_unwind(|| {
        raw_window(
            renamed,
            "Task 4: watermark-aware per-minute catch-up seal",
            Some("D2-pre:"),
        )
        .len()
    });
    assert!(
        result.is_err(),
        "a missing/renamed END anchor must panic loudly — a silent \
         open-ended fallback re-opens the FIX-1 de-spawn escape"
    );
    // The healthy shape still slices (no panic, later spawn excluded):
    let healthy = "// --- Task 4: watermark-aware per-minute catch-up seal ---\n\
                   tokio::spawn(async move {});\n\
                   // D2-pre: next section\n\
                   fn later() { tokio::spawn(async move {}); }\n";
    let window = raw_window(
        healthy,
        "Task 4: watermark-aware per-minute catch-up seal",
        Some("D2-pre:"),
    );
    assert!(window.contains("tokio::spawn(") && !window.contains("fn later()"));
}

/// FIX-3 synthetic pin (2026-07-14 mutation review): deleting the LIVE
/// single-conn `spawn_groww_catchup_seal(` call must fail even while the
/// dormant fleet wrapper still calls it — the per-wrapper window must not
/// be satisfiable by the other wrapper's call.
#[test]
fn test_synthetic_per_wrapper_window_catches_single_conn_call_deletion() {
    let mutated = "pub fn spawn_supervised_groww_bridge(x: u32) {\n\
                   // catch-up call DELETED here (the mutation)\n\
                   }\n\
                   pub fn spawn_supervised_groww_shard_bridges(x: u32) {\n\
                   spawn_groww_catchup_seal(a, b, c);\n\
                   }\n";
    let single_conn = strip_line_comments(raw_window(
        mutated,
        "pub fn spawn_supervised_groww_bridge(",
        Some("pub fn spawn_supervised_groww_shard_bridges("),
    ));
    assert!(
        !single_conn.contains("spawn_groww_catchup_seal("),
        "the single-conn wrapper's window must NOT be satisfied by the \
         fleet wrapper's call: {single_conn}"
    );
    let fleet = strip_line_comments(raw_window(
        mutated,
        "pub fn spawn_supervised_groww_shard_bridges(",
        None,
    ));
    assert!(fleet.contains("spawn_groww_catchup_seal("));
}
