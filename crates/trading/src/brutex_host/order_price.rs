//! The marketable LIMIT price for a paper or live order.
//!
//! An order is sent as a LIMIT that the market can fill at once, never as a
//! bare MARKET order:
//!
//! * Buy: `min(ltp × (1 + band), slot × 5 / qty when a slot is given, the upper
//!   circuit when given)`, rounded DOWN to the tick grid.
//! * Sell: `max(ltp × (1 − band), the lower circuit when given)`, rounded UP to
//!   the tick grid. The slot cap applies to buys only.
//!
//! When rounding leaves the limit on the wrong side of the last traded price
//! (a buy limit below LTP, a sell limit above it) the order would rest instead
//! of filling, so it is refused. [`LimitPrice::bound`] names the bound that set
//! the price; ties name the hardest bound (Circuit, then SlotCap, then Band).
//!
//! Integer paise throughout; products run in `i128`. O(1), no allocation.

use super::engine::Side;
use super::worst_fill::{BPS_DENOMINATOR, MAX_PRICE_PAISE, MAX_QTY};

/// The default band: 10% (1,000 basis points).
pub const DEFAULT_BAND_BPS: u32 = 1_000;

/// Which bound set the limit price (what gets logged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitBound {
    Band,
    SlotCap,
    Circuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitPrice {
    pub price_paise: i64,
    pub bound: LimitBound,
}

/// Why no marketable limit could be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PriceRefusal {
    #[error("last traded price is zero, negative or above the limit")]
    LtpOutOfRange,
    #[error("band must be 1..=9999 basis points")]
    BadBand,
    #[error("tick size is zero, negative or above the price limit")]
    BadTick,
    #[error("quantity is zero or above the share limit")]
    BadQty,
    #[error("slot amount is zero or negative")]
    BadSlot,
    #[error("circuit limits are invalid")]
    BadCircuit,
    #[error("last traded price is outside the circuit limits")]
    LtpOutsideCircuit,
    #[error("limit {limit_paise} paise would not fill against LTP {ltp_paise} paise")]
    NotMarketable { limit_paise: i64, ltp_paise: i64 },
}

#[inline]
const fn price_ok(p: i64) -> bool {
    p > 0 && p <= MAX_PRICE_PAISE
}

/// Builds the marketable limit. O(1), no allocation.
pub fn marketable_limit(
    side: Side,
    ltp_paise: i64,
    band_bps: u32,
    tick_paise: i64,
    qty: u64,
    slot_paise: Option<i64>,
    circuit: Option<(i64, i64)>,
) -> Result<LimitPrice, PriceRefusal> {
    if !price_ok(ltp_paise) {
        return Err(PriceRefusal::LtpOutOfRange);
    }
    if band_bps == 0 || band_bps >= BPS_DENOMINATOR {
        return Err(PriceRefusal::BadBand);
    }
    if !price_ok(tick_paise) {
        return Err(PriceRefusal::BadTick);
    }
    if qty == 0 || qty > MAX_QTY {
        return Err(PriceRefusal::BadQty);
    }
    if let Some(slot) = slot_paise
        && slot <= 0
    {
        return Err(PriceRefusal::BadSlot);
    }
    if let Some((lower, upper)) = circuit {
        if !price_ok(lower) || !price_ok(upper) || lower > upper {
            return Err(PriceRefusal::BadCircuit);
        }
        if ltp_paise < lower || ltp_paise > upper {
            return Err(PriceRefusal::LtpOutsideCircuit);
        }
    }
    let ltp = i128::from(ltp_paise);
    let den = i128::from(BPS_DENOMINATOR);
    let tick = i128::from(tick_paise);
    let (raw, bound) = match side {
        Side::Buy => {
            let mut best = (ltp * (den + i128::from(band_bps)) / den, LimitBound::Band);
            if let Some(slot) = slot_paise {
                let cap = i128::from(slot) * 5 / i128::from(qty);
                if cap <= best.0 {
                    best = (cap, LimitBound::SlotCap);
                }
            }
            if let Some((_, upper)) = circuit {
                let upper = i128::from(upper);
                if upper <= best.0 {
                    best = (upper, LimitBound::Circuit);
                }
            }
            (best.0 - best.0.rem_euclid(tick), best.1)
        }
        Side::Sell => {
            let num = ltp * (den - i128::from(band_bps));
            let mut best = ((num + den - 1) / den, LimitBound::Band);
            if let Some((lower, _)) = circuit {
                let lower = i128::from(lower);
                if lower >= best.0 {
                    best = (lower, LimitBound::Circuit);
                }
            }
            let r = best.0.rem_euclid(tick);
            let up = if r == 0 { best.0 } else { best.0 + (tick - r) };
            (up, best.1)
        }
    };
    let marketable = match side {
        Side::Buy => raw >= ltp,
        Side::Sell => raw <= ltp,
    };
    // Every bound is at most 2 × MAX_PRICE_PAISE, so this conversion only
    // fails for a value the marketable check already refuses.
    let limit_paise = i64::try_from(raw).unwrap_or(if raw < 0 { i64::MIN } else { i64::MAX });
    if !marketable || limit_paise <= 0 {
        return Err(PriceRefusal::NotMarketable {
            limit_paise,
            ltp_paise,
        });
    }
    Ok(LimitPrice {
        price_paise: limit_paise,
        bound,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy(
        ltp: i64,
        slot: Option<i64>,
        circuit: Option<(i64, i64)>,
    ) -> Result<LimitPrice, PriceRefusal> {
        marketable_limit(Side::Buy, ltp, DEFAULT_BAND_BPS, 5, 50, slot, circuit)
    }

    fn sell(ltp: i64, circuit: Option<(i64, i64)>) -> Result<LimitPrice, PriceRefusal> {
        marketable_limit(Side::Sell, ltp, DEFAULT_BAND_BPS, 5, 50, None, circuit)
    }

    #[test]
    fn buy_band_rounds_down() {
        // 10003 × 1.1 = 11003.3 -> floor 11003 -> tick 11000.
        assert_eq!(
            buy(10_003, None, None),
            Ok(LimitPrice {
                price_paise: 11_000,
                bound: LimitBound::Band
            })
        );
    }

    #[test]
    fn sell_band_rounds_up() {
        // 10003 × 0.9 = 9002.7 -> ceil 9003 -> tick 9005.
        assert_eq!(
            sell(10_003, None),
            Ok(LimitPrice {
                price_paise: 9_005,
                bound: LimitBound::Band
            })
        );
    }

    #[test]
    fn slot_cap_wins() {
        // slot 105000 × 5 / 50 = 10500, below the band's 11000.
        assert_eq!(
            buy(10_000, Some(105_000), None),
            Ok(LimitPrice {
                price_paise: 10_500,
                bound: LimitBound::SlotCap
            })
        );
        // A slot cap above the band does not win.
        assert_eq!(
            buy(10_000, Some(10_000_000), None).map(|l| l.bound),
            Ok(LimitBound::Band)
        );
    }

    #[test]
    fn circuit_wins_and_ties_name_circuit() {
        assert_eq!(
            buy(10_000, None, Some((9_000, 10_502))),
            Ok(LimitPrice {
                price_paise: 10_500,
                bound: LimitBound::Circuit
            })
        );
        assert_eq!(
            buy(10_000, None, Some((9_000, 11_000))).map(|l| l.bound),
            Ok(LimitBound::Circuit)
        );
        assert_eq!(
            sell(10_000, Some((9_501, 11_000))),
            Ok(LimitPrice {
                price_paise: 9_505,
                bound: LimitBound::Circuit
            })
        );
    }

    #[test]
    fn not_marketable_refused() {
        // Slot cap below LTP.
        assert_eq!(
            buy(10_000, Some(50_000), None),
            Err(PriceRefusal::NotMarketable {
                limit_paise: 5_000,
                ltp_paise: 10_000
            })
        );
        // LTP off the grid at the upper circuit: rounding down goes below LTP.
        assert!(matches!(
            buy(10_002, None, Some((9_000, 10_002))),
            Err(PriceRefusal::NotMarketable { .. })
        ));
        assert!(matches!(
            sell(10_002, Some((10_002, 11_000))),
            Err(PriceRefusal::NotMarketable { .. })
        ));
    }

    #[test]
    fn bad_inputs_refused() {
        assert_eq!(buy(0, None, None), Err(PriceRefusal::LtpOutOfRange));
        assert_eq!(
            buy(MAX_PRICE_PAISE + 1, None, None),
            Err(PriceRefusal::LtpOutOfRange)
        );
        assert_eq!(
            marketable_limit(Side::Buy, 100, 0, 5, 1, None, None),
            Err(PriceRefusal::BadBand)
        );
        assert_eq!(
            marketable_limit(Side::Sell, 100, 10_000, 5, 1, None, None),
            Err(PriceRefusal::BadBand)
        );
        assert_eq!(
            marketable_limit(Side::Buy, 100, 1_000, 0, 1, None, None),
            Err(PriceRefusal::BadTick)
        );
        assert_eq!(
            marketable_limit(Side::Buy, 100, 1_000, 5, 0, None, None),
            Err(PriceRefusal::BadQty)
        );
        assert_eq!(buy(100, Some(0), None), Err(PriceRefusal::BadSlot));
        assert_eq!(
            buy(100, None, Some((200, 100))),
            Err(PriceRefusal::BadCircuit)
        );
        assert_eq!(
            buy(100, None, Some((0, 100))),
            Err(PriceRefusal::BadCircuit)
        );
        assert_eq!(
            buy(300, None, Some((100, 200))),
            Err(PriceRefusal::LtpOutsideCircuit)
        );
    }

    #[test]
    fn extreme_prices_do_not_overflow() {
        let l = buy(MAX_PRICE_PAISE, Some(i64::MAX), None);
        assert_eq!(l.map(|x| x.price_paise % 5), Ok(0));
        let l = sell(MAX_PRICE_PAISE, None);
        assert!(l.is_ok_and(|x| x.price_paise <= MAX_PRICE_PAISE));
        // A one-paisa sell rounds up to the 5-paise tick, above LTP: refused.
        assert!(matches!(
            sell(1, None),
            Err(PriceRefusal::NotMarketable { .. })
        ));
    }
}
