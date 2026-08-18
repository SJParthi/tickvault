//! Source-scan ratchet: every production WebSocket dial MUST disable Nagle.
//!
//! WHY THIS FILE EXISTS (2026-08-18)
//! ================================
//! `connection.rs` carried the comment *"Pinned by
//! `dial_disables_nagle_on_every_endpoint`"* — and that test **did not
//! exist anywhere in the workspace**. The only occurrence of the name was
//! the comment asserting it. So the main feed's correct setting was
//! protected by nothing, and the order-update socket had silently shipped
//! the opposite value (`false`) with no guard to catch the divergence.
//!
//! That is the phantom-guard class: a comment that claims enforcement,
//! reads as enforcement in review, and enforces nothing. This file is the
//! missing caller.
//!
//! WHY NAGLE MATTERS HERE
//! ======================
//! `tokio_tungstenite::connect_async_tls_with_config` takes Nagle control as
//! a POSITIONAL BOOLEAN — the third argument, `disable_nagle` — not a
//! `set_nodelay()` method call. An audit that greps for `set_nodelay` finds
//! zero hits and wrongly concludes Nagle is enabled everywhere; one that
//! greps for the literal `true` finds nothing meaningful either. Only the
//! argument position tells the truth, which is exactly why this needs a
//! structural guard rather than a comment.
//!
//! With Nagle ON, a small write is held until the previous segment is ACKed
//! — up to ~40ms. Both sockets carry mostly tiny control frames (auth
//! payloads, subscribe batches, pong replies) and Dhan tears the connection
//! down on a missed ping deadline, so a delayed pong is a self-inflicted
//! disconnect followed by a full re-auth.
//!
//! HONEST SCOPE
//! ============
//! This is a SOURCE SCAN, not a runtime assertion. It proves the argument is
//! spelled correctly at every production dial site; it does NOT prove the
//! kernel honoured `TCP_NODELAY` on the box, and it cannot measure latency.
//! It also deliberately ignores `#[cfg(test)]` and `tests/` code, where a
//! mock dial may legitimately differ.

use std::path::{Path, PathBuf};

/// Production dial sites that MUST pass `disable_nagle = true`.
///
/// Keep this list in lockstep with reality: a new WebSocket endpoint that
/// dials via `connect_async*` belongs here, or the guard silently stops
/// covering it — the very failure this file was written to end.
const DIAL_SITES: &[&str] = &[
    "src/websocket/connection.rs",
    "src/websocket/order_update_connection.rs",
];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Strip `//` line comments so a commented-out example can never satisfy —
/// or trip — the scan. Block comments are not used at these call sites.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse whitespace so an argument list broken across lines by rustfmt
/// still matches. Without this the guard would break the first time the
/// formatter wrapped a call — passing vacuously by finding no call at all.
fn normalize(src: &str) -> String {
    src.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn read_site(rel: &str) -> String {
    let path: PathBuf = crate_root().join(rel);
    assert!(
        Path::new(&path).exists(),
        "dial site {rel} does not exist — either it moved (update DIAL_SITES) \
         or this guard has been scanning nothing"
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("failed to read {rel}: {err}");
    });
    normalize(&strip_line_comments(&raw))
}

#[test]
fn every_production_dial_disables_nagle() {
    for rel in DIAL_SITES {
        let src = read_site(rel);

        // The dial must exist at all — a guard that finds no call site and
        // reports success is the vacuous pass this file exists to prevent.
        assert!(
            src.contains("connect_async_tls_with_config("),
            "{rel}: no connect_async_tls_with_config call found — the dial moved \
             and this guard is now vacuous"
        );

        // `disable_nagle` is the THIRD positional argument. Assert the two
        // real shapes rather than parsing Rust: `None` config (order-update)
        // and a `config` binding (main feed).
        let ok = src.contains("connect_async_tls_with_config(request, None, true,")
            || src.contains("connect_async_tls_with_config(request, config, true,");

        assert!(
            ok,
            "{rel}: the dial does NOT pass disable_nagle = true.\n\
             The third positional argument of connect_async_tls_with_config \
             controls Nagle. With it false, small control frames (auth, \
             subscribe batches, pongs) are held up to ~40ms, and a delayed \
             pong past Dhan's ping deadline is a self-inflicted disconnect \
             plus a full re-auth."
        );
    }
}

#[test]
fn no_production_dial_uses_the_nagle_less_convenience_helper() {
    // `connect_async(url)` has no disable_nagle parameter at all, so a dial
    // added through it silently reintroduces Nagle while looking harmless.
    for rel in DIAL_SITES {
        let src = read_site(rel);
        assert!(
            !src.contains("= connect_async("),
            "{rel}: uses connect_async(), which cannot disable Nagle. Use \
             connect_async_tls_with_config with disable_nagle = true."
        );
    }
}

#[test]
fn guard_self_test_detects_a_reintroduced_nagle() {
    // Bite test: the guard must actually FAIL on the defect it claims to
    // catch. Without this, a scan that matches nothing passes forever.
    let regressed = normalize(&strip_line_comments(
        "let (ws, _r) = connect_async_tls_with_config(request, None, false, Some(tls)).await?;",
    ));
    assert!(
        !(regressed.contains("connect_async_tls_with_config(request, None, true,")
            || regressed.contains("connect_async_tls_with_config(request, config, true,")),
        "self-test: the matcher accepted a disable_nagle = false dial — it would \
         never catch a real regression"
    );

    let fixed = normalize(&strip_line_comments(
        "let (ws, _r) = connect_async_tls_with_config(request, None, true, Some(tls)).await?;",
    ));
    assert!(
        fixed.contains("connect_async_tls_with_config(request, None, true,"),
        "self-test: the matcher rejected a correct dial — it would fail the build \
         on good code"
    );
}

#[test]
fn comment_only_mentions_cannot_satisfy_the_scan() {
    // The phantom-guard failure mode in miniature: prose that looks like the
    // real thing must not count as the real thing.
    let commented = normalize(&strip_line_comments(
        "// connect_async_tls_with_config(request, None, true, Some(tls))",
    ));
    assert!(
        !commented.contains("connect_async_tls_with_config"),
        "self-test: a commented-out dial satisfied the scan — the comment \
         stripper is not working, and this guard could pass on prose alone"
    );
}
