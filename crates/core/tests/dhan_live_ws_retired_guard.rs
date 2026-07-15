//! Source-scan ratchet: the Dhan live main-feed WS stays DELETED (PR-C2).
//!
//! Operator retirement directive 2026-07-13 (verbatim + full contract:
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"): the Dhan
//! live main-feed WebSocket lane is RETIRED — re-introducing ANY Dhan
//! market-data WS requires a fresh dated operator quote in that rule file
//! FIRST (§D). These tests fail the build if the deleted machinery
//! reappears in `crates/core/`.
//!
//! What survives (deliberately NOT pinned as absent): the functional-dormant
//! ORDER-UPDATE WS (`order_update_connection.rs`, spawned by
//! `dhan_rest_stack` per the Q4-i "agreed dude" ruling) and the shared
//! plumbing it uses (`types` / `activity_watchdog` / `market_hours_gate` /
//! `tls`).

use std::fs;
use std::path::{Path, PathBuf};

fn websocket_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/websocket")
}

#[test]
fn test_main_feed_ws_modules_stay_deleted() {
    // The five lane modules deleted by PR-C2. Re-creating any of them is a
    // scope-lock violation without a fresh dated operator quote.
    for deleted in [
        "connection.rs",
        "connection_pool.rs",
        "subscription_builder.rs",
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
/// down (test fixtures + doc comments may cite the retired endpoint as an
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
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
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
    // No PRODUCTION code under crates/core/src may carry the main-feed WS
    // endpoint — the ONLY Dhan WS endpoint core may connect to is the
    // order-update one. (Doc comments + in-file test fixtures citing the
    // retired endpoint as an example are tolerated; the `api-feed.dhan.co`
    // literal in crates/common is doc/sanitize-example text.)
    let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    scan_rs_files(&src_root, &mut hits, "api-feed.dhan.co");
    assert!(
        hits.is_empty(),
        "the Dhan main-feed WS endpoint reappeared in crates/core/src \
         production code — the live-WS lane is retired (PR-C2, 2026-07-13; \
         scope-lock §D). Hits: {hits:?}"
    );
}

#[test]
fn test_websocket_mod_exports_only_surviving_modules() {
    // The websocket mod must not re-declare the deleted lane modules.
    let mod_src = fs::read_to_string(websocket_src_dir().join("mod.rs"))
        .expect("read crates/core/src/websocket/mod.rs");
    for banned in [
        "pub mod connection;",
        "pub mod connection_pool;",
        "pub mod subscription_builder;",
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
