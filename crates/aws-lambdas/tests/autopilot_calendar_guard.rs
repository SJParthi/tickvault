//! Build-failing ratchet (2026-11-07): `scripts/aws-autopilot.sh` must never
//! start the box on an NSE holiday because the holiday-stop marker is missing
//! or unreadable. It reads the calendar itself (`config/base.toml`, the app's
//! one source) and fails closed after 09:00 IST when the calendar cannot be
//! read. The autopilot runs on the GitHub runner, which has the repository
//! checked out, so the file is there.

#![cfg(test)]

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The autopilot reads the holiday as the line right after each
/// `[[trading.nse_holidays]]` header (`grep -A1`), so every header must be
/// followed directly by its `date = "YYYY-MM-DD"` line.
#[test]
fn every_holiday_header_is_followed_by_its_date_line() {
    let toml = read("config/base.toml");
    let lines: Vec<&str> = toml.lines().collect();
    let mut headers = 0;
    for (i, line) in lines.iter().enumerate() {
        if *line == "[[trading.nse_holidays]]" {
            headers += 1;
            let next = lines.get(i + 1).copied().unwrap_or("");
            assert!(
                next.starts_with("date = \"") && next.len() == "date = \"2026-01-01\"".len(),
                "line {} after a holiday header must be `date = \"YYYY-MM-DD\"`, got `{next}`",
                i + 2
            );
        }
    }
    assert!(
        headers >= 10,
        "config/base.toml must still list the NSE holidays"
    );
}

/// Regression (2026-11-07): autopilot started the box on a weekday holiday
/// whenever the holiday-stop marker was missing. It must read the calendar
/// itself, refuse a holiday, and refuse after 09:00 IST when the calendar is
/// unreadable, all BEFORE its start-instances call.
#[test]
fn autopilot_reads_the_calendar_before_its_self_start() {
    let sh = read("scripts/aws-autopilot.sh");
    let pos = |needle: &str| {
        sh.find(needle)
            .unwrap_or_else(|| panic!("aws-autopilot.sh lost `{needle}`"))
    };
    let cal = pos("CAL=config/base.toml;");
    let holiday = pos(
        "elif grep -A1 -xF '[[trading.nse_holidays]]' \"$CAL\" 2>/dev/null | grep -qxF \"date = \\\"$TODAY_IST\\\"\"; then",
    );
    let unreadable = pos(
        "elif ! grep -qxF '[[trading.nse_holidays]]' \"$CAL\" 2>/dev/null && [ \"$((10#$NOW_HM))\" -ge 900 ]; then",
    );
    let start = pos("aws ec2 start-instances --region \"$REGION\" --instance-ids \"$INSTANCE_ID\"");
    assert!(cal < holiday && holiday < unreadable && unreadable < start);
    // The holiday branch must not start the box: it reports and moves on.
    assert!(
        !sh[holiday..unreadable].contains("start-instances"),
        "the holiday branch must not start the box"
    );
}
