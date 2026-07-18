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
//! 1. the GROWW cadence executor forwards marks at EXACTLY ONE site,
//!    Option-gated, AFTER the persist → flush-ACK → heartbeat → fold
//!    handoff chain (a mark must never reference a price the audit
//!    record does not back);
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

#[test]
fn test_groww_cadence_executor_forwards_marks_after_persist_confirm() {
    let scan = scan_region("src/groww_cadence_executor.rs");

    // EXACTLY ONE forward site (the 1-tap-per-leg discipline of
    // order_runtime_spawn_site_guard::ratchet_groww_rest_leg_mark_taps_pinned
    // applied to the cadence producer): a second site would mean a
    // non-own-fire arm started producing marks (the stale-price class).
    let forwards = scan.matches(".mark_forward(").count();
    assert_eq!(
        forwards, 1,
        "groww_cadence_executor.rs production region must forward marks at \
         EXACTLY ONE site (the spot persist-confirm seam) — found \
         {forwards}; zero re-opens the PR #1624 producer-less mark channel \
         (paper fills + P&L marks silently dead)"
    );

    // Option-gate pin: the tap must be a no-op when [order_runtime] is off.
    assert!(
        scan.contains("if let Some(forwarder) = self.mark_forwarder.as_ref()"),
        "groww_cadence_executor.rs lost the Option-gate on the mark forward \
         — the tap must be a no-op when [order_runtime] is disabled"
    );

    // Source-order pin: persist (append) → flush ACK → heartbeat → fold
    // handoff → mark forward. First occurrences all live in the spot
    // fetch; a mark forwarded BEFORE the flush ACK would reference a
    // price the audit record does not back.
    let needles = [
        "writer.append_row(&row)",
        "flush_off_worker(|| writer.flush())",
        "if flush_ok {",
        "metrics::gauge!(\"tv_rest_1m_fire_heartbeat\")",
        "send_confirmed_bars(",
        ".mark_forward(",
    ];
    let mut last = 0usize;
    for needle in needles {
        let at = scan.find(needle).unwrap_or_else(|| {
            panic!("groww_cadence_executor.rs production region lost `{needle}`")
        });
        assert!(
            at > last,
            "ordering violated: `{needle}` at byte {at} must come after the \
             previous needle (byte {last}) — the persist → flush-ACK → \
             heartbeat → fold → mark contract"
        );
        last = at;
    }
}

#[test]
fn test_dhan_cadence_executor_carries_no_mark_tap() {
    // ID-SPACE BAN: the paper book keys on the Groww-native u64 id space
    // (bit-62 `stable_index_security_id` indices — order-runtime-dryrun.md
    // §3/E9). Dhan cadence spot sids are the Dhan IDX_I space (NIFTY=13,
    // BANKNIFTY=25, SENSEX=51). Feeding Dhan marks alongside Groww marks
    // would DOUBLE-KEY the same instrument as two book entries — invisible
    // to the first-seen-SEGMENT tripwire, because both entries carry the
    // SAME segment code (IDX_I) under different ids. Marks come from the
    // GROWW lane only.
    let scan = scan_region("src/dhan_cadence_executor.rs");
    for needle in ["mark_forward", "MarkForwarder", "mark_forwarder"] {
        assert!(
            !scan.contains(needle),
            "dhan_cadence_executor.rs production region mentions `{needle}` \
             — the Dhan cadence lane must NEVER produce order-runtime marks \
             (cross-ID-SPACE double-keying of the paper book; see the \
             rationale comment in this test)"
        );
    }
}

#[test]
fn test_main_threads_forwarder_into_cadence_boot() {
    let src = strip_line_comments(&app_src("src/main.rs"));
    let at = src
        .find("spawn_cadence_scheduler(")
        .expect("main.rs lost the spawn_cadence_scheduler call");
    let window = &src[at..(at + 400).min(src.len())];
    assert!(
        window.contains("order_runtime_mark_forwarder"),
        "the main.rs spawn_cadence_scheduler call must pass \
         order_runtime_mark_forwarder — without it the cadence Groww \
         executor has no mark tap and the order runtime's mark channel is \
         producer-less at boot (the PR #1624 regression)"
    );
}

#[test]
fn test_cadence_boot_passes_forwarder_to_groww_executor_only() {
    let scan = scan_region("src/cadence_boot.rs");
    // The GROWW constructor window carries the forwarder…
    let groww_at = scan
        .find("GrowwCadenceExecutor::new(")
        .expect("cadence_boot.rs lost GrowwCadenceExecutor::new(");
    let groww_window = &scan[groww_at..(groww_at + 200).min(scan.len())];
    assert!(
        groww_window.contains("groww_mark_forwarder"),
        "cadence_boot.rs must pass groww_mark_forwarder into \
         GrowwCadenceExecutor::new — the sole live mark producer"
    );
    // …and the DHAN constructor window must NOT (the id-space ban).
    let dhan_at = scan
        .find("DhanCadenceExecutor::new(")
        .expect("cadence_boot.rs lost DhanCadenceExecutor::new(");
    let dhan_window = &scan[dhan_at..(dhan_at + 300).min(scan.len())];
    assert!(
        !dhan_window.contains("mark_forwarder"),
        "cadence_boot.rs passes a mark forwarder into the DHAN executor — \
         forbidden (cross-ID-SPACE double-keying of the paper book)"
    );
}

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
