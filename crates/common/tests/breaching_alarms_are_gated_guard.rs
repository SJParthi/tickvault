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

/// The deaf-socket alarm needs the gate too, and the guard above cannot see it.
///
/// Everything above checks `breaching` alarms, on the reasoning that absence
/// must not page overnight. `dhan_worst_socket_deaf` is `notBreaching`, so it
/// is invisible to that scan — and it needs the gate just as badly, for a
/// different and MORE frequent reason.
///
/// Its gauge is a per-socket age that only climbs. After the 15:30 close every
/// socket legitimately stops delivering, so the age crosses its 600 s
/// threshold within ten minutes and the alarm fires on a market that is simply
/// shut — **every single trading day, at about 15:40**. The breaching alarms
/// misfire nightly at shutdown; this one would misfire two hours earlier, in
/// the middle of a working evening, which is if anything more corrosive.
///
/// The general rule ("any alarm whose metric legitimately climbs after close
/// must be gated") is not mechanically decidable — nothing in terraform says
/// which gauges do that. So this alarm is pinned by name, with the reason
/// written down, rather than left to a scan that cannot know.
#[test]
fn the_deaf_socket_alarm_is_gated_even_though_it_is_not_breaching() {
    let gate = code_only(GATE);
    let names_at = gate
        .find("ALARM_NAMES")
        .expect("the gate Lambda must still take an ALARM_NAMES env list");
    let tail = &gate[names_at..];
    let block_end = tail.find("])").expect("ALARM_NAMES must be a join() list");
    let list = &tail[..block_end];

    assert!(
        list.contains("dhan_worst_socket_deaf"),
        "`dhan_worst_socket_deaf` must appear INSIDE the gate's ALARM_NAMES \
         join() list. It is notBreaching, so the scan above cannot catch this \
         omission — and without the gate it pages EVERY TRADING DAY at ~15:40, \
         when every socket legitimately stops delivering after the close. \
         Found list:\n{list}"
    );

    // And it must ship disarmed, or the gate has nothing to enable.
    let lane = code_only(LIVE_LANE);
    let at = lane
        .find(r#"resource "aws_cloudwatch_metric_alarm" "dhan_worst_socket_deaf""#)
        .expect("the deaf-socket alarm must exist");
    let body = &lane[at..];
    let body = &body[..body.find("\n}").unwrap_or(body.len())];
    assert!(
        body.contains("actions_enabled = false"),
        "the deaf-socket alarm must ship with actions OFF — the gate ENABLES \
         actions during the session and cannot disarm an alarm that ships armed"
    );
    // The threshold is inherited from the lane-level alarm, not invented. If
    // someone lowers it, that is a decision needing a measured baseline.
    assert!(
        body.contains("threshold           = 600") || body.contains("threshold = 600"),
        "the deaf-socket threshold must stay 600 s — the same ten minutes the \
         lane-level no-ticks alarm uses (300 s x 2 periods), so it is inherited \
         rather than invented. Lowering it needs a measured baseline"
    );
}

/// Every member of the gate's `ALARM_NAMES` join, parsed from COMMENT-STRIPPED
/// source so a commented-out member line cannot count.
fn gate_members(gate_src: &str) -> Vec<String> {
    let gate = code_only(gate_src);
    let names_at = gate
        .find("ALARM_NAMES")
        .expect("the gate Lambda must still take an ALARM_NAMES env list");
    let tail = &gate[names_at..];
    let block_end = tail.find("])").expect("ALARM_NAMES must be a join() list");
    tail[..block_end]
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("aws_cloudwatch_metric_alarm.")
                .and_then(|rest| rest.split(".alarm_name").next())
                .map(str::to_owned)
        })
        .filter(|n| !n.is_empty())
        .collect()
}

/// True iff a code line of the block sets `actions_enabled` to `false`,
/// tolerant of terraform-fmt alignment padding around the `=`.
fn declares_disarmed(block: &str) -> bool {
    block.lines().any(|l| {
        let mut parts = l.splitn(2, '=');
        matches!(
            (parts.next().map(str::trim), parts.next().map(str::trim)),
            (Some("actions_enabled"), Some("false"))
        )
    })
}

/// The gate members whose resource block does NOT ship `actions_enabled = false`.
/// A member whose resource cannot be found at all is reported too — a list
/// entry pointing at nothing is a broken arming path, never a pass.
fn members_not_shipped_disarmed(gate_src: &str, tf_sources: &[String]) -> Vec<String> {
    let stripped: Vec<String> = tf_sources.iter().map(|s| code_only(s)).collect();
    gate_members(gate_src)
        .into_iter()
        .filter(|name| {
            let needle = format!("resource \"aws_cloudwatch_metric_alarm\" \"{name}\"");
            match stripped.iter().find(|s| s.contains(&needle)) {
                Some(src) => !declares_disarmed(alarm_block(src, name)),
                None => true,
            }
        })
        .collect()
}

fn all_terraform_sources() -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "tf") {
                out.push(
                    std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())),
                );
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/terraform");
    let mut out = Vec::new();
    walk(&root, &mut out);
    out
}

/// WIDENED 2026-09-23 (dhan-rest-only-noise-lock-2026-07-14.md §2.3x).
///
/// The scan above checks only BREACHING alarms, and only in one file. But the
/// gate owns the arming of EVERY alarm in its list, breaching or not: it
/// enables actions at 09:20 IST and disables them at close. Terraform's
/// default for `actions_enabled` is TRUE, so any member that omits the line
/// is re-armed by every apply and pages outside the window the gate exists to
/// keep quiet. Three members did exactly that on 2026-09-23
/// (`tick_spill_replay_failing`, `ticks_spilling`,
/// `aggregator_refusal_rate_high`), and the one that paged that morning,
/// `ws_no_alive_connections`, was not in the list at all.
///
/// This derives the membership from the gate itself, so a future member is
/// covered without anyone remembering to add it here.
#[test]
fn every_gate_listed_alarm_ships_disarmed() {
    let members = gate_members(GATE);
    assert!(
        members.len() >= 10,
        "expected at least ten gate members, parsed {} ({members:?}). A broken parser \
         reporting zero members would make this guard pass vacuously.",
        members.len()
    );
    let offenders = members_not_shipped_disarmed(GATE, &all_terraform_sources());
    assert!(
        offenders.is_empty(),
        "these market-hours-gated alarms do not ship `actions_enabled = false` (or their \
         resource was not found): {offenders:?}. The gate Lambda only ENABLES actions for \
         the session; an alarm that ships armed is re-armed by every terraform apply and \
         pages all night and all weekend."
    );
}

#[test]
fn the_disarmed_gate_scan_bites_on_an_armed_member() {
    // Anti-vacuity: an armed member, a member with no resource, and a
    // commented-out member must be handled correctly.
    let gate = "ALARM_NAMES = join(\",\", [\n\
                aws_cloudwatch_metric_alarm.ok_one.alarm_name,\n\
                aws_cloudwatch_metric_alarm.armed_one.alarm_name,\n\
                aws_cloudwatch_metric_alarm.missing_one.alarm_name,\n\
                # aws_cloudwatch_metric_alarm.commented_one.alarm_name,\n\
                ])";
    let tf = vec![String::from(
        "resource \"aws_cloudwatch_metric_alarm\" \"ok_one\" {\n  actions_enabled   = false\n}\n\
         resource \"aws_cloudwatch_metric_alarm\" \"armed_one\" {\n  # actions_enabled = false\n  alarm_actions = []\n}\n",
    )];
    assert_eq!(
        gate_members(gate),
        vec!["ok_one", "armed_one", "missing_one"],
        "a commented-out member must never count"
    );
    assert_eq!(
        members_not_shipped_disarmed(gate, &tf),
        vec!["armed_one", "missing_one"],
        "an armed member (even one whose disarm line is only a COMMENT) and a member \
         with no resource must both be reported"
    );
}
