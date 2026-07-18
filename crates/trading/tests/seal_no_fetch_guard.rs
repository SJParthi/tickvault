//! Ratchet: the candle seal paths are pure in-RAM — no I/O may ever enter them.
//! Scans the PRODUCTION region (before `#[cfg(test)]`) of the two seal files.

fn production_region(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

/// Strip line + block comments, but treat `://` (inside string literals/URLs) as code.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' && (i == 0 || b[i - 1] != b':') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

const NEEDLES: &[&str] = &[
    "reqwest",
    "SELECT ",
    "select 1",
    "http::",
    "Http",
    "ilp",
    "Sender::from_conf",
    "std::fs",
    "tokio::fs",
    "TcpStream",
    "connect(",
];

fn scan(path: &str) {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let prod = strip_comments(production_region(&src));
    for n in NEEDLES {
        assert!(
            !prod.contains(n),
            "seal-path I/O regression: needle `{n}` found in production region of {path} — \
             the 1m/21-TF seal must stay pure in-RAM"
        );
    }
}

#[test]
fn seal_paths_contain_no_io() {
    // Integration tests run with CWD = the crate directory (crates/trading).
    // 2026-07-17 (stage-3 dead-WS sweep): re-pinned from the DELETED tick
    // aggregator files (aggregator_cell.rs / multi_tf_aggregator.rs) to the
    // SURVIVING pure seal-path modules — the ring, the shared candle state,
    // and the seal-time pct stamping stay in-RAM I/O-free.
    scan("src/candles/seal_ring.rs");
    scan("src/candles/live_candle_state.rs");
    scan("src/candles/pct_stamping.rs");
}

#[test]
fn scanner_is_not_vacuous() {
    // self-test: a poisoned snippet must trip the scan
    let poisoned = "fn f() { let _ = reqwest::Client::new(); }";
    let prod = strip_comments(production_region(poisoned));
    assert!(prod.contains("reqwest"));
    // and comment-stripping must NOT hide code after a URL-like `://`
    let url = "let u = \"http://x\"; reqwest::get(u);";
    assert!(strip_comments(url).contains("reqwest"));
}
