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

use cov_xvs_support::{FakeQuestDb, TempRoot, init_tracing, is_weekday, ist_now, weekday_calendar};
use tickvault_app::expiry_scope_check::spawn_expiry_scope_check;

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
    init_tracing();
    let original_cwd = std::env::current_dir().expect("cwd");
    let root = TempRoot::new("expiry-spawn");
    let (today, _) = ist_now();
    write_artifact(&root.path, today);
    std::env::set_current_dir(&root.path).expect("chdir temp");

    let srv = FakeQuestDb::start(responder());
    let handle = spawn_expiry_scope_check(srv.questdb_config(), weekday_calendar());

    let real_start = std::time::Instant::now();
    let mut found: Option<String> = None;
    while real_start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_secs(600)).await;
        found = srv
            .writes()
            .iter()
            .flat_map(|w| w.lines().map(str::to_owned).collect::<Vec<_>>())
            .find(|l| l.starts_with("expiry_scope_audit"));
        if found.is_some() {
            break;
        }
    }
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");

    std::env::set_current_dir(&original_cwd).expect("chdir back");
    root.cleanup();

    let (end_day, _) = ist_now();
    if end_day != today {
        return; // crossed IST midnight during the test; no stable expectation
    }
    if is_weekday(today) {
        let line = found.expect("a weekday run writes one expiry_scope_audit row");
        assert!(line.contains("feed=dhan"), "{line}");
        assert!(
            line.contains("verdict=partial"),
            "an under-specified day is never clean nor a violation: {line}"
        );
    } else {
        assert!(found.is_none(), "a weekend writes no row: {found:?}");
    }
}
