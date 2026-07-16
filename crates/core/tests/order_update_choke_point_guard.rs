//! Build-failing ratchets for the order-update decode choke point (Cov#1/#2).
//!
//! Every production order-update frame MUST decode through
//! `parser::order_update::parse_order_update` — the single site that applies
//! the dual-casing normalizer, the null-merge semantics, the AlgoOrdNo
//! coercion, the frame-size cap, and the quirk counter. A direct
//! `serde_json::from_str::<OrderUpdateMessage>` elsewhere would silently
//! bypass all of that. Source-scan pattern per `oms_reconcile_wiring_guard.rs`
//! (read + needle scan; scanned files are READ-ONLY here).

use std::fs;
use std::path::{Path, PathBuf};

/// Bounds a source string to its production region (everything before the
/// first `#[cfg(test)]` marker), so test code can never satisfy or trip a pin.
fn production_region(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(pos) => &src[..pos],
        None => src,
    }
}

/// Bypass needles: any of these outside the parser module is a direct decode
/// that skips the normalizer/cap/counter choke point.
///
/// Refuter-A M2 (2026-07-14): the `::<OrderUpdate` prefix (no closing `>`)
/// covers BOTH `OrderUpdate` and `OrderUpdateMessage` turbofish targets
/// across `from_str`/`from_slice`/`from_value`/`from_reader`, and the two
/// inference-form needles catch typed-binding decodes with no turbofish.
/// The four fn-qualified prefixes are used (not a bare `::<OrderUpdate`)
/// so legitimate non-decode turbofish like `channel::<OrderUpdate>` can
/// never false-positive.
const BYPASS_NEEDLES: &[&str] = &[
    "from_str::<OrderUpdate",
    "from_slice::<OrderUpdate",
    "from_value::<OrderUpdate",
    "from_reader::<OrderUpdate",
    // from_reader's turbofish carries the reader type first — cover the
    // realistic `from_reader::<_, OrderUpdate…>` form explicitly.
    "from_reader::<_, OrderUpdate",
    ": OrderUpdate = serde_json::from_",
    ": OrderUpdateMessage = serde_json::from_",
];

/// Returns the bypass needles present in a production-region source string.
fn find_bypass_violations(prod: &str) -> Vec<&'static str> {
    BYPASS_NEEDLES
        .iter()
        .copied()
        .filter(|needle| prod.contains(needle))
        .collect()
}

/// Recursively collects every `.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The two scanned production source roots: this crate + the app crate.
fn scanned_roots() -> Vec<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    vec![manifest.join("src"), manifest.join("../app/src")]
}

// Cov#1 — exactly the one known production caller uses the choke point, and
// no production file outside the parser decodes OrderUpdateMessage directly.
//
// 2026-07-15 merge note: this pin was "exactly two callers
// (order_update_connection.rs + boot_helpers.rs)" when authored. Main's
// order-update spawn retirement (`websocket-connection-scope-lock.md` §A.1 /
// the Phase C-2 lane deletion) removed boot_helpers.rs's WAL-replay decode
// site, so the post-merge truth is exactly ONE caller —
// order_update_connection.rs (the retained-dormant module). Re-adding a
// second caller must go through this pin deliberately.
#[test]
fn ratchet_parse_order_update_has_exactly_one_production_caller() {
    let mut files = Vec::new();
    for root in scanned_roots() {
        collect_rs_files(&root, &mut files);
    }
    assert!(
        files.len() > 50,
        "scanner must actually walk the source tree"
    );

    let mut callers: Vec<String> = Vec::new();
    for path in &files {
        let src =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let prod = production_region(&src);
        let path_str = path.to_string_lossy().replace('\\', "/");
        let is_parser_module = path_str.ends_with("parser/order_update.rs");

        if !is_parser_module && prod.contains("parse_order_update(") {
            callers.push(path_str.clone());
        }
        if !is_parser_module {
            let violations = find_bypass_violations(prod);
            assert!(
                violations.is_empty(),
                "{path_str} decodes OrderUpdateMessage directly ({violations:?}) — \
                 all production order-update decode MUST go through \
                 parser::order_update::parse_order_update (the choke point \
                 applying the normalizer, frame cap, and quirk counter)"
            );
        }
    }

    callers.sort();
    assert_eq!(
        callers.len(),
        1,
        "expected exactly 1 production caller of parse_order_update \
         (order_update_connection.rs — boot_helpers.rs's WAL-replay decode \
         site retired with the order-update spawn, 2026-07-15), found: {callers:?}"
    );
    assert!(
        callers[0].ends_with("websocket/order_update_connection.rs"),
        "the sole caller must be order_update_connection.rs, found: {callers:?}"
    );
}

// Cov#2 — the decode-quirk counter literal stays wired in the parser's
// production region (the observability half of the choke-point contract).
#[test]
fn ratchet_decode_quirk_counter_exists_in_parser() {
    let parser = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/parser/order_update.rs");
    let src =
        fs::read_to_string(&parser).unwrap_or_else(|e| panic!("read {}: {e}", parser.display()));
    let prod = production_region(&src);
    assert!(
        prod.contains("tv_order_update_decode_quirks_total"),
        "the parser must keep the tv_order_update_decode_quirks_total \
         quirk counter (decode-drift observability)"
    );
}

// Self-test: the bypass scanner cannot regress to a vacuous pass — one
// planted violation per needle form (Refuter-A M2 evasion coverage).
#[test]
fn bypass_scanner_detects_planted_violation() {
    let planted = "let msg = serde_json::from_str::<OrderUpdateMessage>(payload)?;\nlet other = 1;";
    assert_eq!(
        find_bypass_violations(planted),
        vec!["from_str::<OrderUpdate"]
    );
    let planted_slice = "let msg = serde_json::from_slice::<OrderUpdateMessage>(bytes)?;";
    assert_eq!(
        find_bypass_violations(planted_slice),
        vec!["from_slice::<OrderUpdate"]
    );
    let planted_value = "let msg = serde_json::from_value::<OrderUpdate>(v)?;";
    assert_eq!(
        find_bypass_violations(planted_value),
        vec!["from_value::<OrderUpdate"]
    );
    let planted_reader = "let msg = serde_json::from_reader::<_, OrderUpdate>(r)?;";
    assert_eq!(
        find_bypass_violations(planted_reader),
        vec!["from_reader::<_, OrderUpdate"]
    );
    let planted_reader_direct = "let msg = serde_json::from_reader::<OrderUpdateMessage>(r)?;";
    assert_eq!(
        find_bypass_violations(planted_reader_direct),
        vec!["from_reader::<OrderUpdate"]
    );
    let planted_binding = "let m: OrderUpdateMessage = serde_json::from_value(v)?;";
    assert_eq!(
        find_bypass_violations(planted_binding),
        vec![": OrderUpdateMessage = serde_json::from_"]
    );
    let planted_inference = "let m: OrderUpdate = serde_json::from_str(s)?;";
    assert_eq!(
        find_bypass_violations(planted_inference),
        vec![": OrderUpdate = serde_json::from_"]
    );
    assert!(find_bypass_violations("fn clean() {}").is_empty());
    // Legitimate non-decode turbofish must never false-positive.
    let legit = "let (tx, rx) = broadcast::channel::<OrderUpdate>(16);";
    assert!(find_bypass_violations(legit).is_empty());
    // The test-region cut keeps test code out of scope.
    let with_tests = "prod();\n#[cfg(test)]\nmod t { from_str::<OrderUpdateMessage> }";
    assert!(find_bypass_violations(production_region(with_tests)).is_empty());
}
