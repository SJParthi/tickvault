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
fn instance_lock_authoritative_rule_file_pins_t4g_medium() {
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
        body.contains("Instance lock — t4g.medium"),
        "the §7 heading must lock t4g.medium (not r8g.large) as the CURRENT instance type"
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
/// approved the t4g.medium downsize 2026-07-15 (Quote 8); the pinned
/// number is the honest INTERIM ~₹1,471/mo (270 hrs, EIP kept, incl.
/// 18% GST, LIVE 50 GB root — gp3 cannot shrink tonight). ~₹1,197/mo is
/// pinned only as the labelled post-20-GB-recreate figure, and ~₹986/mo
/// requires BOTH the ~176-hr auto-schedule basis AND the post-recreate
/// 20 GB volume (on the live 50 GB root the ~176-hr figure is ~₹1,260) —
/// the guard requires those labels so the live bill is never misstated.
/// If someone re-tunes any of this in the rule file without operator
/// approval, the build fails.
#[test]
fn instance_lock_monthly_bill_pinned_to_rupees_1471_interim() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("~₹1,471/mo") || body.contains("Rs 1,471/mo"),
        "rule file §7 must pin the INTERIM ~₹1,471/mo bill (operator approved 2026-07-15 Quote 8: t4g.medium, 270 hrs, live 50 GB EBS, +EIP, incl GST)"
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
