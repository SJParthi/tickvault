//! Coverage for `expiry_scope_check::spawn_expiry_scope_check` end to end,
//! against a QuestDB-shaped server on 127.0.0.1 and a paused tokio clock.
//!
//! The task reads the wall clock to pick today, so what it does depends on
//! the IST weekday: on Monday to Friday (a calendar with no holidays) it runs
//! today's check and writes one `expiry_scope_audit` row over ILP/HTTP; on a
//! weekend it writes nothing. Both are asserted.
//!
//! One test in this binary: it changes the process working directory to a
//! temp folder so the relative contract-artifact read finds the file written
//! here and nothing is written inside the repository.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use cov_xvs_support::{
    CwdGuard, FakeQuestDb, drive_paused, init_no_proxy, init_tracing, is_weekday, ist_now,
    lock_cwd, weekday_calendar,
};
use tickvault_app::expiry_scope_check::spawn_expiry_scope_check;
use tickvault_storage::expiry_scope_audit_persistence::EXPIRY_SCOPE_AUDIT_TABLE;

fn responder() -> cov_xvs_support::ExecResponder {
    Arc::new(|q: &str| {
        if q.contains("wal_tables") {
            (200, r#"{"columns":[],"dataset":[]}"#.to_owned())
        } else if q.contains("candles_1m") {
            (
                200,
                r#"{"columns":[],"dataset":[[1001,"NSE_FNO",null],["x","NSE_FNO","BAD"],[]]}"#
                    .to_owned(),
            )
        } else {
            (200, r#"{"ddl":"OK"}"#.to_owned())
        }
    })
}

fn write_artifact(root: &std::path::Path, day: chrono::NaiveDate) {
    let dir = root.join("data/instrument-cache");
    std::fs::create_dir_all(&dir).expect("artifact dir");
    // One stock option on a far expiry: never expired, and with a single leg
    // its underlying has no real expiry, so the day can never read as a
    // violation and never as clean.
    let rows =
        r#"[{"i":1001,"x":"NSE","c":"OPTSTK","e":20991231,"s":100000,"l":"CE","u":"ABC","z":1}]"#;
    for d in [day, day.succ_opt().unwrap_or(day)] {
        let name = format!("dhan-contracts-{}.json", d.format("%Y-%m-%d"));
        std::fs::write(dir.join(name), rows).expect("artifact write");
    }
}

#[tokio::test(start_paused = true)]
async fn test_expiry_scope_check_spawn_expiry_scope_check_writes_a_partial_row_on_a_weekday() {
    init_no_proxy();
    init_tracing();
    let _cwd_lock = lock_cwd().await;
    let (today, _) = ist_now();
    let cwd = CwdGuard::enter("expiry-spawn");
    write_artifact(cwd.path(), today);

    let srv = FakeQuestDb::start(responder());
    let handle = spawn_expiry_scope_check(srv.questdb_config(), weekday_calendar());

    // 17 h of virtual time reaches the 15:50 IST check from any start (and a
    // weekday after 15:50 runs it at once); the loop stops at the first row.
    let rows = || srv.ilp_lines(EXPIRY_SCOPE_AUDIT_TABLE);
    drive_paused(&srv, Duration::from_secs(17 * 3_600), || !rows().is_empty()).await;
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");

    let found = rows();
    let queries = srv.exec_queries();
    let requests = srv.request_lines();
    drop(cwd);

    // Every day: the task ensures its table first, and never calls Dhan.
    assert!(
        queries
            .iter()
            .any(|q| q.starts_with("CREATE TABLE IF NOT EXISTS expiry_scope_audit")),
        "the audit table DDL reached /exec: {queries:?}"
    );
    assert!(
        !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
        "the vendor is never called: {requests:?}"
    );
    assert!(found.len() <= 1, "at most one row per day: {found:?}");

    let (end_day, _) = ist_now();
    if end_day != today {
        return; // crossed IST midnight during the test; no stable expectation
    }
    let candle_read = format!("from candles_1m where ts in '{}'", today.format("%Y-%m-%d"));
    if is_weekday(today) {
        assert_eq!(found.len(), 1, "a weekday run writes one row: {found:?}");
        let line = &found[0];
        assert!(line.contains("feed=dhan"), "{line}");
        assert!(
            line.contains("verdict=partial"),
            "an under-specified day is never clean nor a violation: {line}"
        );
        assert!(
            queries.iter().any(|q| q.contains(&candle_read)),
            "today's candle read reached /exec: {queries:?}"
        );
    } else {
        assert!(found.is_empty(), "a weekend writes no row: {found:?}");
        assert!(
            !queries.iter().any(|q| q.contains("candles_1m")),
            "a weekend reads no candles: {queries:?}"
        );
    }
}
