//! Every EMF-selected LOSS series must be seeded, workspace-wide.
//!
//! # Why a second seeding guard exists beside `loss_series_seeding_guard`
//!
//! That guard is exact and narrow: it names two registration functions and
//! asserts what they seed. It is the right shape for the two paths it covers
//! and it cannot see a counter anywhere else in the workspace.
//!
//! On 2026-09-05 a sweep asked the wider question — *which shipped loss series
//! are unseeded ANYWHERE?* — and the answer took five attempts to get right.
//! Each attempt was wrong in the same direction: it reported counters as
//! unseeded that were, in fact, seeded through a mechanism the scan could not
//! follow.
//!
//! | attempt | missed because |
//! |---|---|
//! | 1 | matched `counter!("literal")`; the house declares names as `const` and uses the identifier |
//! | 2 | matched one line; a seed formatted across lines was invisible |
//! | 3 | resolved the const; a handle stored in a struct field is seeded on the FIELD, not at the macro |
//! | 4 | followed the field; an ARRAY of handles is seeded in a loop over an index |
//! | 5 | followed arrays; a per-label seed loop (`for reason in [...]`) sits far from the increment |
//!
//! Thirteen names survived attempt one. Three survived all five. The other ten
//! were already correct, and reporting them would have sent the next reader to
//! rebuild ten things that work — the cost this repository has recorded before
//! under stale rows that manufacture false findings.
//!
//! So the point of this test is not only to catch an unseeded counter. It is
//! to make the RESOLUTION reusable, so the next sweep does not repeat five
//! wrong answers before reaching the true one.
//!
//! # Why seeding matters at all
//!
//! The CloudWatch agent computes a counter as the DELTA between consecutive
//! samples and drops the first sample of a series it has never seen. A counter
//! that is never incremented is therefore never registered, never published
//! and never plotted — and an ABSENT series is indistinguishable from a
//! healthy zero one. A loss counter is rare BY DESIGN, so without a seed its
//! first episode — the one it exists for — publishes nothing.
//!
//! Measured cost of exactly this, 2026-08-28: `tv_depth_rows_dropped_total`
//! read 104,540 while its rescue discriminator had no series at all, so
//! whether those rows were saved or lost is unanswerable for that session and
//! always will be.
//!
//! # What this does NOT claim
//!
//! It is a static scan, not dataflow. It resolves the four shapes the house
//! actually uses; a fifth shape invented later could pass unseen. The failure
//! direction is stated plainly because it is the unusual one: a NEW seeding
//! style produces a FALSE ALARM here, not a silent miss — the test names the
//! counter and a human confirms. That is the safe direction for a guard whose
//! whole subject is things that are invisible when absent.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("cannot canonicalize repo root")
}

/// Words that make a metric a LOSS series — something went missing, was
/// refused, or was given up on. Deliberately broad: a false inclusion costs
/// one seed line, a false exclusion costs an invisible episode.
const LOSS_WORDS: &[&str] = &[
    "dropped",
    "lost",
    "refused",
    "failed",
    "discard",
    "reject",
    "unapplied",
    "unlanded",
    "skipped",
    "exhaust",
    "abandon",
];

/// Every production `.rs` file under `crates/*/src`, with its text.
fn production_sources() -> Vec<(PathBuf, String)> {
    let root = repo_root();
    let mut out = Vec::new();
    let crates = std::fs::read_dir(root.join("crates")).expect("crates/ must be readable");
    for entry in crates.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            collect_rs(&src, &mut out);
        }
    }
    assert!(
        out.len() > 100,
        "found only {} production sources — the walker is broken and every \
         assertion below would pass vacuously",
        out.len()
    );
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // Lossy: one invalid byte must not remove a whole file from the
            // scan. U+FFFD matches nothing, every valid byte around it is
            // still read.
            let bytes = std::fs::read(&path).expect("listed file must be readable");
            out.push((path, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
}

/// The EMF selector's metric names — the ones that actually reach CloudWatch.
/// A counter outside this list publishes nowhere, so seeding it changes
/// nothing and this test has no business demanding it.
fn emf_selected() -> BTreeSet<String> {
    let path = repo_root().join("deploy/aws/cloudwatch-agent.json");
    let body = std::fs::read_to_string(&path).expect("the EMF selector must be readable");
    let mut names = BTreeSet::new();
    for chunk in body.split("tv_").skip(1) {
        let end = chunk
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(chunk.len());
        names.insert(format!("tv_{}", &chunk[..end]));
    }
    assert!(
        names.len() > 50,
        "parsed only {} EMF names — the selector parser is broken",
        names.len()
    );
    names
}

/// `metric name -> constant identifier`, for the house style that declares a
/// name once and uses the identifier everywhere. Attempt 1's blind spot.
fn const_idents(sources: &[(PathBuf, String)]) -> Vec<(String, String)> {
    let mut map = Vec::new();
    for (_, body) in sources {
        for line in body.lines() {
            let line = line.trim();
            let Some(rest) = line
                .strip_prefix("pub const ")
                .or(line.strip_prefix("const "))
            else {
                continue;
            };
            let Some((ident, tail)) = rest.split_once(": &str = \"") else {
                continue;
            };
            let Some((name, _)) = tail.split_once('"') else {
                continue;
            };
            if name.starts_with("tv_") {
                map.push((name.to_owned(), ident.to_owned()));
            }
        }
    }
    map
}

/// Is `metric` seeded anywhere in `body`, under any of the four house shapes?
///
/// `tokens` is every spelling the metric is referenced by: the quoted literal
/// and any constant identifier bound to it.
fn is_seeded_in(body: &str, tokens: &[String]) -> bool {
    for token in tokens {
        for idx in macro_sites(body, token) {
            let tail = &body[idx..];
            // Shape 1 — inline. The seed is on the same statement as the
            // macro. Bounded at the statement end so a LATER `.increment(0)`
            // on an unrelated counter cannot satisfy this one.
            let stmt_end = tail.find(";\n").map_or(tail.len(), |i| i + 1);
            if tail[..stmt_end].contains("increment(0)") {
                return true;
            }
            // Shapes 2-4 — a handle. Walk back to the binding this macro's
            // value is assigned to (`field: counter!(..)`, `let x = ..`) and
            // ask whether that binding is ever seeded in this file. Covers a
            // struct field, an array built by `.map`, and a per-label loop,
            // because all three seed through a NAME rather than at the macro.
            if let Some(binding) = binding_before(&body[..idx]) {
                let direct = format!("{binding}.increment(0)");
                let indexed = format!("{binding}[");
                if body.contains(&direct)
                    || body.match_indices(&indexed).any(|(i, _)| {
                        body[i..]
                            .split_once(".increment(0)")
                            .is_some_and(|(head, _)| !head.contains(';'))
                    })
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Byte offsets of every `counter!(` whose FIRST argument is `token`.
///
/// Written as a search for the token followed by a look-back, rather than a
/// search for `counter!(token`, because the house wraps a labelled macro
/// across lines:
///
/// ```text
/// metrics::counter!(
///     "tv_ticks_lost_total",
///     "ws_type" => t,
/// )
/// ```
///
/// so `counter!("tv_ticks_lost_total"` never appears contiguously. That was
/// the sixth wrong answer, and it produced four FALSE ALARMS on counters that
/// were correctly seeded — the failure this whole file exists to stop
/// repeating.
fn macro_sites(body: &str, token: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (idx, _) in body.match_indices(token) {
        // A constant identifier must not match inside a longer one.
        if !token.starts_with('"') {
            let after_ok = body[idx + token.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            let before_ok = body[..idx]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            if !(after_ok && before_ok) {
                continue;
            }
        }
        let head = body[..idx].trim_end();
        if head.ends_with("counter!(") {
            out.push(head.len() - "counter!(".len());
        }
    }
    out
}

/// The identifier a `counter!` expression is being assigned to, if any:
/// the `x` in `x: metrics::counter!(..)`, `let x = metrics::counter!(..)`,
/// or `let x = WS_TYPES.map(|t| metrics::counter!(..))`.
fn binding_before(head: &str) -> Option<String> {
    // Look back a bounded distance so a binding on an unrelated earlier
    // statement cannot be picked up.
    let window = &head[head.len().saturating_sub(200)..];
    let last_line_start = window.rfind('\n').map_or(0, |i| i + 1);
    // The binding may be on an earlier line when the macro is wrapped, so
    // consider the last two lines.
    let two_lines_start = window[..last_line_start.saturating_sub(1)]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    for candidate in [&window[last_line_start..], &window[two_lines_start..]] {
        let trimmed = candidate.trim_start();
        let body = trimmed.strip_prefix("let ").unwrap_or(trimmed);
        let ident: String = body
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if ident.is_empty() {
            continue;
        }
        let after = body[ident.len()..].trim_start();
        // `::` is a PATH, not a binding. Without this, `metrics::counter!(..)`
        // resolves to a binding named `metrics` — which then "finds" a seed
        // anywhere in the file and passes every counter vacuously.
        if after.starts_with("::") {
            continue;
        }
        if after.starts_with(':') || after.starts_with('=') {
            return Some(ident);
        }
    }
    None
}

#[test]
fn every_emf_selected_loss_series_is_seeded() {
    let sources = production_sources();
    let idents = const_idents(&sources);
    let selected = emf_selected();

    let loss: Vec<&String> = selected
        .iter()
        .filter(|n| LOSS_WORDS.iter().any(|w| n.contains(w)))
        .collect();
    assert!(
        loss.len() >= 30,
        "only {} loss-shaped EMF series found — the filter or the selector \
         parse is broken and this test would pass vacuously",
        loss.len()
    );

    let mut unseeded = Vec::new();
    for metric in loss {
        let mut tokens = vec![format!("\"{metric}\"")];
        for (name, ident) in &idents {
            if name == metric {
                tokens.push(ident.clone());
            }
        }
        // A gauge is published verbatim — there is no delta, so the
        // first-sample rule does not apply and a seed buys nothing.
        let is_gauge = sources.iter().any(|(_, b)| {
            tokens.iter().any(|t| {
                b.contains(&format!("gauge!({t}")) && !b.contains(&format!("counter!({t}"))
            })
        });
        if is_gauge {
            continue;
        }
        if !sources.iter().any(|(_, b)| is_seeded_in(b, &tokens)) {
            unseeded.push(metric.clone());
        }
    }

    assert!(
        unseeded.is_empty(),
        "UNSEEDED LOSS SERIES: {unseeded:?}\n\n\
         Each of these ships to CloudWatch and is incremented only when \
         something is lost. The agent computes a counter as the delta between \
         samples and DROPS the first sample of a series it has never seen, so \
         an unseeded counter publishes nothing on its first episode — the one \
         it exists for.\n\n\
         Add `metrics::counter!(<name>).increment(0);` in the registration \
         function, constructor or loop that owns it, beside its siblings.\n\n\
         If you added a NEW seeding style, this test cannot follow it yet: \
         extend `is_seeded_in`, do not exempt the counter."
    );
}

#[test]
fn the_resolver_follows_all_four_house_shapes() {
    // Every shape below is taken from real production code. A resolver that
    // cannot follow one of them is what produced five wrong answers.
    let literal = r#"metrics::counter!("tv_x_dropped_total").increment(0);"#;
    assert!(
        is_seeded_in(literal, &[String::from("\"tv_x_dropped_total\"")]),
        "shape 1 (inline literal) must resolve"
    );

    let via_const = "metrics::counter!(DROP_METRIC, \"feed\" => f).increment(0);";
    assert!(
        is_seeded_in(via_const, &[String::from("DROP_METRIC")]),
        "shape 2 (constant identifier) must resolve — attempt 1's blind spot"
    );

    let field = "\
        Self {\n\
            wal_dropped: metrics::counter!(WAL_DROP_METRIC, \"endpoint\" => e),\n\
        }\n\
        fn pre_register(&self) {\n\
            self.wal_dropped.increment(0);\n\
        }";
    assert!(
        is_seeded_in(field, &[String::from("WAL_DROP_METRIC")]),
        "shape 3 (struct field seeded elsewhere) must resolve — attempt 3's blind spot"
    );

    let array = "\
        let ticks_lost_channel_full = WS_TYPES_BY_INDEX.map(|t| {\n\
            metrics::counter!(\"tv_ticks_lost_total\", \"ws_type\" => t)\n\
        });\n\
        for idx in 0..N {\n\
            counters.ticks_lost_channel_full[idx].increment(0);\n\
        }";
    assert!(
        is_seeded_in(array, &[String::from("\"tv_ticks_lost_total\"")]),
        "shape 4 (array of handles seeded in a loop) must resolve — attempt 4's blind spot"
    );

    // And the negative: a lone increment is NOT a seed. Without this the
    // whole test passes on any file that mentions the counter at all.
    let unseeded = r#"metrics::counter!("tv_y_dropped_total").increment(1);"#;
    assert!(
        !is_seeded_in(unseeded, &[String::from("\"tv_y_dropped_total\"")]),
        "an increment(1) must NEVER be mistaken for a seed"
    );

    // A seed on a DIFFERENT counter later in the file must not satisfy this
    // one — the statement bound is what stops that.
    let neighbour = "\
        metrics::counter!(\"tv_z_dropped_total\").increment(1);\n\
        metrics::counter!(\"tv_other_total\").increment(0);";
    assert!(
        !is_seeded_in(neighbour, &[String::from("\"tv_z_dropped_total\"")]),
        "a neighbouring counter's seed must not satisfy an unseeded one"
    );
}
