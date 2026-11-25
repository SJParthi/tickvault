//! Source-scan ratchet on the Dhan main-feed WS lane.
//!
//! ⚠ THE FILENAME AND THE 2026-07-13 HEADER BELOW IT ARE BOTH STALE. The
//! lane is NOT retired. It was REVIVED by dated operator quote on
//! 2026-08-09 (`websocket-connection-scope-lock.md`, the revival section
//! plus "SAME DAY, SECOND QUOTE — 16 CONNECTIONS + depth-20/depth-200
//! AUTHORIZED"), its companion plan was flipped to APPROVED on 2026-08-11,
//! and `config/base.toml` carries `dhan_enabled = true`. The dial path
//! `build_feed_url` lives at `crates/core/src/websocket/connection.rs:376`
//! — PRODUCTION code in the very directory these tests scan, since the
//! file's `#[cfg(test)]` does not begin until line 1446.
//!
//! CORRECTED 2026-08-26. The test BODIES were re-blessed correctly on
//! 2026-08-11, with the reasoning written out at each one. What was left
//! behind was every surface a reader meets FIRST: this header, the
//! endpoint test's failure message, and the filename. That is the more
//! dangerous kind of partial re-bless, because the file looks reviewed —
//! and the failure message would have told whoever next tripped it that
//! "the live-WS lane is retired" while it was live and carrying ticks.
//! CLAUDE.md records this exact class twice (the `day_ohlc_tracker` row of
//! 2026-08-12 and the `WAL-SUSPEND-01` row of 2026-08-25), both times
//! having cost real work to whoever trusted the stale text next.
//!
//! The FILENAME is deliberately NOT changed: three plan files under
//! `.claude/plans/` cite it by path, and plan archives are immutable by
//! house convention, so a rename would leave dangling references to buy a
//! nicer name. The header is the fix.
//!
//! WHAT THESE THREE TESTS ACTUALLY ENFORCE TODAY:
//!
//! 1. `test_main_feed_ws_modules_stay_deleted` — a NARROWED deleted-set.
//!    `connection.rs` / `subscription_builder.rs` are REQUIRED now (pinned
//!    positively elsewhere); what stays deleted is the superseded
//!    supervision pair (`connection_pool.rs`, `pool_watchdog.rs` — recreating
//!    them would give the live lane TWO supervisors), plus
//!    `rate_limit_cooldown.rs` and `depth_connection.rs`.
//!
//! 2. `test_no_main_feed_endpoint_connect_path_in_core_src` — the endpoint
//!    HOST is config-driven and never hardcoded in core. `build_feed_url`
//!    takes `base` as a PARAMETER; the literal lives in config (surfaced in
//!    `crates/api/src/state.rs`). This assertion is still true and still
//!    worth having — it keeps the URL in one place and is what lets the
//!    operator move environments — but it no longer has anything to do with
//!    retirement, and its message said otherwise until today.
//!
//! 3. `test_websocket_mod_exports_only_surviving_modules` — the module tree
//!    must not re-declare the superseded lane modules.
//!
//! Still deliberately NOT pinned as absent: the order-update WS
//! (`order_update_connection.rs`) and the shared plumbing it uses
//! (`types` / `activity_watchdog` / `market_hours_gate` / `tls`).

use std::fs;
use std::path::{Path, PathBuf};

fn websocket_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/websocket")
}

#[test]
fn test_main_feed_ws_modules_stay_deleted() {
    // RE-BLESSED 2026-08-11 for the operator-approved 16-connection revival.
    //
    // `connection.rs` and `subscription_builder.rs` were REMOVED from this
    // list because the escape hatch this guard's own message names has been
    // taken: the fresh dated operator quotes landed 2026-08-09 in
    // websocket-connection-scope-lock.md (the revival section plus the
    // "SAME DAY, SECOND QUOTE — 16 CONNECTIONS + depth-20/depth-200
    // AUTHORIZED" section), and the companion plan was flipped to APPROVED
    // on 2026-08-11. Those two modules are now REQUIRED, and their presence
    // is asserted by `test_revived_lane_modules_are_present` below — so the
    // protection is inverted, not dropped.
    //
    // The rest stay deleted. `connection_pool.rs` and `pool_watchdog.rs` are
    // superseded by `pool_supervisor.rs` (a different, tested design), and
    // re-creating the old ones would give the lane TWO supervisors.
    // `depth_connection.rs` predates the revival and its replacement belongs
    // to the depth work, not here.
    for deleted in [
        "connection_pool.rs",
        "pool_watchdog.rs",
        "rate_limit_cooldown.rs",
        "depth_connection.rs", // deleted earlier (AWS-lifecycle PR #4); stays gone
    ] {
        let path = websocket_src_dir().join(deleted);
        assert!(
            !path.exists(),
            "crates/core/src/websocket/{deleted} was DELETED with the Dhan \
             live-WS lane (PR-C2, 2026-07-13). Re-introducing it requires a \
             fresh dated operator quote in websocket-connection-scope-lock.md \
             \"2026-07-13 Amendment\" §D FIRST."
        );
    }
}

/// Comment-stripped, production-region-only view of a source file: drops
/// `//`-prefixed lines and everything from the in-file `mod tests` module
/// down (test fixtures + doc comments may cite the endpoint literal as an
/// example; only PRODUCTION code carrying it is a violation).
fn production_region(src: &str) -> String {
    let cut = src.find("mod tests").unwrap_or(src.len());
    src[..cut]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn scan_rs_files(dir: &Path, hits: &mut Vec<String>, needle: &str) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| {
        // 2026-08-10: was a silent `else { return; }` — an unreadable or
        // MISSING directory became "nothing to check, pass", so the guard
        // could report green while scanning zero files.
        panic!("guard corpus unreadable {:?}: {}", dir, e)
    });
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_rs_files(&path, hits, needle);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(src) = fs::read_to_string(&path)
            && production_region(&src).contains(needle)
        {
            hits.push(path.display().to_string());
        }
    }
}

#[test]
fn test_no_main_feed_endpoint_connect_path_in_core_src() {
    // The endpoint HOST must stay CONFIG-DRIVEN — never a literal in core.
    //
    // CORRECTED 2026-08-26. This test used to say the lane was retired and
    // that core must therefore not reach the main feed. That reason died
    // with the 2026-08-09 revival: `build_feed_url` in
    // `crates/core/src/websocket/connection.rs` is exactly how the live lane
    // dials it today.
    //
    // The ASSERTION survives the revival intact, because it never really
    // tested retirement — it tests that the host arrives as the `base`
    // PARAMETER rather than being baked into core. That is worth keeping on
    // its own merits: one place to change the host, which is what lets the
    // operator move environments, and it is why the same `build_feed_url`
    // serves the main feed, depth-20 and depth-200 from one code path.
    //
    // Tolerated by construction: `//` comment lines and everything from the
    // in-file `mod tests` down (see `production_region`), and the
    // `api-feed.dhan.co` literal in crates/common, which is
    // doc/sanitize-example text.
    let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    scan_rs_files(&src_root, &mut hits, "api-feed.dhan.co");
    assert!(
        hits.is_empty(),
        "the Dhan main-feed endpoint HOST is hardcoded in crates/core/src \
         production code. It must arrive as the `base` parameter of \
         `build_feed_url` so the host lives in config in ONE place — this is \
         NOT a claim that the lane is retired (it was revived 2026-08-09 and \
         `dhan_enabled = true`). Hits: {hits:?}"
    );
}

#[test]
fn test_websocket_mod_exports_only_surviving_modules() {
    // The websocket mod must not re-declare the deleted lane modules.
    let mod_src = fs::read_to_string(websocket_src_dir().join("mod.rs"))
        .expect("read crates/core/src/websocket/mod.rs");
    // RE-BLESSED 2026-08-11 — see the note on
    // `test_main_feed_ws_modules_stay_deleted`. `pub mod connection;` and
    // `pub mod subscription_builder;` are now REQUIRED declarations, pinned
    // positively by `test_revived_lane_modules_are_present`.
    for banned in [
        "pub mod connection_pool;",
        "pub mod pool_watchdog;",
        "pub mod rate_limit_cooldown;",
    ] {
        assert!(
            !mod_src.contains(banned),
            "websocket/mod.rs re-declares a deleted Dhan live-WS lane module \
             (`{banned}`) — retired PR-C2, 2026-07-13; scope-lock §D."
        );
    }
    // The surviving order-update WS stays exported (functional-dormant, Q4-i).
    assert!(
        mod_src.contains("pub mod order_update_connection;"),
        "the order-update WS module must stay (kept functional-dormant per \
         the Q4-i ruling — it is spawned by dhan_rest_stack)."
    );
}
