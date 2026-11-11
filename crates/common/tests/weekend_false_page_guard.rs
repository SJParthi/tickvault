//! Guard: a `treat_missing_data = "breaching"` alarm watching a MON-FRI-only
//! Lambda schedule must be inside the market-hours gate, or it pages every
//! weekend forever.
//!
//! # The arithmetic this pins
//!
//! Three alarms answer the question "did this scheduled check actually RUN?":
//! `deploy-watchdog-not-invoked`, `market-open-readiness-not-invoked` and
//! `boot-heartbeat-gate-not-invoked`. Each is
//! `Invocations < 1`, `period = 86400`, `evaluation_periods = 1`,
//! `treat_missing_data = "breaching"`, on a Lambda whose EventBridge rule is
//! `cron(... ? * MON-FRI *)`.
//!
//! On Saturday and Sunday the watched Lambda correctly does not run. Its
//! Invocations datapoint is therefore ABSENT, `breaching` turns that absence
//! into ALARM, and all three page — roughly six weekend pages a month, from
//! three checks behaving exactly as designed. That is the fastest possible way
//! to train an operator to ignore an alarm, and the alarms it teaches them to
//! ignore include the one that catches a box booted on a stale binary.
//!
//! # Why the obvious fix is the wrong one
//!
//! `treat_missing_data = "notBreaching"` would silence the weekend — by making
//! the check blind. ABSENCE IS THE CONDITION: a dropped EventBridge schedule
//! produces no invocation, therefore no error, which is precisely why each
//! alarm's sibling `-errors` alarm cannot see this class at all. Flipping to
//! notBreaching trades a false page for a false OK, and a false OK on a
//! did-it-run check is worse than the noise.
//!
//! The correct half to change is the ACTIONS, not the evaluation: the
//! market-hours gate Lambda disables them Friday 15:35 IST → Monday 09:20 IST.
//! (That gate resets each alarm to OK BEFORE enabling actions, so Monday brings
//! no spurious "recovered" either — pinned by `market_hours_gate.rs`s own
//! ordering test, not duplicated here.)
//! This guard exists because those two halves live in four different files, and
//! a future edit that drops a gate membership while leaving `breaching` in
//! place reintroduces the weekend storm with nothing to catch it.

const GATE: &str = include_str!("../../../deploy/aws/terraform/market-hours-liveness-alarm.tf");
const DEPLOY_WATCHDOG: &str =
    include_str!("../../../deploy/aws/terraform/deploy-watchdog-lambda.tf");
const MARKET_OPEN_READINESS: &str =
    include_str!("../../../deploy/aws/terraform/market-open-readiness-lambda.tf");
const BOOT_HEARTBEAT: &str = include_str!("../../../deploy/aws/terraform/boot-heartbeat-alarm.tf");

/// The three (file, terraform resource name) pairs this guard covers. Named
/// explicitly rather than derived: "is this alarm's metric produced by a
/// MON-FRI-only schedule?" is not decidable from the terraform, so the set is
/// pinned by hand with the reason written down.
const DID_IT_RUN_ALARMS: [(&str, &str); 3] = [
    (DEPLOY_WATCHDOG, "deploy_watchdog_not_invoked"),
    (MARKET_OPEN_READINESS, "market_open_readiness_not_invoked"),
    (BOOT_HEARTBEAT, "boot_heartbeat_gate_not_invoked"),
];

/// Strip `#` comments so prose naming a resource cannot satisfy a scan meant to
/// find a real terraform reference.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| match line.find('#') {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of one `resource "aws_cloudwatch_metric_alarm" "<name>" { ... }`
/// block, bounded at its matching brace so a neighbour's settings can never be
/// attributed to this alarm.
fn alarm_block<'a>(src: &'a str, resource: &str) -> &'a str {
    let needle = format!("resource \"aws_cloudwatch_metric_alarm\" \"{resource}\"");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("alarm resource `{resource}` not found"));
    let rest = &src[start..];
    let open = rest.find('{').expect("resource block must have a brace");
    let mut depth = 0usize;
    for (idx, ch) in rest[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..open + idx + 1];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in `{resource}`");
}

/// The contents of the gate Lambda's `ALARM_NAMES = join(",", [ ... ])` list —
/// membership must be INSIDE the list, not merely somewhere in the file.
fn gate_alarm_names_list(gate: &str) -> &str {
    let at = gate
        .find("ALARM_NAMES")
        .expect("the gate Lambda must still take an ALARM_NAMES env list");
    let tail = &gate[at..];
    let end = tail.find("])").expect("ALARM_NAMES must be a join() list");
    &tail[..end]
}

#[test]
fn every_did_it_run_alarm_is_gated_and_ships_disarmed() {
    let gate = code_only(GATE);
    let list = gate_alarm_names_list(&gate);

    let review = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/audits/2026-11-alarm-review.md"),
    )
    .expect("the Quote 53 alarm review must be readable"); // APPROVED: test
    for (src, resource) in DID_IT_RUN_ALARMS {
        let code = code_only(src);
        // 2026-11-11: operator Quote 53 removed two of the three did-it-run
        // alarms (rows in docs/audits/2026-11-alarm-review.md); the review KEPT
        // boot_heartbeat_gate_not_invoked, which every check below covers. A removed one needs its
        // REMOVE row and no leftover gate reference; if one comes back, every
        // check below applies to it again unchanged.
        let header = format!("resource \"aws_cloudwatch_metric_alarm\" \"{resource}\"");
        if !code.contains(&header) {
            let row = format!("`tv-<env>-{}`", resource.replace('_', "-"));
            assert!(
                review
                    .lines()
                    .any(|l| l.contains("| REMOVE |") && l.contains(&row)),
                "`{resource}` is gone without a REMOVE row for {row} in the Quote 53 \
                 alarm review"
            );
            assert!(
                !list.contains(&format!(
                    "aws_cloudwatch_metric_alarm.{resource}.alarm_name"
                )),
                "`{resource}` is removed but still named in the gate's ALARM_NAMES"
            );
            continue;
        }
        let block = alarm_block(&code, resource);

        // Precondition: if a future edit makes one of these notBreaching, this
        // guard's premise no longer holds and the assertion below would be
        // enforcing a rule nobody needs. Fail loudly instead of passing quietly.
        assert!(
            block.contains(r#"treat_missing_data = "breaching""#),
            "`{resource}` no longer treats missing data as breaching. ABSENCE IS THE \
             CONDITION these alarms detect — a dropped EventBridge schedule produces no \
             invocation and therefore no error, so the sibling -errors alarm is \
             structurally blind to it. notBreaching turns a false weekend page into a \
             permanent false OK, which is the worse of the two failures."
        );

        assert!(
            list.contains(&format!(
                "aws_cloudwatch_metric_alarm.{resource}.alarm_name"
            )),
            "`{resource}` watches a MON-FRI-only Lambda schedule with \
             treat_missing_data = breaching, so it MUST appear INSIDE the market-hours \
             gate's ALARM_NAMES join() list. Without the gate its Invocations datapoint \
             is legitimately ABSENT every Saturday and Sunday and it pages — about six \
             weekend pages a month across the three, from checks that are working."
        );

        assert!(
            block.contains("actions_enabled = false"),
            "`{resource}` is gated but does not set `actions_enabled = false`. The gate \
             Lambda ENABLES actions at 09:20 IST; it cannot disarm an alarm that ships \
             armed, so the weekend pages would return between apply and the next close."
        );
    }
}

const BUDGET_GUARDS: &str = include_str!("../../../deploy/aws/terraform/budget-guards.tf");

/// The value of `key = N` inside one alarm block, if present.
fn int_setting(block: &str, key: &str) -> Option<u32> {
    block.lines().find_map(|line| {
        let t = line.trim();
        let rest = t.strip_prefix(key)?.trim_start().strip_prefix('=')?;
        rest.trim().parse().ok()
    })
}

/// 2026-11-07: the fourth MON-FRI did-it-run alarm, the daily budget digest's,
/// was missed by the 2026-09-01 gating and paged every weekend. It cannot use
/// the gate (the digest runs at 17:30 IST, so a 09:20 Monday arm would see an
/// empty 24 h and page every Monday). It must instead need at least FOUR
/// consecutive missing days: a weekend is two, plus one for a Monday run that
/// lands a minute late, plus one of margin.
#[test]
fn daily_budget_digest_not_invoked_survives_a_weekend() {
    let code = code_only(BUDGET_GUARDS);
    let block = alarm_block(&code, "tv_daily_budget_digest_not_invoked");
    assert!(
        block.contains(r#"treat_missing_data = "breaching""#),
        "absence is the condition this alarm detects; keep it breaching"
    );
    assert!(
        code.contains("cron(0 12 ? * MON-FRI *)"),
        "the digest schedule changed: re-derive the weekend arithmetic"
    );
    assert_eq!(int_setting(block, "period"), Some(86_400));
    let eval = int_setting(block, "evaluation_periods").unwrap_or(1);
    let dta = int_setting(block, "datapoints_to_alarm").unwrap_or(eval);
    assert!(
        eval >= 4 && dta == eval,
        "tv_daily_budget_digest_not_invoked needs >= 4 consecutive breaching days \
         (evaluation_periods = datapoints_to_alarm = 4); with {eval}/{dta} it pages \
         on an ordinary weekend"
    );
}

#[test]
fn the_watched_lambdas_really_are_weekday_only() {
    // The whole premise is "the schedule is MON-FRI, so the weekend absence is
    // correct behaviour". If a schedule is widened to every day the weekend
    // pages stop being false and the gate becomes a real blind spot — which
    // should be a deliberate decision, not a silent one.
    for (src, resource) in DID_IT_RUN_ALARMS {
        let code = code_only(src);
        assert!(
            code.contains("MON-FRI"),
            "`{resource}` lives in a file that no longer schedules anything MON-FRI. \
             This guard gates it on the assumption that weekend absence is CORRECT. If \
             the schedule became daily, weekend absence is a real fault and gating it \
             hides one — re-derive the decision instead of deleting this assertion."
        );
    }
}

#[test]
fn guard_self_test_block_extraction_and_comment_stripping_bite() {
    // A guard that would pass on any input proves nothing.
    let src = r#"
resource "aws_cloudwatch_metric_alarm" "alpha" {
  treat_missing_data = "notBreaching"
}
resource "aws_cloudwatch_metric_alarm" "beta" {
  treat_missing_data = "breaching"
  actions_enabled = false
}
"#;
    let alpha = alarm_block(src, "alpha");
    assert!(
        alpha.contains("notBreaching") && !alpha.contains("actions_enabled"),
        "block extraction must stop at the resource's own closing brace, or one alarm's \
         setting can be read from its neighbour"
    );
    let stripped = code_only("# actions_enabled = false\nreal = 1");
    assert!(
        !stripped.contains("actions_enabled"),
        "comment stripping must drop commented settings, or prose satisfies the scan"
    );

    // And the list extractor must be bounded by the join() list, not the file.
    let fake_gate = "ALARM_NAMES = join(\",\", [\n  a.alarm_name,\n])\nb.alarm_name";
    let list = gate_alarm_names_list(fake_gate);
    assert!(
        list.contains("a.alarm_name") && !list.contains("b.alarm_name"),
        "membership must be checked INSIDE the join() list — a reference elsewhere in \
         the file (a comment, a description, another resource) does not arm anything"
    );
}
