//! Picking the newest version folder and the newest usable snapshot from key or
//! folder names. Pure; O(names) once per load, cold.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
/// The newest `v<N>` among names, the versions skipped below it (gaps are
/// allowed and logged, never refused), and every name that looked like a
/// version but was malformed (`v01`, `v0`, `v-1`, `vx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionPick {
    pub newest: Option<u32>,
    pub all: Vec<u32>,
    pub gaps: Vec<u32>,
    pub malformed: Vec<String>,
}

/// Parses `v<N>` (N >= 1, no leading zero, no sign) folder names.
#[must_use]
pub fn parse_version(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('v')?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|n| *n >= 1)
}

#[must_use]
pub fn pick_version<'a>(names: impl IntoIterator<Item = &'a str>) -> VersionPick {
    let mut all = Vec::new();
    let mut malformed = Vec::new();
    for n in names {
        match parse_version(n) {
            Some(v) => all.push(v),
            None if n.starts_with('v') => malformed.push(n.to_owned()),
            None => {}
        }
    }
    all.sort_unstable();
    all.dedup();
    let newest = all.last().copied();
    let mut gaps = Vec::new();
    if let Some(max) = newest {
        let mut next = 1u32;
        for &v in &all {
            while next < v {
                gaps.push(next);
                next += 1;
            }
            next = v.saturating_add(1);
        }
        debug_assert!(gaps.iter().all(|g| *g < max));
    }
    VersionPick {
        newest,
        all,
        gaps,
        malformed,
    }
}

/// Snapshot dates (`YYYY-MM-DD`) on or before the trading day, newest first.
/// A date after the trading day is ignored (published early); a malformed name
/// is ignored and returned separately.
#[must_use]
pub fn usable_snapshot_dates<'a>(
    names: impl IntoIterator<Item = &'a str>,
    trading_day: &str,
) -> (Vec<String>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for n in names {
        if is_iso_date(n) {
            if n <= trading_day {
                ok.push(n.to_owned());
            }
        } else {
            bad.push(n.to_owned());
        }
    }
    ok.sort_unstable_by(|a, b| b.cmp(a));
    ok.dedup();
    (ok, bad)
}

/// `YYYY-MM-DD` with a real month and day (lexical order = date order).
#[must_use]
pub fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}
// O(1) EXEMPT: end
