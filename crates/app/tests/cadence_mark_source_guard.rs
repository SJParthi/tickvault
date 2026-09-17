//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning the order
//! runtime's LIVE mark producer on the cadence path (2026-07-18).
//!
//! The incident this guards against: PR #1624's cadence cutover stood the
//! legacy Groww per-minute legs down (`[groww_spot_1m]` /
//! `[groww_contract_1m]` enabled = false) while those legs carried the
//! ONLY `mark_forward` call sites — the order runtime's mark channel had
//! ZERO producers at boot, the sole sender dropped, and the Fix-F arm
//! swallowed the closed channel as the benign day-complete state. Paper
//! fills + unrealized-P&L marks died silently; the daily paper self-test
//! failed loudly at its 180s AwaitingMark timeout (OMS-GAP-06,
//! log-sink-only). BITE-PROVEN: the §1 needle
//! (`forwarder.mark_forward(` in groww_cadence_executor.rs) is ABSENT at
//! origin/main 0f5aa760 — this guard FAILS there by construction.
//!
//! Pins:
//! 1. the GROWW cadence executor forwards marks at EXACTLY TWO sites —
//!    the SPOT persist-confirm seam and (2026-07-18 deliberate ratchet
//!    widening, Item 3) the CHAIN persist-confirm seam — each
//!    Option-gated and AFTER its own persist → flush-ACK chain (a mark
//!    must never reference a price the audit record does not back); the
//!    chain seam additionally resolves REAL exchange_token identities
//!    from the day-cached master-derived contract index (a synthetic id
//!    is the id-space-divergence class this guard bans);
//! 2. the DHAN cadence executor carries NO mark tap (id-space ban);
//! 3. main.rs threads the forwarder into the cadence boot call;
//! 4. cadence_boot.rs passes it to the GROWW executor only;
//! 5. order_runtime.rs's Fix-F arm distinguishes never-any-mark from the
//!    benign day-complete close.
//!
//! Mirrors the codebase's `*_wiring_guard` pattern
//! (`cadence_boot_wiring_guard.rs`, `order_runtime_spawn_site_guard.rs`).
//! Reads SOURCE text, so it runs on the default build independent of any
//! feature flag.

use std::fs;
use std::path::PathBuf;

fn app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The PRODUCTION region of a source file: everything above the first
/// column-0 `#[cfg(test)]` line (the house production-region split — the
/// margin_gate_off_guard / cadence_boot_wiring_guard precedent). A
/// test-module mention of a pinned needle can never satisfy or
/// double-count a production pin.
fn production_region(src: &str) -> &str {
    match src.find("\n#[cfg(test)]") {
        Some(at) => &src[..at],
        None => src,
    }
}

/// Strip `//` line comments, treating `://` (URL scheme separators inside
/// string literals) as code — the house stripper copied verbatim from
/// `cadence_boot_wiring_guard.rs` (itself the
/// `http_client_fallback_guard.rs` precedent). Needle scans run on the
/// STRIPPED source so a prose comment carrying a needle can never
/// vacuously satisfy a pin.
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

/// Collapse every whitespace run to a single space — source-shape needle
/// matching tolerant of rustfmt line wrapping.
fn normalize_ws(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Comment-stripped, whitespace-normalized production region.
fn scan_region(rel: &str) -> String {
    let src = app_src(rel);
    normalize_ws(&strip_line_comments(production_region(&src)))
}

// ---- Pins 1-4 RETIRED 2026-09-16: there is no cadence mark producer ----
//
// `test_dhan_cadence_executor_is_now_the_mark_producer`,
// `test_main_threads_forwarder_into_cadence_boot` and
// `test_cadence_boot_passes_the_forwarder_to_the_sole_executor` pinned the
// mark tap onto the cadence executor and the forwarder's route from main.rs
// through cadence_boot. All three subjects are gone: the operator's
// SOCKETS-ONLY narrowing removed the per-minute price pulls
// (`no-rest-except-live-feed-2026-06-27.md` §12.10), and `CadenceExecutor`
// declared exactly the three removed fetch methods, so the scheduler, its
// boot and the executor went with them.
//
// ⚠ THE INCIDENT IN THE MODULE DOCBLOCK ABOVE IS NOT HISTORICAL — IT IS THE
// SHAPE THIS BRANCH WALKED INTO AGAIN, FROM THE OTHER DIRECTION.
//
// PR #1624 stood the legacy legs down while they carried the only
// `mark_forward` call sites, so the order runtime's mark channel had zero
// producers, the sole sender dropped, and the Fix-F arm read the closed
// channel as the benign day-complete state. Paper fills and unrealized-P&L
// marks died silently.
//
// This removal deleted the cadence executor that #1624 had made the
// producer — and the sender was MOVED BY VALUE into the cadence spawn, so
// deleting that spawn left the binding alive in `async_main`'s frame for the
// process lifetime. The channel would therefore NEVER close, `recv()` would
// pend forever, and the producer-less warn at the Fix-F arm could not fire:
// the same silent stall as #1624, except that #1624 was at least loud at the
// 180s AwaitingMark timeout. `main.rs` now explicitly drops
// `order_runtime_mark_forwarder` so the channel CLOSES and the arm pinned
// below is reachable. §12.10.7(a) carries the measurement.
//
// So pins 1-4 are retired and pins 5+ are KEPT AND LOAD-BEARING: they are
// the only pins on the never-any-mark and abnormal-death arms, and this
// branch is the reason those arms are the live signal rather than a
// fallback. Deleting this whole file to make the suite green would have
// removed the guard on the exact defect the removal re-created.
//
// NOT claimed: that a mark producer exists. None does. The paper book runs
// unmarked and now SAYS SO, which is the whole difference.
#[test]
fn test_order_runtime_fix_f_distinguishes_never_any_mark() {
    // The Fix-F closed-channel arm must not claim "day complete" when NO
    // mark ever arrived (the producer-less boot shape). Needles live in
    // string literals, so scan the comment-stripped production region
    // WITHOUT normalization (the literals carry `\`-continuations; each
    // needle sits on one physical line).
    let src = app_src("src/order_runtime.rs");
    let prod = strip_line_comments(production_region(&src));
    for needle in [
        "saw_any_mark",
        "mark channel closed before any mark",
        "no live mark producer is configured",
    ] {
        assert!(
            prod.contains(needle),
            "order_runtime.rs production region lost `{needle}` — the Fix-F \
             closed-channel warn no longer distinguishes a producer-less \
             boot from the benign day-complete close"
        );
    }
}

#[test]
fn test_order_runtime_abnormal_mid_session_death_arm_is_coded() {
    // MED-1 (2026-07-18): the disarm arm's THIRD case — marks flowed and
    // the channel closed BEFORE the 15:30 IST close boundary — must stay a
    // coded OMS-GAP-02 error! + a static counter. Scan the None-arm window
    // (from `let Some(first) = mark else` forward) of the comment-stripped
    // production region so the lagged-receiver arm's own OmsGapReconciliation
    // emit can never vacuously satisfy the needle.
    let src = app_src("src/order_runtime.rs");
    let prod = strip_line_comments(production_region(&src));
    let at = prod
        .find("let Some(first) = mark else")
        .expect("order_runtime.rs lost the mark-channel None (disarm) arm");
    let window = &prod[at..(at + 2_500).min(prod.len())];
    for needle in [
        "OmsGapReconciliation.code_str()",
        "tv_order_runtime_mark_producer_lost_total",
        "mark channel closed MID-SESSION after",
        "TICK_PERSIST_END_SECS_OF_DAY_IST",
    ] {
        assert!(
            window.contains(needle),
            "order_runtime.rs mark-channel disarm arm lost `{needle}` — the \
             abnormal MID-SESSION producer-death case is no longer a coded \
             OMS-GAP-02 error + counter (a mid-session mark-producer death \
             would freeze paper fills + P&L marks silently)"
        );
    }
}

#[test]
fn test_scanner_self_check() {
    // The stripper removes a comment-borne needle but keeps code + URLs.
    let stripped = strip_line_comments(
        "// mark_forward in prose never satisfies\nlet url = \"https://x\"; f.mark_forward(1, 0, 2.0);\n",
    );
    assert!(stripped.contains(".mark_forward("));
    assert!(stripped.contains("https://x"));
    assert!(!stripped.contains("prose never satisfies"));
    // And normalize_ws makes wrapped shapes matchable.
    assert_eq!(
        normalize_ws("if let Some(forwarder) =\n    self.mark_forwarder.as_ref()"),
        "if let Some(forwarder) = self.mark_forwarder.as_ref()"
    );
}
