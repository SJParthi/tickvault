//! Sub-PR #11 of 2026-05-27 daily-universe expansion — boot-time-of-day
//! guard.
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Contract (§10 of the rule file)
//!
//! From §10:
//!
//! > "**Boot-time-of-day guard:** if `now > 08:55 IST` at orchestrator
//! > start, refuse to begin a fresh-fetch cycle without explicit operator
//! > override flag (`--allow-late-boot`). Avoids mid-market universe
//! > rebuild attempts."
//!
//! Rationale: the boot sequence's hard deadline before market open is
//! 08:55 IST per §10. Booting AFTER this leaves no margin to:
//! - Fetch + parse + validate the CSV
//! - Build + persist the universe
//! - Dispatch WebSocket subscriptions
//! - Receive first ticks at 09:00 IST market open
//!
//! Rather than admit a half-built universe at 09:00, the orchestrator
//! REFUSES to start unless the operator explicitly overrides via
//! `--allow-late-boot`.
//!
//! ## What this module does NOT do
//!
//! - Read system clock (caller passes `now_ist_secs_of_day`)
//! - Parse the operator override flag from clap (caller passes a bool)
//! - Emit Telegram events (caller does, on `Err`)
//! - Wire into main.rs (Sub-PR #10b will)

#![cfg(feature = "daily_universe_fetcher")]

use thiserror::Error;

/// Hard boot deadline before market open per §10. Equals 08:55:00 IST
/// = 8 × 3600 + 55 × 60 = 32,100 seconds of day.
///
/// Booting AFTER this without `--allow-late-boot` is rejected because:
/// - Market opens at 09:00:00 IST (5 minutes later)
/// - Fetch + parse + build + subscribe takes ~3-5 minutes typically
/// - No margin for retries
pub const BOOT_HARD_DEADLINE_SECS_OF_DAY_IST: u32 = 8 * 3600 + 55 * 60;

/// Earliest allowed boot start per §10. Equals 08:30:00 IST
/// = 8 × 3600 + 30 × 60 = 30,600 seconds of day.
///
/// Per §7 the EC2 instance schedule is "08:30–17:00 IST every day".
/// Booting BEFORE 08:30 IST means the EventBridge cron fired too early
/// (clock skew on the orchestrator host) — also a halt condition
/// because the static-IP whitelist + Dhan auth tokens may not yet be
/// ready.
pub const BOOT_EARLIEST_ALLOWED_SECS_OF_DAY_IST: u32 = 8 * 3600 + 30 * 60;

/// Result of the boot-time-of-day guard check.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BootTimeOfDayError {
    /// Boot started AFTER `BOOT_HARD_DEADLINE_SECS_OF_DAY_IST` (08:55
    /// IST) without `--allow-late-boot`. Per §10 — operator must
    /// explicitly authorize a late-boot attempt OR wait for the next
    /// trading day's 08:30 IST EventBridge cron.
    #[error(
        "boot started at {now_secs_of_day_ist} IST seconds-of-day ({now_hh_mm}), past hard \
         deadline {BOOT_HARD_DEADLINE_SECS_OF_DAY_IST} (08:55 IST) — use --allow-late-boot \
         to override"
    )]
    TooLate {
        now_secs_of_day_ist: u32,
        now_hh_mm: &'static str,
    },

    /// Boot started BEFORE `BOOT_EARLIEST_ALLOWED_SECS_OF_DAY_IST` (08:30
    /// IST). Per §7 the EC2 schedule starts at 08:30 IST — earlier than
    /// that means the host clock is skewed (possibly malicious or
    /// timezone-misconfigured) and Dhan auth may not be ready.
    #[error(
        "boot started at {now_secs_of_day_ist} IST seconds-of-day, before earliest allowed \
         {BOOT_EARLIEST_ALLOWED_SECS_OF_DAY_IST} (08:30 IST) — check host clock + EC2 \
         cron schedule"
    )]
    TooEarly { now_secs_of_day_ist: u32 },
}

/// Pure guard function: returns `Ok(())` if the boot is starting within
/// the allowed window `[BOOT_EARLIEST_ALLOWED, BOOT_HARD_DEADLINE]` OR
/// the `allow_late_boot` operator-override flag is set.
///
/// # Arguments
///
/// * `now_secs_of_day_ist` — current IST seconds-of-day (computed by
///   the caller via `chrono::Utc::now().timestamp() + IST_UTC_OFFSET_SECONDS_I64`
///   then `.rem_euclid(SECONDS_PER_DAY)`)
/// * `allow_late_boot` — `true` if the operator passed
///   `--allow-late-boot` on the CLI
///
/// # Errors
///
/// See [`BootTimeOfDayError`] variants.
///
/// # Performance
///
/// COLD PATH — called once at boot. Pure integer comparison; ~1ns.
pub fn check_boot_time_of_day(
    now_secs_of_day_ist: u32,
    allow_late_boot: bool,
) -> Result<(), BootTimeOfDayError> {
    if now_secs_of_day_ist < BOOT_EARLIEST_ALLOWED_SECS_OF_DAY_IST {
        // Note: "too early" CANNOT be overridden by the operator flag.
        // The operator flag explicitly addresses LATE boots (§10
        // language: "Avoids mid-market universe rebuild attempts").
        // A too-early boot indicates clock skew — fail-closed.
        return Err(BootTimeOfDayError::TooEarly {
            now_secs_of_day_ist,
        });
    }

    if now_secs_of_day_ist > BOOT_HARD_DEADLINE_SECS_OF_DAY_IST {
        if allow_late_boot {
            return Ok(());
        }
        return Err(BootTimeOfDayError::TooLate {
            now_secs_of_day_ist,
            now_hh_mm: hh_mm_label(now_secs_of_day_ist),
        });
    }

    Ok(())
}

/// Stable label for the bucketed time-of-day in operator-facing
/// error messages. Returns a `&'static str` covering common breakpoints
/// (08:55, 09:00, 09:15, 10:00, 15:30, "post-market") so we don't
/// allocate a `String` in the error path.
#[must_use]
fn hh_mm_label(secs_of_day: u32) -> &'static str {
    const NINE_AM: u32 = 9 * 3600;
    const NINE_FIFTEEN: u32 = 9 * 3600 + 15 * 60;
    const TEN_AM: u32 = 10 * 3600;
    const FIFTEEN_THIRTY: u32 = 15 * 3600 + 30 * 60;

    if secs_of_day < NINE_AM {
        "before 09:00"
    } else if secs_of_day < NINE_FIFTEEN {
        "between 09:00 and 09:15 (pre-market)"
    } else if secs_of_day < TEN_AM {
        "between 09:15 and 10:00 (first hour of trading)"
    } else if secs_of_day < FIFTEEN_THIRTY {
        "during market hours"
    } else {
        "post-market"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: convert HH:MM IST to seconds-of-day.
    const fn hhmm(hh: u32, mm: u32) -> u32 {
        hh * 3600 + mm * 60
    }

    #[test]
    fn deadline_constants_match_rule_file_section_10() {
        // 08:55 IST = 32,100 seconds of day.
        assert_eq!(BOOT_HARD_DEADLINE_SECS_OF_DAY_IST, 32_100);
        // 08:30 IST = 30,600 seconds of day.
        assert_eq!(BOOT_EARLIEST_ALLOWED_SECS_OF_DAY_IST, 30_600);
    }

    #[test]
    fn earliest_allowed_before_hard_deadline() {
        // Defensive sanity — the allowed window must be non-degenerate.
        assert!(BOOT_EARLIEST_ALLOWED_SECS_OF_DAY_IST < BOOT_HARD_DEADLINE_SECS_OF_DAY_IST);
    }

    #[test]
    fn accepts_boot_at_08_30_exactly() {
        // Edge case — boot started exactly at 08:30 IST.
        assert!(check_boot_time_of_day(hhmm(8, 30), false).is_ok());
    }

    #[test]
    fn accepts_boot_at_08_45() {
        // Typical case — boot started at 08:45 IST (15 min before market).
        assert!(check_boot_time_of_day(hhmm(8, 45), false).is_ok());
    }

    #[test]
    fn accepts_boot_at_08_55_exactly() {
        // Edge case — boot started exactly at the hard deadline.
        assert!(check_boot_time_of_day(hhmm(8, 55), false).is_ok());
    }

    #[test]
    fn rejects_boot_at_08_56_without_override() {
        // 1 second past deadline (08:55:00 IST = 32,100; 08:55:01 is +1).
        let now = BOOT_HARD_DEADLINE_SECS_OF_DAY_IST + 1;
        let err = check_boot_time_of_day(now, false).unwrap_err();
        assert!(matches!(err, BootTimeOfDayError::TooLate { .. }));
    }

    #[test]
    fn allows_late_boot_with_operator_override() {
        // Booting at 10:00 IST (well into market) with --allow-late-boot.
        let now = hhmm(10, 0);
        assert!(check_boot_time_of_day(now, true).is_ok());
    }

    #[test]
    fn rejects_late_boot_without_override_even_mid_market() {
        // Booting at 12:00 IST without the flag → reject.
        let now = hhmm(12, 0);
        let err = check_boot_time_of_day(now, false).unwrap_err();
        assert!(matches!(err, BootTimeOfDayError::TooLate { .. }));
    }

    #[test]
    fn rejects_boot_at_08_29_too_early() {
        // 1 minute before earliest allowed.
        let now = hhmm(8, 29);
        let err = check_boot_time_of_day(now, false).unwrap_err();
        assert!(matches!(err, BootTimeOfDayError::TooEarly { .. }));
    }

    #[test]
    fn too_early_cannot_be_overridden_by_operator_flag() {
        // Per §10 the override flag addresses LATE boots only. A
        // too-early boot indicates clock skew (potentially malicious)
        // and must NOT be overridable.
        let now = hhmm(7, 0);
        let err = check_boot_time_of_day(now, true).unwrap_err();
        assert!(matches!(err, BootTimeOfDayError::TooEarly { .. }));
    }

    #[test]
    fn rejects_boot_at_midnight() {
        // Edge case — boot at 00:00 IST (off-hours).
        let err = check_boot_time_of_day(0, false).unwrap_err();
        assert!(matches!(err, BootTimeOfDayError::TooEarly { .. }));
    }

    #[test]
    fn rejects_boot_at_23_59_post_market() {
        // Late evening boot — past hard deadline; even with override
        // this is fine via the operator flag, but without it should
        // fail.
        let now = hhmm(23, 59);
        let err = check_boot_time_of_day(now, false).unwrap_err();
        assert!(matches!(err, BootTimeOfDayError::TooLate { .. }));
    }

    #[test]
    fn allows_late_evening_boot_with_override() {
        // 23:59 IST with --allow-late-boot (would be unusual but
        // logically the flag overrides any too-late condition).
        let now = hhmm(23, 59);
        assert!(check_boot_time_of_day(now, true).is_ok());
    }

    #[test]
    fn error_display_includes_diagnostic_context() {
        // The TooLate error embeds the wall-clock time (for operator
        // triage) + the hard deadline.
        let now = hhmm(10, 0);
        let err = check_boot_time_of_day(now, false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("36000") || msg.contains("08:55") || msg.contains("--allow-late-boot"),
            "error message must include actionable context: {msg}"
        );
    }

    #[test]
    fn hh_mm_label_buckets_cover_full_day() {
        // Defensive — every second of the day maps to a non-empty
        // label.
        assert_eq!(hh_mm_label(0), "before 09:00");
        assert_eq!(hh_mm_label(hhmm(8, 30)), "before 09:00");
        assert_eq!(
            hh_mm_label(hhmm(9, 0)),
            "between 09:00 and 09:15 (pre-market)"
        );
        assert_eq!(
            hh_mm_label(hhmm(9, 15)),
            "between 09:15 and 10:00 (first hour of trading)"
        );
        assert_eq!(hh_mm_label(hhmm(12, 0)), "during market hours");
        assert_eq!(hh_mm_label(hhmm(15, 30)), "post-market");
        assert_eq!(hh_mm_label(hhmm(23, 59)), "post-market");
    }

    #[test]
    fn deterministic_pure_function() {
        // Same inputs → same output, every time.
        for hh in 0u32..24 {
            for mm in (0u32..60).step_by(15) {
                let now = hhmm(hh, mm);
                let r1 = check_boot_time_of_day(now, false);
                let r2 = check_boot_time_of_day(now, false);
                assert_eq!(r1.is_ok(), r2.is_ok());
            }
        }
    }
}
