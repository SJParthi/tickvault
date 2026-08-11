//! Ratchet: every HIGH/CRITICAL error code is either alarmed or explicitly,
//! reviewably exempt — and the exempt list can only ever SHRINK (2026-08-10).
//!
//! # The gap this closes
//!
//! `ErrorCode` carries 167 variants. 102 of them are `Severity::High` or
//! `Severity::Critical` — the two levels the enum's own doc comment says are
//! routed to Telegram and SNS. Only 11 error codes have a CloudWatch
//! metric-filter alarm in `error-code-alarms.tf`.
//!
//! That is not, by itself, a bug. `observability-architecture.md` states the
//! paging contract as a three-way OR: a code pages if it has an error-code
//! filter, **or** its own metric alarm, **or** a typed `NotificationEvent`;
//! everything else is log-sink-only on purpose. Several families are
//! contractually log-sink-only and adding an alarm for them is an explicit
//! REJECT (`dhan-exit-order-lockout-2026-07-14.md` §3 names EXIT-ORDER-01 and
//! EXIT-VERIFY-01 that way, and the Groww order-side runbooks say the same for
//! ten more).
//!
//! The bug is that **the choice was never recorded anywhere a machine can
//! read**. A new High/Critical variant lands, nobody wires an alarm, nobody
//! writes down that it was deliberate, and the difference between "we decided
//! this is log-sink-only" and "we forgot" is invisible forever after. That is
//! the same silent-drift class as a guard that scans zero files: the state
//! looks identical whether the work was done or skipped.
//!
//! # What this guard enforces
//!
//! Every High/Critical code must appear in exactly one of two places:
//!
//!   * a `$.code = "..."` (or bare-term) filter in `error-code-alarms.tf`, or
//!   * `LOG_SINK_ONLY_EXEMPT` below, which is a **shrink-only ratchet**.
//!
//! A new High/Critical variant therefore cannot be added without a deliberate
//! act in one direction or the other, and an existing exemption cannot be
//! quietly re-taken once an alarm exists for it.
//!
//! # What this guard deliberately does NOT do
//!
//! It does not demand an alarm for every High/Critical code. Three reasons,
//! each verified rather than assumed:
//!
//!   * **Some are unsatisfiable.** At least 26 of the uncovered codes have no
//!     production `error!` emit site at all. `error_code_paging_filter_drift_guard`
//!     already fails the build on a filter whose code can never be emitted, so
//!     "add an alarm" would turn a different test red.
//!   * **Some are forbidden by contract.** See the rule-file citations beside
//!     the exempt entries.
//!   * **They are not free.** `aws-budget.md` prices a CloudWatch alarm at
//!     ~$0.10/month. Ninety-one of them is ~$9.10/month against a live budget
//!     kill-ceiling and a stated sub-₹1,000/month target — a cost decision that
//!     belongs to the operator, not to a test.
//!
//! So this guard makes the coverage *decision* mandatory and mechanical. It
//! does not make the decision.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::{ErrorCode, Severity};

const ALARM_TF: &str = "deploy/aws/terraform/error-code-alarms.tf";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

/// Strip HCL comments without cutting inside a quoted string.
///
/// This matters more than it looks: every alarm `pattern` in the file contains
/// `{`, `}` and `$`, and several `desc` strings contain `#`. A naive
/// line-comment strip would truncate a pattern mid-string and silently drop the
/// code it references — which would make this guard report a MISSING alarm that
/// actually exists. Erring that way is loud rather than dangerous, but it would
/// still be wrong.
fn strip_hcl_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut escaped = false;
        let mut cut = line.len();
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i];
            if escaped {
                escaped = false;
            } else if c == b'\\' && in_str {
                escaped = true;
            } else if c == b'"' {
                in_str = !in_str;
            } else if !in_str && c == b'#' {
                cut = i;
                break;
            } else if !in_str && c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
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

/// Every error code the terraform file references, in any filter shape.
///
/// Matching is done by asking "does this code's canonical string appear as a
/// quoted token in the comment-stripped file?" rather than by parsing HCL. The
/// file uses two filter shapes today — `{ $.code = \"DH-901\" ... }` and the
/// bare term `"\"DH-906\""` — and a third shape appearing later should still be
/// recognised, so a structural parser tuned to today's two would be the fragile
/// choice here.
fn alarmed_codes() -> BTreeSet<&'static str> {
    let tf = strip_hcl_comments(&read(ALARM_TF));
    ErrorCode::all()
        .iter()
        .map(|c| c.code_str())
        .filter(|code| {
            // Require the code to be delimited, so `CHAIN-01` does not match
            // inside a hypothetical `CHAIN-011`.
            tf.match_indices(*code).any(|(idx, _)| {
                let after = tf.as_bytes().get(idx + code.len()).copied();
                !matches!(after, Some(b) if b.is_ascii_alphanumeric() || b == b'-')
            })
        })
        .collect()
}

fn high_or_critical() -> Vec<ErrorCode> {
    ErrorCode::all()
        .iter()
        .copied()
        .filter(|c| matches!(c.severity(), Severity::High | Severity::Critical))
        .collect()
}

/// High/Critical codes that are deliberately NOT wired to a CloudWatch alarm.
///
/// **This list is a shrink-only ratchet.** Adding an entry requires the same
/// scrutiny as adding an alarm — it is a decision that this condition will
/// never page an operator through CloudWatch. Removing one (because an alarm
/// now exists) is always welcome and is enforced by
/// `exemption_list_has_no_stale_entries` below.
///
/// Seeded 2026-08-10 from the live state, so this guard starts as a freeze on
/// today's coverage rather than a demand for new spend. Three groups, by why:
///
///   * **contractual** — a rule file explicitly forbids an alarm. Named below.
///   * **no emit site** — the code has no production `error!` today, so an
///     alarm would be a dead filter and `error_code_paging_filter_drift_guard`
///     would fail the build for it.
///   * **typed event** — the operator page is a `NotificationEvent` (Telegram),
///     which is a legitimate leg of the documented paging OR but is not
///     mechanically discoverable, so it must be recorded here by hand.
const LOG_SINK_ONLY_EXEMPT: &[&str] = &[
    // Seeded 2026-08-10 by running this guard against the live tree and pasting
    // its own failure output. Deliberately NOT annotated with 90 individually
    // researched reasons: inventing a plausible-sounding justification per line
    // would be exactly the fabricated-confidence this repo's charter forbids,
    // and a wrong reason is worse than an honest "not yet reviewed" because the
    // next reader trusts it.
    //
    // What IS verified about this set, and applies to it as a whole:
    //   * every entry is a real High/Critical `ErrorCode::code_str()` (pinned by
    //     `exemption_list_has_no_stale_entries`);
    //   * none of them has a CloudWatch filter today (same test);
    //   * at least 26 have no production `error!` emit site at all, so an alarm
    //     would be a dead filter that `error_code_paging_filter_drift_guard`
    //     independently fails the build for;
    //   * the EXIT-ORDER-01 / EXIT-VERIFY-01 and Groww order-side families are
    //     contractually log-sink-only — `dhan-exit-order-lockout-2026-07-14.md`
    //     §3 lists adding an alarm for them as an explicit REJECT.
    //
    // The value of this list is not the reasons. It is that from today the set
    // can only SHRINK, and no NEW High/Critical code can join it without
    // someone typing it in on purpose.
    //
    // SHRUNK 2026-08-11 (merge): BOOT-02, BOOT-03, OMS-GAP-06 and WS-SPILL-02
    // are removed because they now HAVE CloudWatch filters. This list was
    // seeded on one branch while the Critical-severity paging-gap sweep added
    // those four alarms on another; the merge made the entries stale and
    // `exemption_list_has_no_stale_entries` failed on exactly them, by name.
    // Worth recording because it is the ratchet earning its keep on its first
    // real merge: had the entries survived, a later removal of those four
    // alarms would have passed silently — the exact failure mode the test was
    // written to prevent, arrived at by accident rather than intent.
    "AGGREGATOR-LAG-01",
    "AGGREGATOR-LATE-01",
    "AUTH-GAP-01",
    "AUTH-GAP-02",
    "BAR-MISMATCH-01",
    "BAR-MISMATCH-02",
    "BAR-MISMATCH-03",
    "BOOT-01",
    "BRUTEX-XVERIFY-01",
    "BRUTEX-XVERIFY-02",
    "CADENCE-01",
    "CADENCE-02",
    "CHAIN-03",
    "DATA-805",
    "DATA-807",
    "DATA-808",
    "DATA-809",
    "DATA-810",
    "DH-903",
    "DH-904",
    "DH-905",
    "EXIT-ORDER-01",
    "EXIT-VERIFY-01",
    "FEED-REJECT-01",
    "FEED-STALL-01",
    "FEED-SUPERVISOR-01",
    "FOLD-01",
    "FUTIDX-01",
    "FUTIDX-02",
    "GROWW-MARG-01",
    "GROWW-MARG-02",
    "GROWW-MARG-03",
    "GROWW-MARG-04",
    "GROWW-OCO-01",
    "GROWW-OCO-02",
    "GROWW-OCO-03",
    "GROWW-OCO-05",
    "GROWW-ORD-01",
    "GROWW-ORD-02",
    "GROWW-ORD-03",
    "GROWW-ORD-04",
    "GROWW-ORD-06",
    "GROWW-ORD-09",
    "GROWW-ORD-10",
    "GROWW-PORT-01",
    "GROWW-PORT-02",
    "GROWW-PORT-03",
    "GROWW-PORT-04",
    "GROWW-PUSH-01",
    "GROWW-PUSH-02",
    "GROWW-PUSH-04",
    "GROWW-SCALE-01",
    "GROWW-SCALE-02",
    "GROWW-SCALE-03",
    "GROWW-SCALE-05",
    "HTTP-CLIENT-01",
    "I-P0-03",
    "INDEX-OHLC-02",
    "OMS-GAP-01",
    "OMS-GAP-02",
    "OMS-GAP-03",
    "OMS-GAP-04",
    "OMS-GAP-05",
    "ORDER-EVT-01",
    "ORDER-PNL-01",
    "ORDER-READY-01",
    "ORPHAN-POSITION-01",
    "PREVCLOSE-03",
    "RAMSTORE-01",
    "RESILIENCE-01",
    "RESILIENCE-03",
    "RESOURCE-01",
    "RESOURCE-02",
    "RESOURCE-03",
    "RISK-GAP-01",
    "RISK-GAP-02",
    "SELFTEST-02",
    "SPOT-XVERIFY-01",
    "SPOT-XVERIFY-02",
    "SPOT1M-02",
    "TF-VERIFY-01",
    "TF-VERIFY-02",
    "TICK-FLUSH-01",
    "VOLUME-MONO-01",
    "WS-GAP-10",
    "WS-SPILL-01",
];

#[test]
fn every_high_or_critical_code_is_alarmed_or_explicitly_exempt() {
    let alarmed = alarmed_codes();
    let exempt: BTreeSet<&str> = LOG_SINK_ONLY_EXEMPT.iter().copied().collect();

    let mut undecided: Vec<&'static str> = Vec::new();
    for code in high_or_critical() {
        let s = code.code_str();
        if !alarmed.contains(s) && !exempt.contains(s) {
            undecided.push(s);
        }
    }
    undecided.sort_unstable();

    assert!(
        undecided.is_empty(),
        "UNDECIDED PAGING COVERAGE: {} High/Critical error code(s) have neither a \
CloudWatch filter in {ALARM_TF} nor an entry in LOG_SINK_ONLY_EXEMPT. Every \
High/Critical code must be one or the other — an alarm if the operator should be \
woken, an exemption (with the reason) if it is log-sink-only by design. Silence \
because nobody chose is the state this guard exists to make impossible.\n\n\
Add each of these as an alarm entry, or paste it into LOG_SINK_ONLY_EXEMPT with \
its reason:\n{}",
        undecided.len(),
        undecided
            .iter()
            .map(|c| format!("    \"{c}\","))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn exemption_list_has_no_stale_entries() {
    let alarmed = alarmed_codes();
    let all_codes: BTreeSet<&str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    let hc: BTreeSet<&str> = high_or_critical().iter().map(|c| c.code_str()).collect();

    let mut now_alarmed = Vec::new();
    let mut not_a_code = Vec::new();
    let mut not_high = Vec::new();
    let mut dupes = Vec::new();
    let mut seen = BTreeSet::new();

    for entry in LOG_SINK_ONLY_EXEMPT {
        if !seen.insert(*entry) {
            dupes.push(*entry);
        }
        if !all_codes.contains(entry) {
            not_a_code.push(*entry);
            continue;
        }
        if alarmed.contains(entry) {
            now_alarmed.push(*entry);
        }
        if !hc.contains(entry) {
            not_high.push(*entry);
        }
    }

    assert!(
        now_alarmed.is_empty(),
        "STALE EXEMPTIONS: these codes now HAVE a CloudWatch filter, so their \
log-sink-only exemption is obsolete and must be deleted. Leaving it would let a \
future alarm removal pass unnoticed, which is the whole failure mode this \
ratchet is built to catch: {now_alarmed:?}"
    );
    assert!(
        not_a_code.is_empty(),
        "UNKNOWN EXEMPTIONS: these strings are not any ErrorCode::code_str() — a \
typo here silently exempts nothing while looking like it exempts something: \
{not_a_code:?}"
    );
    assert!(
        not_high.is_empty(),
        "POINTLESS EXEMPTIONS: these codes are not High/Critical, so this guard \
never asks about them. Remove them so the list stays a truthful record of what \
was actually decided: {not_high:?}"
    );
    assert!(
        dupes.is_empty(),
        "DUPLICATE EXEMPTIONS: {dupes:?} — a duplicated entry usually means two \
people exempted the same code for different reasons and one reason is now lost."
    );
}

#[test]
fn alarm_file_still_carries_a_real_corpus() {
    // Anti-vacuity. Both tests above are satisfiable by a `alarmed_codes()`
    // that returns nothing (everything would fall to the exempt list) — and by
    // a terraform file that was deleted, emptied, or moved. This asserts the
    // corpus is real, so the guard cannot pass by scanning nothing. The
    // branch this shipped on found 22 guards that could do exactly that.
    let raw = read(ALARM_TF);
    assert!(
        raw.len() > 4_000,
        "{ALARM_TF} is only {} bytes — it has been emptied, truncated or moved, \
and this guard would silently be checking nothing",
        raw.len()
    );

    let alarmed = alarmed_codes();
    assert!(
        alarmed.len() >= 11,
        "only {} error code(s) found in {ALARM_TF}; 11 existed on 2026-08-10. \
Alarms may be added freely, but a DROP means paging coverage was deleted — say \
so in a dated rule-file note and lower this floor deliberately. Found: {alarmed:?}",
        alarmed.len()
    );

    let hc = high_or_critical();
    assert!(
        hc.len() >= 100,
        "only {} High/Critical variants seen; 102 existed on 2026-08-10. Either \
ErrorCode::all() has shrunk or severity() was re-graded downward — both change \
what pages an operator and neither should happen silently",
        hc.len()
    );
}

#[test]
fn comment_stripper_does_not_cut_inside_a_pattern_string() {
    // The stripper is the one piece of parsing here, so it gets a bite test.
    // A `#` inside a quoted pattern must survive; a real comment must not.
    let src = r#"
    "dh-901" = {
      pattern = "{ $.code = \"DH-901\" && $.msg = \"a # inside\" }"   # real comment
    }
"#;
    let out = strip_hcl_comments(src);
    assert!(
        out.contains("a # inside"),
        "stripper cut inside a quoted string: {out}"
    );
    assert!(
        !out.contains("real comment"),
        "stripper failed to remove an actual comment: {out}"
    );
    assert!(
        out.contains("DH-901"),
        "stripper destroyed the code token: {out}"
    );
}
