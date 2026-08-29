//! Every loss counter must be REACHABLE by a human, or be listed here.
//!
//! # The defect class this closes
//!
//! A counter that records data loss and reaches no operator surface is worse
//! than no counter at all: the loss is measured, the measurement is discarded,
//! and every dashboard stays green. This session alone found four instances of
//! exactly that shape, each discovered by accident rather than by a gate:
//!
//! | Counter | How it was invisible |
//! |---|---|
//! | `tv_mark_forward_dropped_total` | Prometheus-only; nothing on the box scrapes `/metrics` |
//! | `tv_ws_frame_spill_write_errors_total` | excluded from EMF on a premise that stopped being true when the live lane was revived |
//! | `tv_ticks_dropped_total` | lane instrumented at the socket, blind at the database |
//! | `tv_dhan_ws_ring_bytes_full_total` | emitted by the binary, selected by nothing |
//!
//! Four of the same bug, found four separate ways, none of them by a test.
//! That is what this file is for.
//!
//! # The rule
//!
//! A counter whose name says it counts LOSS (`*_dropped_total`,
//! `*_refused_total`, `*_errors_total`, `*_discarded_total`, `*_lost_total`,
//! `*_failed_total`) must be at least one of:
//!
//! 1. **SHIPPED** — present in the EMF `metric_selectors` allowlist, so it
//!    exists in CloudWatch and can carry an alarm; or
//! 2. **LOGGED** — accompanied by an `error!` or `warn!` within a few lines of
//!    its emit site, so it reaches `errors.jsonl` → CloudWatch Logs and is
//!    triageable even without a metric; or
//! 3. **ALLOWLISTED** here, with the reason recorded in the table below.
//!
//! # Why this is a shrinking allowlist and not a hard zero
//!
//! There are 90 loss-shaped counters and CloudWatch bills ~$0.30 per custom
//! metric per month. Shipping all of them would cost ~$27/mo against a $100
//! kill-ceiling whose AUTOMATIC budget actions STOP the trading box at 90% —
//! so a blanket "ship everything" rule would trade one silent failure for a
//! different one. The allowlist makes the trade-off VISIBLE and DELIBERATE
//! instead of accidental, and it can only shrink.
//!
//! # What this guard does NOT claim
//!
//! Being shipped or logged is not the same as PAGING. A metric with no alarm
//! and a log line with no filter both reach a surface a human must go and
//! look at. This guard enforces reachability, not alerting. Alarms are a
//! per-signal cost decision made in `error-code-alarms.tf` and the dated
//! COST NOTEs in `aws-budget.md`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Suffixes that make a counter name a claim about LOSS.
const LOSS_SUFFIXES: &[&str] = &[
    "dropped",
    // "drop" (singular) is NOT a synonym-for-completeness — it is a real
    // blind-spot fix found 2026-08-14. `tv_ws_frame_spill_drop_critical` is the
    // WAL durable-floor breach counter, arguably the most critical loss series
    // in the workspace, and its second-to-last segment is "drop", not
    // "dropped". Without this entry `is_loss_shaped` returned false for it, so
    // the guard that exists to make loss visible could not see the one counter
    // that reports the durable floor being breached.
    "drop",
    "refused",
    "errors",
    "discarded",
    "lost",
    "failed",
];

/// Lines after a counter emit within which a log macro counts as "logged".
///
/// Emit sites in this workspace put the counter and its log line adjacent,
/// usually counter-then-log or log-then-counter with a few argument lines
/// between. 12 lines forward and 4 back covers the observed shapes without
/// reaching into an unrelated neighbouring statement.
const LOG_LOOKAHEAD_LINES: usize = 12;

/// Hard ceiling on the backward walk. The real stop is the enclosing block
/// boundary (see `logged_near`); this only bounds the search.
///
/// 24, not 4. The house pattern for a loss site is one `error!` describing
/// what was lost, followed by several `metrics::counter!` calls attributing
/// it — and those `error!` blocks are multi-line, with a field per line. In
/// `ws_frame_spill` the `error!` sits **17 lines** above `tv_ticks_lost_total`
/// inside the same `TrySendError::Full` arm. A 4-line window called that
/// counter unreachable when it is in fact one of the loudest sites in the
/// workspace, which is how a false gap gets manufactured.
///
/// Widening alone was NOT enough and briefly made things worse: at 24 lines
/// with no block check, a planted counter picked up an `error!` from the
/// PREVIOUS function and the guard stopped biting. A gate that cannot fail is
/// decoration. Hence the indentation-aware stop in `logged_near`.
const LOG_LOOKBEHIND_LINES: usize = 24;

/// Counters that are neither shipped nor logged, each with the reason it is
/// tolerated. **This list may only SHRINK.**
///
/// Adding a row is a deliberate act that needs a reason a reviewer can check.
/// Removing one means the counter became reachable — the direction of travel.
const UNREACHABLE_ALLOWLIST: &[(&str, &str)] = &[
    // Kept in ONE sorted block so diffs stay readable and the sorted-ness is
    // machine-checked. The reason string carries the classification:
    //
    //   "LIVE PATH"  — sits on the path carrying market data today. Triage
    //                  these first; there are five.
    //   "groww_orders feature" — compiled out of the deploy build (Gate 2 of
    //                  the §39.2 live-fire lattice), so it cannot emit at all.
    //   "heartbeat"  — genuinely reachable, just not lexically near the emit,
    //                  which is the one thing this scanner cannot see.
    //   "untriaged"  — found by the FIRST run of this guard and NOT yet looked
    //                  at. The reason says exactly that instead of inventing a
    //                  per-counter justification nobody verified: a
    //                  plausible-sounding wrong reason is worse than an honest
    //                  "not yet examined", because it stops the next person
    //                  from examining it.
    // REMOVED 2026-08-28: `tv_aggregator_tick_refused_total` is now in the EMF
    // selector and charted, so the "a periodic report is good enough" exemption
    // no longer applies. The exemption was written when the counter was
    // unreachable from CloudWatch; it stayed while the counter recorded a
    // 2.4%-of-every-tick hard refusal that nobody could see. It ships now.
    (
        "tv_chain1m_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    // REMOVED 2026-08-14: `tv_dhan_feed_ingest_refused_total` is now in the EMF
    // selector, so the "periodic report is good enough" exemption no longer
    // applies — it ships and is checked like any other loss counter.
    (
        "tv_dhan_depth_universe_failed_total",
        "logged — VERIFIED 2026-08-21: the emit lives in the shared helper record_depth_failure(), and every one of its SIX call sites in dhan_depth_universe.rs places it immediately before a tracing::error! carrying code=WS-GAP-02. The log therefore reaches errors.jsonl on every increment; it is simply one function away from the counter, which is the one thing this scanner cannot see. Pinned in-crate by failure_metric_tests::every_depth_failure_log_carries_an_error_code, which fails the build if any arm loses its code field. EMF-shipping it remains available as a ~$0.30/mo decision; the cheaper path is an errcode log-filter alarm on WS-GAP-02, which the code field above already makes possible and which needs a dated row in dhan-rest-only-noise-lock-2026-07-14.md first.",
    ),
    (
        "tv_dhan_feed_ingest_seq_refused_total",
        "logged — the emit is counters().ingest_seq_refused, three levels of indirection from the literal (const -> struct field -> method), and the error! sits directly beside it; the scanner cannot follow that chain (verified 2026-08-12)",
    ),
    (
        "tv_dhan_feed_seals_dropped",
        "gauge twin — VERIFIED 2026-08-12: this is the session GAUGE (SEALS_DROPPED_GAUGE, set once per drain in run_frame_drain). Its COUNTER twin tv_dhan_feed_seals_dropped_total IS in the EMF selector, so the loss itself is shipped and alarmable; the gauge is the same number in instantaneous form and shipping both would double-bill one signal.",
    ),
    (
        "tv_dhan_live_xverify_audit_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    // REMOVED 2026-08-14: `tv_dhan_ws_dial_failed_total` is now in the EMF
    // selector. This is the counter that would have made the 2026-08-12
    // blackout visible — 12 consecutive HTTP 400 dial failures that reached
    // only the log sink. "Logged" was never an adequate exemption for it.
    (
        "tv_groww_chain1m_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    (
        "tv_groww_contract1m_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    (
        "tv_groww_spot1m_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    (
        "tv_mark_forward_dropped_total",
        "heartbeat — reported by the order-runtime reconcile heartbeat, not at the emit site (the DHAT budget there forbids a log line)",
    ),
    (
        "tv_order_leg_pnl_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    (
        "tv_partition_dropped_total",
        "NOT A LOSS COUNTER — my own 2026-08-12 label of VERIFIED SILENT was WRONG. It fires on the Ok(()) arm of a SUCCESSFUL retention drop and is immediately followed by append_audit(ArchiveOutcome::Dropped), which writes a QuestDB audit row — a stronger operator surface than a log. The name matches the loss classifier only because dropped sits in the tail, the same false positive as tv_errors_summary_refresh_total.",
    ),
    (
        "tv_pnl_audit_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    (
        "tv_spot1m_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
    (
        "tv_tf_verify_audit_rows_discarded_total",
        "poisoned-buffer discard — the counter lives in discard_pending(); every caller is a flush arm that surfaces the returned count one function away, via error!, bail!, or a propagated Err with the count in its .context(). All 11 of this family verified 2026-08-12; the Err-context arms were found by spot-check after the first wording claimed only error!-or-bail!",
    ),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/common")
        .to_path_buf()
}

/// Every `.rs` file under `crates/*/src`, excluding test modules' own files.
fn production_rs_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let crates_dir = repo_root().join("crates");
    let Ok(entries) = fs::read_dir(&crates_dir) else {
        return out;
    };
    for krate in entries.flatten() {
        let src = krate.path().join("src");
        if src.is_dir() {
            collect_rs(&src, &mut out);
        }
    }
    out.sort();
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// A counter name is loss-shaped when a `LOSS_SUFFIXES` word appears in it.
/// The loss word must be the LAST or SECOND-TO-LAST underscore segment.
///
/// A plain `contains` over-matches badly: `tv_errors_summary_refresh_total`
/// is a SUCCESS counter about the errors-summary writer, and
/// `tv_errors_summary_refresh_failed_total` is the loss twin of it. Only the
/// second is a claim about loss, and the difference is purely positional —
/// which is why this checks the tail rather than the whole string.
fn is_loss_shaped(name: &str) -> bool {
    let segs: Vec<&str> = name.split('_').collect();
    let tail = segs.len().saturating_sub(2);
    segs.iter().skip(tail).any(|s| LOSS_SUFFIXES.contains(s))
}

/// Extract `tv_*` identifiers from one line.
///
/// Walks `char`s rather than byte indices on purpose. These files contain
/// em-dashes in prose and in HTML titles, and a byte-index scan panics the
/// moment it slices mid-character — which is exactly how the first version of
/// this function died, on `<title>TickVault — Live Board</title>`. At this
/// scale the char walk is free and cannot panic.
fn tv_names_in(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let is_start = chars[i] == 't'
            && chars.get(i + 1) == Some(&'v')
            && chars.get(i + 2) == Some(&'_')
            // Not the tail of a longer identifier (e.g. `my_tv_thing`).
            && (i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_'));
        if !is_start {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        out.push(chars[start..end].iter().collect::<String>());
        i = end;
    }
    out
}

/// True when the file body is inside a `#[cfg(test)]` module at `line_idx`.
///
/// Cheap approximation, deliberately: the first `#[cfg(test)]` at column 0 in
/// these files begins the trailing test module and nothing production follows
/// it. Used only to avoid counting test-only counters.
fn test_module_start(lines: &[&str]) -> usize {
    lines
        .iter()
        .position(|l| l.trim_start() == "#[cfg(test)]" && l.starts_with("#[cfg(test)]"))
        .unwrap_or(lines.len())
}

/// One loss counter and how it reaches (or fails to reach) an operator.
struct Emit {
    name: String,
    logged: bool,
}

/// `const FOO: &str = "tv_bar_dropped_total";` — the indirection that made the
/// first version of this scanner blind.
///
/// Roughly half the counters in this workspace are declared as a named const
/// and emitted as `counter!(FOO)`. The literal then appears ONLY on the
/// declaration line, usually hundreds of lines away from any emit, so
/// checking log-proximity at the literal checks the wrong place entirely and
/// reports a well-logged counter as unreachable. Resolving the alias is what
/// makes the proximity check mean anything for those.
fn const_aliases(lines: &[&str], cutoff: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in lines.iter().take(cutoff) {
        let t = line.trim_start();

        // Local counter HANDLES: `let m_probe_failed = metrics::counter!("tv_x");`
        // then `m_probe_failed.increment(1)` somewhere else in the loop.
        //
        // This is the second indirection shape in the workspace and it hides
        // counters exactly as thoroughly as the const one: the literal appears
        // once at handle creation, and every real emit is a method call on the
        // binding. `resource_monitor` logs at ALL THREE of its probe-failure
        // arms and still looked unreachable, because the name is 40+ lines
        // above them at the `let`.
        if !t.starts_with("//")
            && t.starts_with("let ")
            && t.contains("counter!(")
            && let Some(ident) = t
                .strip_prefix("let ")
                .and_then(|r| r.split('=').next())
                .map(|s| s.trim().trim_start_matches("mut ").trim())
        {
            for name in tv_names_in(line) {
                if is_loss_shaped(&name) && !ident.is_empty() {
                    out.push((ident.to_string(), name));
                }
            }
        }

        if t.starts_with("//") || !t.contains("const ") || !t.contains(": &str") {
            continue;
        }
        let Some(after_const) = t.split("const ").nth(1) else {
            continue;
        };
        let Some(ident) = after_const.split(':').next() else {
            continue;
        };
        let ident = ident.trim();
        for name in tv_names_in(line) {
            if is_loss_shaped(&name) && !ident.is_empty() {
                out.push((ident.to_string(), name));
            }
        }
    }
    out
}

fn scan_emits() -> Vec<Emit> {
    let mut found: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
    // A counter emitted from several sites is reachable if ANY site reaches a
    // human — the operator only needs one way in. Written as a fn taking the
    // map rather than a closure capturing it, so the declaration pass below
    // can also touch the map without a double-borrow.
    fn note(found: &mut std::collections::BTreeMap<String, bool>, name: String, logged: bool) {
        found
            .entry(name)
            .and_modify(|e| *e = *e || logged)
            .or_insert(logged);
    }

    for path in production_rs_files() {
        let Ok(src) = fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = src.lines().collect();
        let cutoff = test_module_start(&lines);
        let aliases = const_aliases(&lines, cutoff);

        // Proximity ALONE is not enough in either direction, and both failure
        // modes were observed on this codebase:
        //
        //   too narrow (4 lines back) — missed the `error!` sitting 17 lines
        //     above `tv_ticks_lost_total` in the same match arm, and reported
        //     one of the loudest sites in the workspace as unreachable.
        //   too wide (24 lines back, unbounded) — a planted counter at the top
        //     of a file picked up an `error!` from the PREVIOUS function and
        //     passed. The guard stopped biting, which is worse: a gate that
        //     cannot fail is decoration.
        //
        // So the backward walk stops at the enclosing block boundary: a `}`
        // indented LESS than the counter line closes a scope the counter is
        // not in, and anything above it belongs to different code. That keeps
        // the same-arm `error!` and rejects the previous function's.
        let indent_of = |l: &str| l.len() - l.trim_start().len();
        let logged_near = |idx: usize| -> bool {
            let here = indent_of(lines[idx]);
            let floor = idx.saturating_sub(LOG_LOOKBEHIND_LINES);
            let mut lo = idx;
            while lo > floor {
                let prev = lo - 1;
                let t = lines[prev].trim_start();
                let ind = indent_of(lines[prev]);
                // Closing brace of a shallower scope: everything above belongs
                // to code this counter is not in.
                if t.starts_with('}') && ind < here {
                    break;
                }
                // A top-level item boundary also stops the walk. Without this
                // a counter at indent 0 — a one-liner `fn f() { counter!(..) }`
                // — has NO shallower brace to find, so the walk ran the full
                // window and adopted the previous function's `error!`. Found by
                // the bite test: the planted violation passed, which means the
                // guard had quietly stopped guarding.
                if ind == 0
                    && (t.starts_with('}')
                        || t.starts_with("fn ")
                        || t.starts_with("pub fn ")
                        || t.starts_with("impl ")
                        || t.starts_with("pub struct ")
                        || t.starts_with("struct "))
                {
                    break;
                }
                lo = prev;
            }
            let hi = (idx + LOG_LOOKAHEAD_LINES).min(cutoff);
            lines[lo..hi]
                .iter()
                .any(|l| l.contains("error!") || l.contains("warn!"))
        };

        for (idx, line) in lines.iter().enumerate().take(cutoff) {
            // Comments describe counters constantly in this codebase; only
            // real code counts as an emit site.
            if line.trim_start().starts_with("//") {
                continue;
            }

            // Direct literal use.
            for name in tv_names_in(line) {
                if !is_loss_shaped(&name) {
                    continue;
                }
                // A const DECLARATION is not an emit site — judging proximity
                // there is what produced the false "unreachable" verdicts. Its
                // real sites are handled by the alias pass below.
                let is_decl = aliases.iter().any(|(_, n)| *n == name)
                    && line.contains("const ")
                    && line.contains(": &str");
                if is_decl {
                    // Register it so it is never silently dropped, but let the
                    // alias pass decide reachability.
                    found.entry(name).or_insert(false);
                    continue;
                }
                note(&mut found, name, logged_near(idx));
            }

            // Aliased use: `counter!(INGEST_REFUSED_COUNTER, ...)`.
            for (ident, name) in &aliases {
                if line.contains(ident.as_str()) && !line.contains(": &str") {
                    note(&mut found, name.clone(), logged_near(idx));
                }
            }
        }
    }
    found
        .into_iter()
        .map(|(name, logged)| Emit { name, logged })
        .collect()
}

/// Names in the deployed EMF `metric_selectors` allowlist.
fn shipped_names() -> BTreeSet<String> {
    // The deployed agent config. Was embedded in user-data.sh.tftpl until
    // 2026-08-25; that ~1.6 KB duplicate is gone and the template copies this
    // file into place after the Step 5 clone.
    let ud = repo_root().join("deploy/aws/cloudwatch-agent.json");
    let src = fs::read_to_string(&ud).expect("user-data template must be readable");
    src.lines()
        .filter(|l| l.contains("metric_selectors"))
        .flat_map(tv_names_in)
        .collect()
}

#[test]
fn every_loss_counter_is_shipped_logged_or_allowlisted() {
    let shipped = shipped_names();
    let allow: BTreeSet<&str> = UNREACHABLE_ALLOWLIST.iter().map(|(n, _)| *n).collect();

    let mut unreachable: Vec<String> = Vec::new();
    for emit in scan_emits() {
        if shipped.contains(&emit.name) || emit.logged || allow.contains(emit.name.as_str()) {
            continue;
        }
        unreachable.push(emit.name);
    }
    unreachable.sort();

    assert!(
        unreachable.is_empty(),
        "{} loss counter(s) reach NO operator surface — not in the EMF \
         allowlist, no error!/warn! near any emit site, and not allowlisted:\n  {}\n\n\
         A counter that measures data loss and reaches nobody is worse than no \
         counter: the loss is measured, the measurement is discarded, and the \
         dashboard stays green. Fix by ONE of:\n\
         (a) add the name to metric_selectors in deploy/aws/cloudwatch-agent.json and \
         cloudwatch-agent.json, bump the count ratchet, and add a dated COST \
         NOTE (~$0.30/mo each — the budget kill-ceiling STOPS the box at 90%, \
         so this is a real decision, not a formality);\n\
         (b) add an error!/warn! at the emit site so it reaches errors.jsonl;\n\
         (c) add it to UNREACHABLE_ALLOWLIST with the reason it cannot emit \
         (compiled out, stood-down lane, double-billed elsewhere).",
        unreachable.len(),
        unreachable.join("\n  ")
    );
}

#[test]
fn allowlist_has_no_stale_entries() {
    let shipped = shipped_names();
    let emits = scan_emits();

    let mut stale: Vec<String> = Vec::new();
    for (name, reason) in UNREACHABLE_ALLOWLIST {
        let emit = emits.iter().find(|e| e.name == *name);
        match emit {
            None => stale.push(format!("{name}: no emit site at all ({reason})")),
            Some(_e) if shipped.contains(*name) => {
                stale.push(format!("{name}: now SHIPPED, drop the row ({reason})"));
            }
            Some(e) if e.logged => {
                stale.push(format!("{name}: now LOGGED, drop the row ({reason})"));
            }
            Some(_) => {}
        }
    }
    stale.sort();

    assert!(
        stale.is_empty(),
        "UNREACHABLE_ALLOWLIST has stale row(s):\n  {}\n\n\
         A stale exemption is the same false-OK as the gap it was meant to \
         record: it says a decision is still pending when it has already been \
         made. This list may only shrink — remove the rows above.",
        stale.join("\n  ")
    );
}

#[test]
fn allowlist_is_sorted_and_unique_with_reasons() {
    let names: Vec<&str> = UNREACHABLE_ALLOWLIST.iter().map(|(n, _)| *n).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "UNREACHABLE_ALLOWLIST must stay sorted so diffs stay readable"
    );

    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len(), "duplicate allowlist entries");

    for (name, reason) in UNREACHABLE_ALLOWLIST {
        assert!(name.starts_with("tv_"), "{name} is not a tv_ metric name");
        assert!(
            is_loss_shaped(name),
            "{name} is not loss-shaped — it does not belong in this list"
        );
        assert!(
            reason.len() >= 10,
            "{name} needs a real reason, not `{reason}` — a reviewer has to be \
             able to check it"
        );
    }
}

/// No row may say "untriaged". Every exemption states a checkable mechanism.
///
/// This exists because of a mistake worth not repeating. The allowlist was
/// seeded with 20 rows reading `"untriaged (seeded 2026-08-12)"`, and after
/// working through them I reported "untriaged: 20 → 0" — in a commit message
/// and a PR body — on the strength of
/// `grep -c '"untriaged (seeded 2026-08-12)"'` returning zero.
///
/// One row had an ANNOTATED variant (`"untriaged (…) — gauge twin of …"`),
/// which that exact-match grep never saw. The count said zero, the claim said
/// zero, and one row was still unexamined. A substring check would have caught
/// it; an exact-string check verified a narrower thing than the sentence it was
/// used to support.
///
/// So the property is asserted directly on the data instead of being counted by
/// hand: if anyone seeds a fresh batch of untriaged rows, the build says so
/// rather than waiting for someone to grep for the right spelling.
#[test]
fn no_allowlist_row_is_still_untriaged() {
    let stalled: Vec<&str> = UNREACHABLE_ALLOWLIST
        .iter()
        .filter(|(_, reason)| reason.to_ascii_lowercase().contains("untriaged"))
        .map(|(name, _)| *name)
        .collect();

    assert!(
        stalled.is_empty(),
        "{} allowlist row(s) still say `untriaged`, meaning nobody has looked \
         at them:\n  {}\n\n\
         An allowlist of unexamined entries records ignorance without reducing \
         it, and decays into a permanent exemption list. Replace each with the \
         mechanism that actually makes the counter reachable — or, if it is \
         genuinely unreachable, fix the emit site.",
        stalled.len(),
        stalled.join("\n  ")
    );
}

/// The scanner must actually find things, or every assertion above is vacuous.
#[test]
fn scanner_is_not_vacuous() {
    let emits = scan_emits();
    assert!(
        emits.len() >= 50,
        "expected the workspace to emit many loss-shaped counters, found {} — \
         the scanner is probably broken, which would make the guard above pass \
         for the wrong reason",
        emits.len()
    );
    assert!(
        emits.iter().any(|e| e.logged),
        "no emit site was detected as logged — the log-proximity detector is \
         broken and every counter would look unreachable"
    );
    assert!(
        !shipped_names().is_empty(),
        "no shipped names parsed from the EMF selector — the parser is broken \
         and every counter would look unshipped"
    );
}

/// Loss-shape classification, including the cases that must NOT match.
#[test]
fn loss_shape_classifier_is_precise() {
    for yes in [
        "tv_ticks_dropped_total",
        "tv_seq_refused_total",
        "tv_persist_errors_total",
        "tv_rows_discarded_total",
        "tv_frames_lost_total",
        "tv_dial_failed_total",
    ] {
        assert!(is_loss_shaped(yes), "{yes} should be loss-shaped");
    }
    for no in [
        "tv_ticks_total",
        "tv_process_rss_bytes",
        "tv_boot_completed",
        "tv_daily_pnl",
        "tv_instruments_silent",
        // The positional case: a SUCCESS counter that merely contains a loss
        // word in the middle. A plain `contains` matched this and would have
        // demanded operator visibility for a refresh-succeeded counter.
        "tv_errors_summary_refresh_total",
    ] {
        assert!(!is_loss_shaped(no), "{no} must NOT be loss-shaped");
    }
    // ...while its loss twin, differing only in tail position, must match.
    assert!(
        is_loss_shaped("tv_errors_summary_refresh_failed_total"),
        "the _failed_ twin IS a loss claim"
    );
}

/// Comment-only mentions must not register as emit sites.
#[test]
fn tv_name_extractor_handles_real_lines() {
    let names = tv_names_in(r#"metrics::counter!("tv_ticks_dropped_total").increment(1);"#);
    assert_eq!(names, vec!["tv_ticks_dropped_total"]);

    let two = tv_names_in("tv_a_dropped_total|tv_b_refused_total");
    assert_eq!(two, vec!["tv_a_dropped_total", "tv_b_refused_total"]);

    assert!(tv_names_in("no metrics here").is_empty());
}
