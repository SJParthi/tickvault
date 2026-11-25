//! Pins that the risk engine's mark-price refusal is COUNTED and LOGGED.
//!
//! # Why this one matters more than an ordinary refusal
//!
//! `update_market_price_in_segment` feeds `market_prices`, which feeds
//! `daily_loss_state`, which feeds the daily-loss auto-halt — the kill switch.
//! The refusal itself is correct and must stay: a NaN mark folded into the
//! unrealised sum poisons the whole portfolio total, not just one leg.
//!
//! What was wrong is that it was a bare `return`. A feed emitting NaN for one
//! instrument froze that leg's mark at its last good value, the halt kept
//! evaluating against a price that had stopped tracking the market, and
//! nothing anywhere said so. The position still shows a P&L. The dashboard
//! still shows a number. It is simply the wrong number, indefinitely.
//!
//! Freezing beats poisoning, so the behaviour is unchanged — only its
//! visibility. This guard exists because "visible" is exactly the property
//! that a later refactor removes without noticing: deleting a counter looks
//! like tidying, and the code still does the right thing afterwards.

const ENGINE_SRC: &str = include_str!("../src/risk/engine.rs");

fn production_half() -> &'static str {
    ENGINE_SRC
        .split("\nmod tests {")
        .next()
        .expect("splitting on the test module always yields a first half")
}

#[test]
fn the_mark_refusal_increments_a_counter() {
    let src = production_half();
    assert!(
        src.contains("metrics::counter!(MARK_REJECTED_COUNTER"),
        "the mark-price refusal must increment `MARK_REJECTED_COUNTER`. \
         Without it a feed emitting NaN freezes a leg's mark silently and the \
         daily-loss auto-halt keeps deciding on a price that stopped tracking \
         the market."
    );
}

#[test]
fn the_mark_refusal_logs_with_a_coded_error() {
    let src = production_half();
    assert!(
        src.contains("code = ErrorCode::RiskGapPositionPnl.code_str()"),
        "the refusal must carry the RISK-GAP-02 code so coded-error triage and \
         the errcode log filters can see it. An uncoded line is invisible to \
         every automated path in this repo."
    );
}

#[test]
fn the_mark_refusal_log_is_throttled_so_a_nan_storm_cannot_flood_the_sink() {
    let src = production_half();
    assert!(
        src.contains("self.marks_rejected.is_power_of_two()"),
        "the refusal log must be power-of-two throttled. A poisoned feed emits \
         NaN on EVERY tick, so an unthrottled line here is a log flood at tick \
         rate — which costs money, buries every other signal, and is the \
         reason the original author chose a bare `return` in the first place."
    );
}

#[test]
fn the_refusal_counter_resets_daily_so_a_new_episode_reports_from_one() {
    let src = production_half();
    let reset = src
        .split("pub fn reset_daily(&mut self)")
        .nth(1)
        .expect("reset_daily must exist");
    let body = &reset[..reset.find("\n    }").unwrap_or(reset.len())];
    assert!(
        body.contains("self.marks_rejected = 0;"),
        "`reset_daily` must clear the refusal count. Carried across days, the \
         power-of-two stride from a bad session would suppress the first \
         several hundred refusals of the next one — a throttle that grows into \
         a mute."
    );
}
