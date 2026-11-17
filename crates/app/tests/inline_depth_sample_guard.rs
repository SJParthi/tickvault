//! Ratchet: the live drain times its inline depth append through the sampler,
//! and nothing else sits inside that clock (2026-11-17, plan ITEM DT "PR 0,
//! measure first").
//!
//! The `drain_inline_depth` stage is meant to read the depth share of a Full
//! tick. If the sampler's closure gained other work, or the append moved out
//! of it, the stage would silently read something else (or nothing) while
//! still printing a number. This scan fails on either.
//!
//! The behaviour half is
//! `dhan_feed_stack::inline_depth_sample::tests::test_drain_samples_one_in_64_full_packets_with_inline_depth`
//! (the live drain counts every append and the stage gains its samples) and
//! the two Full gates in `dhat_live_ingest_seam.rs` (exactly 149 sampled
//! packets inside each zero-allocation window).
//!
//! HONEST LIMIT: a text scan of one function. It proves the shape of the
//! call, not its cost.

use std::path::PathBuf;

const SAMPLED_CALL: &str = "ingest.inline_depth_sample.time(|| {";

fn source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The production text of `drain_main_feed_frame`, up to the next top-level
/// function.
fn drain_body(src: &str) -> Option<&str> {
    let prod = src.split(concat!("#[cfg(", "test)]")).next()?;
    let start = prod.find("pub fn drain_main_feed_frame(")?;
    let rest = &prod[start..];
    let end = rest[1..].find("\nfn ").map_or(rest.len(), |i| i + 1);
    let end = rest[1..].find("\npub fn ").map_or(end, |i| end.min(i + 1));
    Some(&rest[..end])
}

/// The closure body handed to the sampler: the text between the opening
/// `{` of `SAMPLED_CALL` and its matching `}`.
fn sampled_closure(body: &str) -> Option<&str> {
    let at = body.find(SAMPLED_CALL)? + SAMPLED_CALL.len();
    let inner = &body[at..];
    let mut depth = 1usize;
    for (i, ch) in inner.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&inner[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Problems with the drain's inline depth timing, empty when it is right.
fn problems(src: &str) -> Vec<String> {
    let Some(body) = drain_body(src) else {
        return vec!["drain_main_feed_frame not found".to_string()];
    };
    let mut out = Vec::new();
    if body.matches(SAMPLED_CALL).count() != 1 {
        out.push(format!(
            "expected exactly one `{SAMPLED_CALL}` in the drain"
        ));
    }
    if body.matches("append_inline_depth(").count() != 1 {
        out.push("expected exactly one append_inline_depth call in the drain".to_string());
    }
    match sampled_closure(body) {
        None => out.push("the sampler's closure was not found".to_string()),
        Some(closure) => {
            let code = closure.trim();
            if !code.starts_with("append_inline_depth(") {
                out.push(format!(
                    "the sampler's closure must hold only the append, found `{code}`"
                ));
            }
            if code.matches(';').count() != 0 {
                out.push("the sampler's closure must be one expression".to_string());
            }
        }
    }
    out
}

#[test]
fn drain_times_only_the_inline_depth_append_through_the_sampler() {
    let src = source();
    let found = problems(&src);
    assert!(found.is_empty(), "drain_inline_depth wiring: {found:?}");
}

#[test]
fn guard_bites_when_the_clock_holds_more_than_the_append() {
    let src = source();
    assert!(problems(&src).is_empty());
    let widened = src.replacen(
        "ingest.inline_depth_sample.time(|| {\n",
        "ingest.inline_depth_sample.time(|| {\n                            out.folded += 0;\n",
        1,
    );
    assert!(
        !problems(&widened).is_empty(),
        "extra work inside the clock"
    );
    let unsampled = src.replacen(SAMPLED_CALL, "(|| {", 1);
    assert!(
        !problems(&unsampled).is_empty(),
        "append outside the sampler"
    );
}
