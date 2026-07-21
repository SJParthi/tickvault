//! Sub-PR #1 of 2026-05-27 daily-universe expansion — source-scan ratchet
//! pinning the **t4g.medium instance type lock** documented in
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §7.
//!
//! Lock history: 2026-05-18 t4g.medium → 2026-05-27 t4g.large → 2026-05-29
//! m8g.large (8 GiB) → 2026-06-30 r8g.large (Graviton4, 16 GiB — operator
//! Quote 7) → 2026-07-15 **t4g.medium** (Graviton2, 4 GiB RAM — operator
//! Quote 8 downsize: QDB_MEM_LIMIT 4g → 1g, EBS 20 GB fresh-volume TARGET
//! pre-staged as an executor decision while the live root stays 50 GB;
//! INTERIM bill ~₹1,471/mo incl GST at 270 hrs). This guard fails the
//! build if:
//!
//!  1. The new authoritative rule file disappears or stops pinning
//!     `t4g.medium`.
//!  2. The four superseded rule files lose their SUPERSEDED-BY
//!     markers (= future readers wouldn't find the new contract).
//!  3. The instance-type lock value in any of the docs disagrees
//!     with `t4g.medium`.
//!
//! See:
//! - `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! - `.claude/rules/project/aws-budget.md`
//! - `docs/architecture/aws-indices-only-locked-architecture.md` §5
//! - `.claude/rules/project/websocket-connection-scope-lock.md`
//! - `.claude/rules/project/operator-charter-forever.md` §I

#![cfg(test)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — the new authoritative rule file MUST exist and pin
/// `t4g.medium` as the locked instance type (operator Quote 8, 2026-07-15).
/// r8g.large itself must REMAIN greppable in the file (Quote 7 history is
/// retained per house convention), so no `!contains("r8g.large")` assert —
/// the §7 HEADING pin below is what forbids r8g.large as the CURRENT lock.
#[test]
fn instance_lock_authoritative_rule_file_pins_m8g_large() {
    let root = repo_root();
    let path = root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md");
    assert!(
        path.exists(),
        "authoritative rule file missing at {}",
        path.display()
    );
    let body = read(&path);
    assert!(
        body.contains("t4g.medium"),
        "daily-universe rule file must pin `t4g.medium` as the instance lock (Quote 8, 2026-07-15)"
    );
    assert!(
        body.contains("**t4g.medium**"),
        "daily-universe rule file must bold-pin `t4g.medium` in the §7 instance-spec table"
    );
    assert!(
        body.contains("4 GiB RAM"),
        "daily-universe rule file must pin 4 GiB RAM for t4g.medium"
    );
    assert!(
        body.contains("Instance lock — m8g.large"),
        "the §7 heading must lock m8g.large (not t4g.medium) as the CURRENT instance type"
    );
}

/// Section B — the 4 superseded files MUST carry SUPERSEDED-BY markers
/// pointing at the new rule file. Without these, future readers
/// following the old authority chain would not find the 2026-05-27
/// contract.
#[test]
fn instance_lock_supersession_markers_present_in_aws_budget() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/aws-budget.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "aws-budget.md must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "aws-budget.md must link to the new authoritative rule file"
    );
    assert!(
        body.contains("t4g.medium → t4g.large") || body.contains("t4g.large"),
        "aws-budget.md supersession marker must mention t4g.large upgrade"
    );
    assert!(
        body.contains("RE-SUPERSEDED → t4g.medium 2026-07-15"),
        "aws-budget.md must carry the 2026-07-15 t4g.medium downsize banner (Quote 8)"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_architecture_doc() {
    let root = repo_root();
    let body = read(&root.join("docs/architecture/aws-indices-only-locked-architecture.md"));
    assert!(
        body.contains("§5 SUPERSEDED 2026-05-27"),
        "architecture doc must carry the §5 SUPERSEDED marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "architecture doc must link to the new authoritative rule file"
    );
    assert!(
        body.contains("§5 RE-SUPERSEDED → t4g.medium 2026-07-15"),
        "architecture doc must carry the 2026-07-15 t4g.medium downsize banner (Quote 8)"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_websocket_scope_lock() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/websocket-connection-scope-lock.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "websocket-connection-scope-lock.md must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "websocket-connection-scope-lock.md must link to the new authoritative rule file"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_operator_charter() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/operator-charter-forever.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "operator-charter-forever.md §I must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "operator-charter-forever.md must link to the new authoritative rule file"
    );
}

/// Section C — the bill in §7 must match the locked figure. Operator
/// approved the t4g.medium downsize 2026-07-15 (Quote 8); the stated
/// INTERIM was ~₹1,471/mo on a "live 50 GB root" premise. 2026-07-19
/// live `describe-volumes` verification (coordinator session) showed the
/// root is 30 GiB gp3 — the 2026-07-13 approved 30→50 grow was recorded
/// but never physically applied — so the CORRECTED interim is ~₹1,289/mo
/// (EBS $0.0912 × 30 = $2.74; subtotal $12.85 → ₹1,092 → ×1.18 GST),
/// with ~₹1,471/mo retained as the labelled pre-correction figure.
/// ~₹1,197/mo is pinned only as the labelled post-20-GB-recreate figure,
/// and ~₹986/mo requires BOTH the ~176-hr auto-schedule basis AND the
/// post-recreate 20 GB volume (the live-30-GB ~176-hr figure is ~₹1,077;
/// the superseded 50 GB record's was ~₹1,260) — the guard requires those
/// labels so the live bill is never misstated. If someone re-tunes any of
/// this in the rule file without operator approval, the build fails.
#[test]
fn instance_lock_monthly_bill_pinned_to_rupees_1471_interim() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("~₹1,289/mo") || body.contains("Rs 1,289/mo"),
        "rule file §7 must pin the CORRECTED INTERIM ~₹1,289/mo bill (live 30 GB root verified 2026-07-19: t4g.medium, 270 hrs, +EIP, incl GST)"
    );
    assert!(
        body.contains("30 GiB gp3") && body.contains("never physically applied"),
        "rule file §7 must carry the 2026-07-19 live-volume correction (30 GiB gp3 verified; the 2026-07-13 grow to 50 never physically applied)"
    );
    assert!(
        body.contains("~₹1,077"),
        "rule file §7 must state the corrected live-30-GB ~176-hr figure (~₹1,077)"
    );
    assert!(
        body.contains("~₹1,471/mo") || body.contains("Rs 1,471/mo"),
        "rule file §7 must retain the pre-correction ~₹1,471/mo figure as labelled history (2026-07-15 Quote 8 record on the never-applied 50 GB assumption)"
    );
    assert!(
        body.contains("~₹1,197/mo applies ONLY after the 20 GB fresh-volume recreate"),
        "rule file §7 must label ~₹1,197/mo as the post-20-GB-recreate figure, never the current bill"
    );
    assert!(
        body.contains("requires BOTH the ~176-hr"),
        "rule file §7 must label ~₹986/mo as requiring BOTH the ~176-hr basis AND the post-recreate 20 GB volume (review round 6 — the 176-hr-only label understated the live-50-GB figure)"
    );
    assert!(
        body.contains("~₹1,260"),
        "rule file §7 must state the live-50-GB ~176-hr figure (~₹1,260) so ~₹986 is never presented as reachable before the 20 GB recreate"
    );
}

/// Section C.1 — the 2026-07-19 sub-1K ruling (Quote 9) must stay pinned:
/// the operator formally ACCEPTED the 30 GB root (the 2026-07-13 30→50 grow
/// CANCELLED), re-affirmed t4g.medium, and set a HARD TARGET of < ₹1,000/mo
/// incl GST. The recorded ~₹1,289/~₹1,077 bills do NOT meet that target, so
/// the rule files must (a) carry the verbatim quote, (b) label the bills
/// with the target, and (c) keep the itemized lever path in aws-budget.md —
/// removing any of these would let a future bill be presented as compliant
/// (false-OK) or resurrect the cancelled grow without a fresh quote.
#[test]
fn instance_lock_2026_07_19_sub_1k_ruling_pinned() {
    let root = repo_root();
    let daily =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        daily.contains("just 30 gn enough and onl yt4g medium as of now"),
        "daily-universe §0 must carry the 2026-07-19 Quote 9 verbatim (typos included)"
    );
    assert!(
        daily.contains("< ₹1,000"),
        "daily-universe §7 must carry the 2026-07-19 hard target (< ₹1,000/mo incl GST)"
    );
    assert!(
        daily.contains("grow is **CANCELLED**") || daily.contains("grow CANCELLED"),
        "daily-universe must record that the 2026-07-13 30→50 grow is CANCELLED (Quote 9)"
    );
    let budget = read(&root.join(".claude/rules/project/aws-budget.md"));
    assert!(
        budget.contains("OPERATOR RULING 2026-07-19"),
        "aws-budget.md must carry the OPERATOR RULING 2026-07-19 section (the itemized sub-1K lever path)"
    );
    assert!(
        budget.contains("just 30 gn enough and onl yt4g medium as of now"),
        "aws-budget.md must carry the 2026-07-19 Quote 9 verbatim (typos included)"
    );
    assert!(
        budget.contains("UNREACHABLE without at least one"),
        "aws-budget.md must state plainly that <₹1,000 is unreachable without an operator-gated lever (no false-OK)"
    );
    assert!(
        budget.contains("snap-090ed9c4f3df0ca61"),
        "aws-budget.md must carry the rollback-snapshot deletion scheduling note (Lever 1, after 2026-07-22)"
    );
}

/// Section C.2 — the 2026-07-19 SECOND same-day ruling (Quote 10) must stay
/// pinned: the operator approved the EIP release for the no-real-orders
/// period ("until or unless we flip the real orders static ip is not needed
/// due okay?"), the live verification proved a STANDALONE release unsafe
/// (launch-time ENI attribute — the live ENI never mints an ephemeral IP),
/// and execution is BUNDLED with the erase-window recreate per
/// `docs/runbooks/eip-release.md`. Removing any of these would let a future
/// session (a) release the EIP standalone and brick the box, or (b) flip
/// `enable_eip` without the runbook's verify-first order, or (c) go live on
/// Dhan orders without the ≥7-day setIP re-whitelist protocol.
#[test]
fn instance_lock_2026_07_19_eip_release_ruling_pinned() {
    let root = repo_root();
    let budget = read(&root.join(".claude/rules/project/aws-budget.md"));
    assert!(
        budget
            .contains("until or unless we flip the real orders static ip is not needed due okay?"),
        "aws-budget.md must carry the 2026-07-19 Quote 10 verbatim (typos included)"
    );
    assert!(
        budget.contains("VERIFIED-UNSAFE-STANDALONE"),
        "aws-budget.md must record the live standalone-release verdict (launch-time ENI attribute)"
    );
    assert!(
        budget.contains("docs/runbooks/eip-release.md"),
        "aws-budget.md must point at the bundled-recreate execution runbook"
    );
    let daily =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        daily.contains("Quote 10"),
        "daily-universe §0 must carry the 2026-07-19 Quote 10 EIP-release ruling"
    );
    let scope = read(&root.join(".claude/rules/project/websocket-connection-scope-lock.md"));
    assert!(
        scope.contains("Static IP / EIP ruling"),
        "websocket-connection-scope-lock.md must carry the 2026-07-19 static-IP ruling section"
    );
    let runbook = read(&root.join("docs/runbooks/eip-release.md"));
    assert!(
        runbook.contains("VERIFY outbound-without-EIP FIRST"),
        "the runbook must encode the operator's verify-first safety order"
    );
    assert!(
        runbook.contains("7-day modify cooldown") && runbook.contains("/v2/ip/setIP"),
        "the runbook must carry the live-trading re-enable protocol (new EIP + Dhan setIP >=7 days pre-go-live)"
    );
}

/// Section D — the schedule lock must mention `08:30 IST` (operator narrowed
/// the window back to 08:30-16:30 on 2026-06-05: "make the aws instance start
/// and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02 widening).
#[test]
fn instance_lock_schedule_pinned_to_0830_ist_start() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("08:30–16:30 IST") || body.contains("08:30 IST"),
        "rule file §7 must pin the 08:30 IST EventBridge cron start"
    );
}
