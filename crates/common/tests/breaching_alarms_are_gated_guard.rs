//! Guard: any alarm that treats MISSING DATA as breaching must be inside the
//! market-hours gate — otherwise it pages every night the box is off.
//!
//! # The two-sided trap this pins
//!
//! A gauge alarm on `LessThanThreshold` can only fire on a datapoint that
//! EXISTS. A crashed process publishes nothing, so with
//! `treat_missing_data = "notBreaching"` its silence reads as health, and the
//! lane-down alarm is blind to the exact total-failure case it exists for.
//! Live proof, 2026-08-18: the alarm sat in OK with its own state reason
//! reading *"no datapoints were received for 2 periods and 2 missing
//! datapoints were treated as [NonBreaching]"*.
//!
//! The obvious repair — flip it to `breaching` — is a REGRESSION on its own.
//! The box is stopped outside the trading window by design, so a bare flip
//! pages every single evening at shutdown. The comment that was in the
//! terraform said exactly that, and it was right:
//!
//! > the fastest possible way to train an operator to ignore this alarm
//!
//! Both halves are therefore one change: `breaching` is safe ONLY while the
//! market-hours gate Lambda disables the alarm's ACTIONS outside the session.
//! This guard exists because the two halves live in two different files, and
//! a future edit that removes the gate membership while leaving `breaching`
//! in place would reintroduce nightly false pages — the failure mode that
//! trains operators to ignore real ones.

const LIVE_LANE: &str = include_str!("../../../deploy/aws/terraform/live-lane-alarms.tf");
const GATE: &str = include_str!("../../../deploy/aws/terraform/market-hours-liveness-alarm.tf");

/// Strip `#` comments so prose naming a resource cannot satisfy a scan meant
/// to find a real terraform reference.
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
/// block, bounded at its matching brace.
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

#[test]
fn the_lane_down_alarm_can_actually_see_a_dead_process() {
    let code = code_only(LIVE_LANE);
    let block = alarm_block(&code, "dhan_live_lane_down");
    assert!(
        block.contains(r#"treat_missing_data = "breaching""#),
        "tv-<env>-dhan-live-lane-down must treat missing data as BREACHING. A crashed app \
         publishes no datapoint at all, so notBreaching makes the lane-down alarm blind to \
         the total-failure case it exists for — it reads OK while the lane is gone."
    );
}

#[test]
fn a_breaching_alarm_is_disarmed_outside_the_trading_window() {
    let gate = code_only(GATE);
    let lane = code_only(LIVE_LANE);

    // GENERALISED 2026-08-21. This used to name `dhan_live_lane_down` and only
    // that alarm, so the second breaching alarm to arrive
    // (`dhan_no_ticks_flowing`) would have been ungated and silently paged
    // every evening — the guard would have passed while the thing it exists to
    // prevent happened. It now DERIVES the set from the terraform, so it bites
    // for every future breaching alarm without anyone remembering to add it.
    let mut checked = 0usize;
    for block in lane
        .split("resource \"aws_cloudwatch_metric_alarm\" \"")
        .skip(1)
    {
        let Some(name_end) = block.find('"') else {
            continue;
        };
        let name = &block[..name_end];
        // Bound the scan to this resource block, so a later block's
        // `breaching` cannot be attributed to this alarm.
        let body_end = block.find("\n}").unwrap_or(block.len());
        let body = &block[..body_end];
        if !body.contains(r#"treat_missing_data = "breaching""#) {
            continue;
        }
        checked += 1;

        assert!(
            gate.contains(&format!("aws_cloudwatch_metric_alarm.{name}.alarm_name")),
            "`{name}` treats missing data as breaching, so it MUST appear in the \
             market-hours gate's ALARM_NAMES list. Without the gate it pages every evening \
             the box shuts down and all weekend — which is worse than the blindness it was \
             meant to fix, because it teaches the operator to ignore the alarm."
        );
        assert!(
            body.contains("actions_enabled = false"),
            "`{name}` is breaching and gated, but does not set `actions_enabled = false`. \
             The gate Lambda ENABLES actions during the session; it cannot disarm an alarm \
             that ships armed, so the nightly page would return."
        );
    }

    assert!(
        checked >= 2,
        "expected at least two breaching live-lane alarms to check, found {checked}. \
         Either the extraction broke or a breaching alarm was removed — both need a look, \
         because a vacuous pass here is exactly the false-OK this guard exists to stop."
    );
}

#[test]
fn the_gate_list_is_the_authoritative_arming_path() {
    let gate = code_only(GATE);
    let names_at = gate
        .find("ALARM_NAMES")
        .expect("the gate Lambda must still take an ALARM_NAMES env list");
    let tail = &gate[names_at..];
    let block_end = tail.find("])").expect("ALARM_NAMES must be a join() list");
    let list = &tail[..block_end];
    assert!(
        list.contains("dhan_live_lane_down"),
        "dhan_live_lane_down must appear INSIDE the ALARM_NAMES join() list, not merely \
         somewhere in the file. Found the reference outside the list:\n{list}"
    );
}

#[test]
fn guard_self_test_block_extraction_is_not_vacuous() {
    // A guard that would pass on any input proves nothing.
    let src = r#"
resource "aws_cloudwatch_metric_alarm" "alpha" {
  treat_missing_data = "notBreaching"
}
resource "aws_cloudwatch_metric_alarm" "beta" {
  treat_missing_data = "breaching"
}
"#;
    let alpha = alarm_block(src, "alpha");
    assert!(
        alpha.contains("notBreaching") && !alpha.contains(r#"= "breaching""#),
        "block extraction must stop at the resource's own closing brace, or one alarm's \
         setting can be read from its neighbour"
    );
    let stripped = code_only("# treat_missing_data = \"breaching\"\nreal = 1");
    assert!(
        !stripped.contains("breaching"),
        "comment stripping must drop commented settings, or prose satisfies the scan"
    );
}
