//! The swap rule (decision 5): a new version or snapshot goes live only before
//! the market opens, never mid-session. Pure; the clock comes in as arguments.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use super::Refusal;

/// 09:00:00 IST, seconds of day: the pre-open starts here, so no swap at or after it.
pub const SWAP_DEADLINE_SECS_OF_DAY_IST: u32 = 9 * 3600;

/// Whether a swap may happen now.
///
/// - Trading day: only strictly before 09:00:00 IST of that day.
/// - Non-trading day: any time (no session).
/// - `secs_of_day_ist` outside `0..86_400` is refused (a bad clock is never trusted).
pub fn may_swap(is_trading_day: bool, secs_of_day_ist: u32) -> Result<(), Refusal> {
    if secs_of_day_ist >= 86_400 {
        return Err(Refusal::Swap(format!(
            "clock reading {secs_of_day_ist} is not a time of day"
        )));
    }
    if !is_trading_day || secs_of_day_ist < SWAP_DEADLINE_SECS_OF_DAY_IST {
        Ok(())
    } else {
        Err(Refusal::Swap(
            "the market session has started; the new version waits for the next pre-open"
                .to_owned(),
        ))
    }
}

/// Applies a load outcome: a newly accepted version or snapshot replaces the
/// live one only when [`may_swap`] allows; otherwise the current one stays and
/// the new one waits for the next pre-open. Returns what is live and the events.
#[must_use]
pub fn apply(
    outcome: super::loader::LoadOutcome,
    current: Option<&super::loader::Accepted>,
    is_trading_day: bool,
    secs_of_day_ist: u32,
    at_ist: &str,
) -> (
    Option<super::loader::Accepted>,
    Vec<super::audit::AuditEvent>,
) {
    use super::audit::{AuditEvent, AuditKind};
    let mut events = outcome.events;
    let Some(new) = outcome.live.filter(|_| outcome.changed) else {
        return (current.cloned(), events);
    };
    let event = |kind, detail: String| AuditEvent {
        at_ist: at_ist.to_owned(),
        kind,
        strategy: new.strategy.clone(),
        version: Some(new.version),
        params_date: new.params.as_ref().map(|(d, _)| d.clone()),
        universe_date: new.universe_date.clone(),
        detail,
    };
    match may_swap(is_trading_day, secs_of_day_ist) {
        Ok(()) => {
            let from = current.map_or_else(|| "nothing".to_owned(), |c| format!("v{}", c.version));
            events.push(event(AuditKind::Swapped, format!("now live (was {from})")));
            (Some(new), events)
        }
        Err(r) => {
            events.push(event(AuditKind::SwapRefused, r.to_string()));
            (current.cloned(), events)
        }
    }
}
// O(1) EXEMPT: end
