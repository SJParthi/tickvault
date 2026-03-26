//! Day-over-day instrument universe delta detection.
//!
//! Compares today's `FnoUniverse` with yesterday's (loaded from rkyv cache)
//! and produces `LifecycleEvent`s for every change: contracts added, expired,
//! fields modified, underlyings added or removed.
//!
//! # Performance
//! O(n) where n = max(yesterday_count, today_count). Runs ONLY at build time,
//! outside market hours. Never on the hot path.
//!
//! # First Run
//! If yesterday's universe is `None` (first-ever run or cache missing),
//! returns an empty event list — all contracts are treated as "initial load".

use tracing::info;

use dhan_live_trader_common::instrument_types::FnoUniverse;

use dhan_live_trader_storage::instrument_persistence::{LifecycleEvent, LifecycleEventType};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detects day-over-day changes between yesterday's and today's universe.
///
/// Returns a list of lifecycle events suitable for QuestDB persistence.
/// Returns empty if `yesterday` is `None` (first run / no cache).
pub fn detect_universe_delta(
    yesterday: Option<&FnoUniverse>,
    today: &FnoUniverse,
) -> Vec<LifecycleEvent> {
    let yesterday = match yesterday {
        Some(y) => y,
        None => {
            info!("delta detection: no previous universe (first run), skipping");
            return Vec::new();
        }
    };

    let mut events = Vec::new();

    // 1. Detect expired/removed contracts (in yesterday, not in today)
    for (security_id, contract) in &yesterday.derivative_contracts {
        if !today.derivative_contracts.contains_key(security_id) {
            events.push(LifecycleEvent {
                security_id: *security_id,
                underlying_symbol: contract.underlying_symbol.clone(),
                event_type: LifecycleEventType::ContractExpired,
                field_changed: String::new(),
                old_value: contract.symbol_name.clone(),
                new_value: String::new(),
            });
        }
    }

    // 2. Detect added contracts and modified fields
    for (security_id, contract) in &today.derivative_contracts {
        match yesterday.derivative_contracts.get(security_id) {
            None => {
                // New contract
                events.push(LifecycleEvent {
                    security_id: *security_id,
                    underlying_symbol: contract.underlying_symbol.clone(),
                    event_type: LifecycleEventType::ContractAdded,
                    field_changed: String::new(),
                    old_value: String::new(),
                    new_value: contract.symbol_name.clone(),
                });
            }
            Some(old_contract) => {
                // Check for lot_size change
                if old_contract.lot_size != contract.lot_size {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::LotSizeChanged,
                        field_changed: "lot_size".to_owned(),
                        old_value: old_contract.lot_size.to_string(),
                        new_value: contract.lot_size.to_string(),
                    });
                }
                // Check for tick_size change
                #[allow(clippy::float_cmp)]
                // APPROVED: tick_size is exact decimal from CSV, not computed
                if old_contract.tick_size != contract.tick_size {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::TickSizeChanged,
                        field_changed: "tick_size".to_owned(),
                        old_value: old_contract.tick_size.to_string(),
                        new_value: contract.tick_size.to_string(),
                    });
                }

                // I-P1-02: Full field coverage — detect strike_price, option_type,
                // exchange_segment, display_name, symbol_name changes.
                #[allow(clippy::float_cmp)]
                // APPROVED: strike_price is exact decimal from CSV
                if old_contract.strike_price != contract.strike_price {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::FieldChanged,
                        field_changed: "strike_price".to_owned(),
                        old_value: old_contract.strike_price.to_string(),
                        new_value: contract.strike_price.to_string(),
                    });
                }
                if old_contract.option_type != contract.option_type {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::FieldChanged,
                        field_changed: "option_type".to_owned(),
                        old_value: format!("{:?}", old_contract.option_type),
                        new_value: format!("{:?}", contract.option_type),
                    });
                }
                if old_contract.exchange_segment != contract.exchange_segment {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::FieldChanged,
                        field_changed: "exchange_segment".to_owned(),
                        old_value: format!("{:?}", old_contract.exchange_segment),
                        new_value: format!("{:?}", contract.exchange_segment),
                    });
                }
                if old_contract.display_name != contract.display_name {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::FieldChanged,
                        field_changed: "display_name".to_owned(),
                        old_value: old_contract.display_name.clone(),
                        new_value: contract.display_name.clone(),
                    });
                }
                if old_contract.symbol_name != contract.symbol_name {
                    events.push(LifecycleEvent {
                        security_id: *security_id,
                        underlying_symbol: contract.underlying_symbol.clone(),
                        event_type: LifecycleEventType::FieldChanged,
                        field_changed: "symbol_name".to_owned(),
                        old_value: old_contract.symbol_name.clone(),
                        new_value: contract.symbol_name.clone(),
                    });
                }
            }
        }
    }

    // 3. Detect underlying-level changes
    for symbol in yesterday.underlyings.keys() {
        if !today.underlyings.contains_key(symbol) {
            events.push(LifecycleEvent {
                security_id: 0,
                underlying_symbol: symbol.clone(),
                event_type: LifecycleEventType::UnderlyingRemoved,
                field_changed: String::new(),
                old_value: symbol.clone(),
                new_value: String::new(),
            });
        }
    }

    for symbol in today.underlyings.keys() {
        if !yesterday.underlyings.contains_key(symbol) {
            events.push(LifecycleEvent {
                security_id: 0,
                underlying_symbol: symbol.clone(),
                event_type: LifecycleEventType::UnderlyingAdded,
                field_changed: String::new(),
                old_value: String::new(),
                new_value: symbol.clone(),
            });
        }
    }

    // 4. Check for underlying lot_size changes
    for (symbol, today_underlying) in &today.underlyings {
        if let Some(yesterday_underlying) = yesterday.underlyings.get(symbol)
            && yesterday_underlying.lot_size != today_underlying.lot_size
        {
            events.push(LifecycleEvent {
                security_id: today_underlying.underlying_security_id,
                underlying_symbol: symbol.clone(),
                event_type: LifecycleEventType::LotSizeChanged,
                field_changed: "underlying_lot_size".to_owned(),
                old_value: yesterday_underlying.lot_size.to_string(),
                new_value: today_underlying.lot_size.to_string(),
            });
        }
    }

    // I-P1-03: Security_id reuse detection across different underlyings.
    // If security_id 2885 was RELIANCE yesterday but NEWSTOCK today, emit alert.
    for (security_id, today_contract) in &today.derivative_contracts {
        if let Some(yesterday_contract) = yesterday.derivative_contracts.get(security_id)
            && yesterday_contract.underlying_symbol != today_contract.underlying_symbol
        {
            events.push(LifecycleEvent {
                security_id: *security_id,
                underlying_symbol: today_contract.underlying_symbol.clone(),
                event_type: LifecycleEventType::SecurityIdReused,
                field_changed: "underlying_symbol".to_owned(),
                old_value: yesterday_contract.underlying_symbol.clone(),
                new_value: today_contract.underlying_symbol.clone(),
            });
        }
    }

    // I-P1-04: Security_id reassignment detection for same contract.
    // If NIFTY 24000 CE changed from security_id 48372 to 55000, detect it.
    // Build compound identity: (symbol, expiry, strike, option_type)
    use std::collections::HashMap;
    let mut yesterday_identity_to_id: HashMap<(String, String, String, String), u32> =
        HashMap::new();
    for (sec_id, c) in &yesterday.derivative_contracts {
        let key = (
            c.underlying_symbol.clone(),
            c.expiry_date.to_string(),
            c.strike_price.to_string(),
            format!("{:?}", c.option_type),
        );
        yesterday_identity_to_id.insert(key, *sec_id);
    }

    for (sec_id, c) in &today.derivative_contracts {
        let key = (
            c.underlying_symbol.clone(),
            c.expiry_date.to_string(),
            c.strike_price.to_string(),
            format!("{:?}", c.option_type),
        );
        if let Some(old_id) = yesterday_identity_to_id.get(&key)
            && *old_id != *sec_id
        {
            events.push(LifecycleEvent {
                security_id: *sec_id,
                underlying_symbol: c.underlying_symbol.clone(),
                event_type: LifecycleEventType::SecurityIdReassigned,
                field_changed: "security_id".to_owned(),
                old_value: old_id.to_string(),
                new_value: sec_id.to_string(),
            });
        }
    }

    let added = events
        .iter()
        .filter(|e| e.event_type == LifecycleEventType::ContractAdded)
        .count();
    let expired = events
        .iter()
        .filter(|e| e.event_type == LifecycleEventType::ContractExpired)
        .count();
    let modified = events.len().saturating_sub(added).saturating_sub(expired);

    info!(
        total_events = events.len(),
        contracts_added = added,
        contracts_expired = expired,
        other_changes = modified,
        "delta detection complete"
    );

    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic
mod tests {
    use super::*;
    use chrono::{NaiveDate, Utc};
    use dhan_live_trader_common::instrument_types::{
        DerivativeContract, DhanInstrumentKind, FnoUnderlying, FnoUniverse, UnderlyingKind,
        UniverseBuildMetadata,
    };
    use dhan_live_trader_common::types::{ExchangeSegment, OptionType, SecurityId};
    use std::collections::HashMap;
    use std::time::Duration;

    fn make_contract(security_id: SecurityId, symbol: &str, lot_size: u32) -> DerivativeContract {
        // Use security_id-dependent strike_price to ensure unique compound identity
        // (symbol, expiry, strike, option_type) per contract. Without this,
        // P1-04 security_id reassignment detection false-positives on identical tests.
        let strike = 18000.0 + f64::from(security_id) * 100.0;
        DerivativeContract {
            security_id,
            underlying_symbol: symbol.to_owned(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: strike,
            option_type: Some(OptionType::Call),
            lot_size,
            tick_size: 0.05,
            symbol_name: format!("{symbol}-CE-{}", strike as u64),
            display_name: format!("{symbol} 27 Mar {} CE", strike as u64),
        }
    }

    fn make_underlying(symbol: &str, lot_size: u32) -> FnoUnderlying {
        FnoUnderlying {
            underlying_symbol: symbol.to_owned(),
            underlying_security_id: 26000,
            price_feed_security_id: 13,
            price_feed_segment: ExchangeSegment::IdxI,
            derivative_segment: ExchangeSegment::NseFno,
            kind: UnderlyingKind::NseIndex,
            lot_size,
            contract_count: 100,
        }
    }

    fn make_universe(
        contracts: Vec<DerivativeContract>,
        underlyings: Vec<FnoUnderlying>,
    ) -> FnoUniverse {
        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
        let derivative_contracts: HashMap<SecurityId, DerivativeContract> =
            contracts.into_iter().map(|c| (c.security_id, c)).collect();
        let underlying_map: HashMap<String, FnoUnderlying> = underlyings
            .into_iter()
            .map(|u| (u.underlying_symbol.clone(), u))
            .collect();
        FnoUniverse {
            underlyings: underlying_map,
            derivative_contracts,
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_owned(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: Duration::from_millis(1),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        }
    }

    #[test]
    fn test_no_yesterday_returns_empty() {
        let today = make_universe(vec![make_contract(1, "NIFTY", 75)], vec![]);
        let events = detect_universe_delta(None, &today);
        assert!(events.is_empty());
    }

    #[test]
    fn test_identical_universes_returns_empty() {
        let contracts = vec![make_contract(1, "NIFTY", 75), make_contract(2, "NIFTY", 75)];
        let underlyings = vec![make_underlying("NIFTY", 75)];
        let yesterday = make_universe(contracts.clone(), underlyings.clone());
        let today = make_universe(contracts, underlyings);
        let events = detect_universe_delta(Some(&yesterday), &today);
        assert!(
            events.is_empty(),
            "identical universes should produce 0 events"
        );
    }

    #[test]
    fn test_contract_added() {
        let yesterday = make_universe(
            vec![make_contract(1, "NIFTY", 75)],
            vec![make_underlying("NIFTY", 75)],
        );
        let today = make_universe(
            vec![make_contract(1, "NIFTY", 75), make_contract(2, "NIFTY", 75)],
            vec![make_underlying("NIFTY", 75)],
        );
        let events = detect_universe_delta(Some(&yesterday), &today);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, LifecycleEventType::ContractAdded);
        assert_eq!(events[0].security_id, 2);
    }

    #[test]
    fn test_contract_expired() {
        let yesterday = make_universe(
            vec![make_contract(1, "NIFTY", 75), make_contract(2, "NIFTY", 75)],
            vec![make_underlying("NIFTY", 75)],
        );
        let today = make_universe(
            vec![make_contract(1, "NIFTY", 75)],
            vec![make_underlying("NIFTY", 75)],
        );
        let events = detect_universe_delta(Some(&yesterday), &today);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, LifecycleEventType::ContractExpired);
        assert_eq!(events[0].security_id, 2);
    }

    #[test]
    fn test_lot_size_changed() {
        let yesterday = make_universe(
            vec![make_contract(1, "NIFTY", 75)],
            vec![make_underlying("NIFTY", 75)],
        );
        let today = make_universe(
            vec![make_contract(1, "NIFTY", 50)],
            vec![make_underlying("NIFTY", 50)],
        );
        let events = detect_universe_delta(Some(&yesterday), &today);
        // Should have 2 events: contract lot_size + underlying lot_size
        let lot_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::LotSizeChanged)
            .collect();
        assert_eq!(lot_events.len(), 2);
        assert_eq!(lot_events[0].old_value, "75");
        assert_eq!(lot_events[0].new_value, "50");
    }

    #[test]
    fn test_underlying_added() {
        let yesterday = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(
            vec![],
            vec![
                make_underlying("NIFTY", 75),
                make_underlying("BANKNIFTY", 15),
            ],
        );
        let events = detect_universe_delta(Some(&yesterday), &today);
        let added: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::UnderlyingAdded)
            .collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].underlying_symbol, "BANKNIFTY");
    }

    #[test]
    fn test_underlying_removed() {
        let yesterday = make_universe(
            vec![],
            vec![
                make_underlying("NIFTY", 75),
                make_underlying("BANKNIFTY", 15),
            ],
        );
        let today = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);
        let removed: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::UnderlyingRemoved)
            .collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].underlying_symbol, "BANKNIFTY");
    }

    #[test]
    fn test_mixed_changes() {
        let yesterday = make_universe(
            vec![
                make_contract(1, "NIFTY", 75), // will remain
                make_contract(2, "NIFTY", 75), // will expire
                make_contract(3, "NIFTY", 75), // lot_size will change
            ],
            vec![make_underlying("NIFTY", 75)],
        );
        let today = make_universe(
            vec![
                make_contract(1, "NIFTY", 75), // unchanged
                make_contract(3, "NIFTY", 50), // lot_size changed
                make_contract(4, "NIFTY", 75), // new
            ],
            vec![make_underlying("NIFTY", 75)],
        );
        let events = detect_universe_delta(Some(&yesterday), &today);

        let added = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::ContractAdded)
            .count();
        let expired = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::ContractExpired)
            .count();
        let changed = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::LotSizeChanged)
            .count();

        assert_eq!(added, 1, "contract 4 should be added");
        assert_eq!(expired, 1, "contract 2 should be expired");
        assert_eq!(changed, 1, "contract 3 lot_size should be changed");
    }

    #[test]
    fn test_both_universes_empty() {
        let yesterday = make_universe(vec![], vec![]);
        let today = make_universe(vec![], vec![]);
        let events = detect_universe_delta(Some(&yesterday), &today);
        assert!(events.is_empty());
    }

    #[test]
    fn test_tick_size_changed() {
        let mut contract_old = make_contract(1, "NIFTY", 75);
        contract_old.tick_size = 0.05;
        let mut contract_new = make_contract(1, "NIFTY", 75);
        contract_new.tick_size = 0.10;

        let yesterday = make_universe(vec![contract_old], vec![]);
        let today = make_universe(vec![contract_new], vec![]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let tick_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::TickSizeChanged)
            .collect();
        assert_eq!(tick_events.len(), 1);
        assert_eq!(tick_events[0].field_changed, "tick_size");
    }

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(LifecycleEventType::ContractAdded.as_str(), "contract_added");
        assert_eq!(
            LifecycleEventType::ContractExpired.as_str(),
            "contract_expired"
        );
        assert_eq!(
            LifecycleEventType::LotSizeChanged.as_str(),
            "lot_size_changed"
        );
        assert_eq!(
            LifecycleEventType::TickSizeChanged.as_str(),
            "tick_size_changed"
        );
        assert_eq!(
            LifecycleEventType::UnderlyingAdded.as_str(),
            "underlying_added"
        );
        assert_eq!(
            LifecycleEventType::UnderlyingRemoved.as_str(),
            "underlying_removed"
        );
    }

    // -----------------------------------------------------------------------
    // Gap 9: Delta detection at scale — 1000+ contracts with mixed changes
    // -----------------------------------------------------------------------

    #[test]
    fn test_delta_detection_at_scale_1000_contracts() {
        // Yesterday: 1000 contracts (IDs 1..=1000)
        // Today:     900 old + 200 new + 50 lot_size changed
        //   - Contracts 901..=1000 expired (100 expired)
        //   - Contracts 1..=50 have lot_size changed (50 changed)
        //   - Contracts 1001..=1200 added (200 new)
        //   - Contracts 51..=900 unchanged (850 unchanged)

        let mut today_contracts = Vec::with_capacity(1200);
        // 1..=50 with changed lot_size
        for id in 1..=50_u32 {
            today_contracts.push(make_contract(id, "NIFTY", 50));
        }
        // 51..=900 unchanged
        for id in 51..=900_u32 {
            today_contracts.push(make_contract(id, "NIFTY", 75));
        }
        // 901..=1000 expired (not in today)
        // 1001..=1200 added
        for id in 1001..=1200_u32 {
            today_contracts.push(make_contract(id, "NIFTY", 75));
        }

        let mut today_underlyings = vec![
            make_underlying("NIFTY", 75),
            // BANKNIFTY removed
            // MIDCPNIFTY added
        ];
        today_underlyings.push({
            let mut u = make_underlying("MIDCPNIFTY", 120);
            u.underlying_security_id = 26074;
            u.price_feed_security_id = 442;
            u
        });

        let yesterday = make_universe(
            (1..=1000_u32)
                .map(|id| make_contract(id, "NIFTY", 75))
                .collect(),
            vec![
                make_underlying("NIFTY", 75),
                make_underlying("BANKNIFTY", 15),
            ],
        );

        let today = make_universe(today_contracts, today_underlyings);

        let events = detect_universe_delta(Some(&yesterday), &today);

        let added = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::ContractAdded)
            .count();
        let expired = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::ContractExpired)
            .count();
        let lot_changed = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::LotSizeChanged)
            .count();
        let underlying_added = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::UnderlyingAdded)
            .count();
        let underlying_removed = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::UnderlyingRemoved)
            .count();

        assert_eq!(added, 200, "200 new contracts expected");
        assert_eq!(expired, 100, "100 expired contracts expected");
        assert_eq!(lot_changed, 50, "50 lot_size changed contracts expected");
        assert_eq!(underlying_added, 1, "1 underlying added (MIDCPNIFTY)");
        assert_eq!(underlying_removed, 1, "1 underlying removed (BANKNIFTY)");

        // Total events = 200 + 100 + 50 + 1 + 1 = 352
        assert_eq!(events.len(), 352, "total event count mismatch");
    }

    #[test]
    fn test_delta_detection_all_contracts_expired() {
        // Yesterday has 500 contracts, today has none — all should be expired
        let contracts: Vec<_> = (1..=500_u32)
            .map(|id| make_contract(id, "NIFTY", 75))
            .collect();
        let yesterday = make_universe(contracts, vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);

        let events = detect_universe_delta(Some(&yesterday), &today);
        let expired = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::ContractExpired)
            .count();
        assert_eq!(expired, 500, "all 500 contracts should be expired");
    }

    #[test]
    fn test_delta_detection_all_contracts_new() {
        // Yesterday has no contracts, today has 500 — all should be added
        let yesterday = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);
        let contracts: Vec<_> = (1..=500_u32)
            .map(|id| make_contract(id, "NIFTY", 75))
            .collect();
        let today = make_universe(contracts, vec![make_underlying("NIFTY", 75)]);

        let events = detect_universe_delta(Some(&yesterday), &today);
        let added = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::ContractAdded)
            .count();
        assert_eq!(added, 500, "all 500 contracts should be added");
    }

    // -----------------------------------------------------------------------
    // I-P1-02: Full field coverage — detect strike, option_type, segment,
    // display_name, symbol_name changes
    // -----------------------------------------------------------------------

    #[test]
    fn test_p1_02_strike_price_change_detected() {
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.strike_price = 18000.0;
        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.strike_price = 18500.0;

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let field_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == LifecycleEventType::FieldChanged
                    && e.field_changed == "strike_price"
            })
            .collect();
        assert_eq!(
            field_events.len(),
            1,
            "I-P1-02: strike_price change must be detected"
        );
        assert_eq!(field_events[0].old_value, "18000");
        assert_eq!(field_events[0].new_value, "18500");
    }

    #[test]
    fn test_p1_02_option_type_change_detected() {
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.option_type = Some(OptionType::Call);
        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.option_type = Some(OptionType::Put);

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let field_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == LifecycleEventType::FieldChanged && e.field_changed == "option_type"
            })
            .collect();
        assert_eq!(
            field_events.len(),
            1,
            "I-P1-02: option_type change must be detected"
        );
    }

    #[test]
    fn test_p1_02_exchange_segment_change_detected() {
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.exchange_segment = ExchangeSegment::NseFno;
        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.exchange_segment = ExchangeSegment::BseFno;

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let field_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == LifecycleEventType::FieldChanged
                    && e.field_changed == "exchange_segment"
            })
            .collect();
        assert_eq!(
            field_events.len(),
            1,
            "I-P1-02: exchange_segment change must be detected"
        );
    }

    #[test]
    fn test_p1_02_display_name_change_detected() {
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.display_name = "OLD DISPLAY".to_owned();
        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.display_name = "NEW DISPLAY".to_owned();

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let field_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == LifecycleEventType::FieldChanged
                    && e.field_changed == "display_name"
            })
            .collect();
        assert_eq!(
            field_events.len(),
            1,
            "I-P1-02: display_name change must be detected"
        );
        assert_eq!(field_events[0].old_value, "OLD DISPLAY");
        assert_eq!(field_events[0].new_value, "NEW DISPLAY");
    }

    #[test]
    fn test_p1_02_symbol_name_change_detected() {
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.symbol_name = "OLD-SYMBOL".to_owned();
        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.symbol_name = "NEW-SYMBOL".to_owned();

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let field_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == LifecycleEventType::FieldChanged && e.field_changed == "symbol_name"
            })
            .collect();
        assert_eq!(
            field_events.len(),
            1,
            "I-P1-02: symbol_name change must be detected"
        );
    }

    #[test]
    fn test_p1_02_tick_size_change_detected() {
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.tick_size = 0.05;
        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.tick_size = 0.10;

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let tick_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::TickSizeChanged)
            .collect();
        assert_eq!(
            tick_events.len(),
            1,
            "I-P1-02: tick_size change must be detected"
        );
    }

    #[test]
    fn test_p1_02_multiple_field_changes_all_reported() {
        // Change strike_price, display_name, and symbol_name simultaneously
        let mut old_contract = make_contract(1, "NIFTY", 75);
        old_contract.strike_price = 18000.0;
        old_contract.display_name = "OLD".to_owned();
        old_contract.symbol_name = "OLD-SYM".to_owned();

        let mut new_contract = make_contract(1, "NIFTY", 75);
        new_contract.strike_price = 19000.0;
        new_contract.display_name = "NEW".to_owned();
        new_contract.symbol_name = "NEW-SYM".to_owned();

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);

        let field_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::FieldChanged)
            .collect();
        assert_eq!(
            field_events.len(),
            3,
            "I-P1-02: all 3 field changes must be individual events"
        );

        let fields: Vec<&str> = field_events
            .iter()
            .map(|e| e.field_changed.as_str())
            .collect();
        assert!(fields.contains(&"strike_price"));
        assert!(fields.contains(&"display_name"));
        assert!(fields.contains(&"symbol_name"));
    }

    // -----------------------------------------------------------------------
    // I-P1-03: Security_id reuse detection across different underlyings
    // -----------------------------------------------------------------------

    #[test]
    fn test_p1_03_security_id_reuse_across_underlyings() {
        // security_id 100 was RELIANCE yesterday, NIFTY today
        let old_contract = DerivativeContract {
            security_id: 100,
            underlying_symbol: "RELIANCE".to_owned(),
            instrument_kind: DhanInstrumentKind::FutureStock,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 0.0,
            option_type: None,
            lot_size: 500,
            tick_size: 0.05,
            symbol_name: "RELIANCE-FUT".to_owned(),
            display_name: "RELIANCE MAR FUT".to_owned(),
        };
        let new_contract = DerivativeContract {
            security_id: 100,
            underlying_symbol: "NIFTY".to_owned(),
            instrument_kind: DhanInstrumentKind::FutureIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 0.0,
            option_type: None,
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-FUT".to_owned(),
            display_name: "NIFTY MAR FUT".to_owned(),
        };

        let yesterday = make_universe(
            vec![old_contract],
            vec![
                make_underlying("RELIANCE", 500),
                make_underlying("NIFTY", 75),
            ],
        );
        let today = make_universe(
            vec![new_contract],
            vec![
                make_underlying("RELIANCE", 500),
                make_underlying("NIFTY", 75),
            ],
        );

        let events = detect_universe_delta(Some(&yesterday), &today);
        let reuse_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::SecurityIdReused)
            .collect();
        assert_eq!(
            reuse_events.len(),
            1,
            "I-P1-03: security_id reuse must be detected"
        );
        assert_eq!(reuse_events[0].security_id, 100);
        assert_eq!(reuse_events[0].old_value, "RELIANCE");
        assert_eq!(reuse_events[0].new_value, "NIFTY");
    }

    #[test]
    fn test_p1_03_no_reuse_when_same_underlying() {
        // Same security_id, same underlying — no reuse event
        let contract = make_contract(1, "NIFTY", 75);
        let yesterday = make_universe(vec![contract.clone()], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![contract], vec![make_underlying("NIFTY", 75)]);

        let events = detect_universe_delta(Some(&yesterday), &today);
        let reuse_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::SecurityIdReused)
            .collect();
        assert!(
            reuse_events.is_empty(),
            "I-P1-03: same underlying → no reuse event"
        );
    }

    // -----------------------------------------------------------------------
    // I-P1-04: Security_id reassignment for same contract identity
    // -----------------------------------------------------------------------

    #[test]
    fn test_p1_04_security_id_reassignment_detected() {
        // Same contract identity (NIFTY, 2026-03-27, 20000.0, Call) but
        // different security_ids between yesterday and today.
        let old_contract = DerivativeContract {
            security_id: 48372,
            underlying_symbol: "NIFTY".to_owned(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 20000.0,
            option_type: Some(OptionType::Call),
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-CE-20000".to_owned(),
            display_name: "NIFTY 27 Mar 20000 CE".to_owned(),
        };
        let new_contract = DerivativeContract {
            security_id: 55000,
            underlying_symbol: "NIFTY".to_owned(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 20000.0,
            option_type: Some(OptionType::Call),
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-CE-20000".to_owned(),
            display_name: "NIFTY 27 Mar 20000 CE".to_owned(),
        };

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);

        let events = detect_universe_delta(Some(&yesterday), &today);
        let reassignment_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::SecurityIdReassigned)
            .collect();
        assert_eq!(
            reassignment_events.len(),
            1,
            "I-P1-04: security_id reassignment must be detected"
        );
        assert_eq!(reassignment_events[0].security_id, 55000);
        assert_eq!(reassignment_events[0].old_value, "48372");
        assert_eq!(reassignment_events[0].new_value, "55000");
        assert_eq!(reassignment_events[0].field_changed, "security_id");
    }

    #[test]
    fn test_p1_04_no_reassignment_when_id_unchanged() {
        // Same contract identity AND same security_id → no reassignment
        let contract = DerivativeContract {
            security_id: 48372,
            underlying_symbol: "NIFTY".to_owned(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 20000.0,
            option_type: Some(OptionType::Call),
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-CE-20000".to_owned(),
            display_name: "NIFTY 27 Mar 20000 CE".to_owned(),
        };

        let yesterday = make_universe(vec![contract.clone()], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![contract], vec![make_underlying("NIFTY", 75)]);

        let events = detect_universe_delta(Some(&yesterday), &today);
        let reassignment_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::SecurityIdReassigned)
            .collect();
        assert!(
            reassignment_events.is_empty(),
            "I-P1-04: same security_id → no reassignment event"
        );
    }

    // -----------------------------------------------------------------------
    // detect_universe_delta: underlying lot_size unchanged produces no event
    // -----------------------------------------------------------------------

    #[test]
    fn test_underlying_lot_size_unchanged_no_event() {
        let yesterday = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);
        let lot_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::LotSizeChanged)
            .collect();
        assert!(
            lot_events.is_empty(),
            "same lot_size should produce no event"
        );
    }

    // -----------------------------------------------------------------------
    // detect_universe_delta: underlying lot_size changed produces event
    // -----------------------------------------------------------------------

    #[test]
    fn test_underlying_lot_size_changed_event() {
        let yesterday = make_universe(vec![], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![], vec![make_underlying("NIFTY", 50)]);
        let events = detect_universe_delta(Some(&yesterday), &today);
        let lot_events: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == LifecycleEventType::LotSizeChanged
                    && e.field_changed == "underlying_lot_size"
            })
            .collect();
        assert_eq!(lot_events.len(), 1);
        assert_eq!(lot_events[0].old_value, "75");
        assert_eq!(lot_events[0].new_value, "50");
    }

    // -----------------------------------------------------------------------
    // detect_universe_delta: contract unchanged produces no events
    // -----------------------------------------------------------------------

    #[test]
    fn test_contract_unchanged_no_events() {
        let contract = make_contract(1, "NIFTY", 75);
        let yesterday = make_universe(vec![contract.clone()], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![contract], vec![make_underlying("NIFTY", 75)]);
        let events = detect_universe_delta(Some(&yesterday), &today);
        assert!(
            events.is_empty(),
            "unchanged contract should produce no events"
        );
    }

    #[test]
    fn test_p1_04_different_strike_no_reassignment() {
        // Different strike_price → different compound identity → not a reassignment
        let old_contract = DerivativeContract {
            security_id: 48372,
            underlying_symbol: "NIFTY".to_owned(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 20000.0,
            option_type: Some(OptionType::Call),
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-CE-20000".to_owned(),
            display_name: "NIFTY 27 Mar 20000 CE".to_owned(),
        };
        let new_contract = DerivativeContract {
            security_id: 55000,
            underlying_symbol: "NIFTY".to_owned(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).expect("valid date"),
            strike_price: 21000.0, // Different strike → different identity
            option_type: Some(OptionType::Call),
            lot_size: 75,
            tick_size: 0.05,
            symbol_name: "NIFTY-CE-21000".to_owned(),
            display_name: "NIFTY 27 Mar 21000 CE".to_owned(),
        };

        let yesterday = make_universe(vec![old_contract], vec![make_underlying("NIFTY", 75)]);
        let today = make_universe(vec![new_contract], vec![make_underlying("NIFTY", 75)]);

        let events = detect_universe_delta(Some(&yesterday), &today);
        let reassignment_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == LifecycleEventType::SecurityIdReassigned)
            .collect();
        assert!(
            reassignment_events.is_empty(),
            "I-P1-04: different strike_price → different identity, no reassignment"
        );
    }
}
