//! RETIRED GUARD — NTM constituency downloader hardening ratchet.
//!
//! **RETIRED in PR-C3 (2026-07-14).** The `index_constituency` module this
//! guard read as text (`src/instrument/index_constituency/downloader.rs`)
//! was DELETED with the Dhan instrument-download chain per the operator's
//! 2026-07-13 retirement directive (Q3 — "hereafter no Dhan instrument
//! download/parsing"; `websocket-connection-scope-lock.md` "2026-07-13
//! Amendment" §B item 3). The niftyindices NTM list is now consumed ONLY
//! by the Groww watch build through its own hardened client
//! (`crates/core/src/feed/groww/instruments.rs` — redirect-none,
//! HTTPS-only, timeouts, body cap; that client carries its own unit
//! coverage), so the §18-hardening surface this guard pinned no longer
//! exists in this repo.
//!
//! Retained as a dated tombstone per the C2 guard-retirement precedent
//! (retire loudly with the authority cited, never delete silently). The
//! single test below pins the RETIREMENT: the deleted module must STAY
//! deleted.

use std::path::PathBuf;

/// PR-C3 tombstone: the index_constituency module must stay deleted.
/// Re-introducing a Dhan-side constituency downloader requires a fresh
/// dated operator quote in
/// `.claude/rules/project/websocket-connection-scope-lock.md` FIRST.
#[test]
fn index_constituency_module_stays_deleted() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/instrument/index_constituency");
    assert!(
        !path.exists(),
        "PR-C3 retirement violated: crates/core/src/instrument/\
         index_constituency/ must stay DELETED (operator Q3, 2026-07-13)."
    );
}
