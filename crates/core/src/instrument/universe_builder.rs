//! 5-pass F&O universe building algorithm.
//!
//! Takes parsed CSV rows and builds the complete [`FnoUniverse`] with all
//! lookup maps, option chains, and expiry calendars.
//!
//! # Pass Summary
//!
//! | Pass | Input | Output |
//! |------|-------|--------|
//! | 1 | segment="I" rows | index_lookup: symbol → (security_id, exchange) |
//! | 2 | NSE segment="E" series="EQ" | equity_lookup: symbol → security_id |
//! | 3 | FUTIDX + FUTSTK rows | unlinked underlyings (dedup, skip TEST) |
//! | 4 | Pass 3 + Pass 1/2 lookups | FnoUnderlying with price IDs linked |
//! | 5 | All segment="D" rows | contracts, option chains, expiry calendars |

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{FixedOffset, NaiveDate, Utc};
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::InstrumentConfig;
use dhan_live_trader_common::constants::*;
use dhan_live_trader_common::instrument_types::*;
use dhan_live_trader_common::types::{Exchange, ExchangeSegment, OptionType, SecurityId};

use super::csv_downloader::download_instrument_csv;
use super::csv_parser::{ParsedInstrumentRow, parse_instrument_csv};
use super::validation::validate_fno_universe;

// ---------------------------------------------------------------------------
// Intermediate Types (not exposed outside this module)
// ---------------------------------------------------------------------------

/// An index entry discovered in Pass 1.
#[derive(Debug, Clone)]
struct IndexEntry {
    security_id: SecurityId,
    /// Stored for structural completeness; read in tests.
    #[cfg_attr(not(test), allow(dead_code))]
    exchange: Exchange,
}

/// An F&O underlying discovered in Pass 3 but not yet linked to its price feed.
#[derive(Debug, Clone)]
struct UnlinkedUnderlying {
    underlying_symbol: String,
    underlying_security_id: SecurityId,
    kind: UnderlyingKind,
    lot_size: u32,
    derivative_exchange: Exchange,
}

// ---------------------------------------------------------------------------
// Pass 1: Index Lookup
// ---------------------------------------------------------------------------

/// Build a lookup of all indices: symbol → (security_id, exchange).
///
/// Scans all rows with segment='I' from both NSE and BSE.
/// Keyed by `underlying_symbol` from the IDX_I row.
fn build_index_lookup(rows: &[ParsedInstrumentRow]) -> HashMap<String, IndexEntry> {
    let mut lookup = HashMap::with_capacity(256);

    for row in rows {
        if row.segment != 'I' {
            continue;
        }

        lookup.insert(
            row.underlying_symbol.clone(),
            IndexEntry {
                security_id: row.security_id,
                exchange: row.exchange,
            },
        );
    }

    info!(index_count = lookup.len(), "Pass 1: index lookup built");
    lookup
}

// ---------------------------------------------------------------------------
// Pass 2: Equity Lookup
// ---------------------------------------------------------------------------

/// Build a lookup of NSE equities: symbol → security_id.
///
/// Only includes rows matching (NSE, segment='E', series='EQ').
fn build_equity_lookup(rows: &[ParsedInstrumentRow]) -> HashMap<String, SecurityId> {
    let mut lookup = HashMap::with_capacity(4096);

    for row in rows {
        if row.exchange != Exchange::NationalStockExchange
            || row.segment != 'E'
            || row.series != CSV_SERIES_EQUITY
        {
            continue;
        }

        lookup.insert(row.underlying_symbol.clone(), row.security_id);
    }

    info!(equity_count = lookup.len(), "Pass 2: equity lookup built");
    lookup
}

// ---------------------------------------------------------------------------
// Pass 3: Discover F&O Underlyings
// ---------------------------------------------------------------------------

/// Discover unique F&O underlyings from futures rows.
///
/// Scans FUTIDX and FUTSTK rows, deduplicates by `underlying_symbol`,
/// skips TEST instruments, and classifies each as NseIndex/BseIndex/Stock.
fn discover_fno_underlyings(rows: &[ParsedInstrumentRow]) -> Vec<UnlinkedUnderlying> {
    let mut seen: HashMap<String, UnlinkedUnderlying> = HashMap::with_capacity(256);

    for row in rows {
        if row.segment != 'D' {
            continue;
        }

        let is_futures =
            row.instrument == CSV_INSTRUMENT_FUTIDX || row.instrument == CSV_INSTRUMENT_FUTSTK;
        if !is_futures {
            continue;
        }

        // Skip TEST instruments
        if row.underlying_symbol.contains(CSV_TEST_SYMBOL_MARKER) {
            debug!(symbol = %row.underlying_symbol, "Pass 3: skipping TEST instrument");
            continue;
        }

        // Skip BSE stock futures — BSE_FNO only allowed for index derivatives
        // (SENSEX, BANKEX, SENSEX50). BSE stock derivatives are not traded.
        if row.exchange == Exchange::BombayStockExchange && row.instrument == CSV_INSTRUMENT_FUTSTK
        {
            debug!(symbol = %row.underlying_symbol, "Pass 3: skipping BSE stock future");
            continue;
        }

        // Dedup: first occurrence wins (lot_size from first future encountered)
        if seen.contains_key(&row.underlying_symbol) {
            continue;
        }

        let kind = match (row.instrument.as_str(), row.exchange) {
            (CSV_INSTRUMENT_FUTIDX, Exchange::NationalStockExchange) => UnderlyingKind::NseIndex,
            (CSV_INSTRUMENT_FUTIDX, Exchange::BombayStockExchange) => UnderlyingKind::BseIndex,
            (CSV_INSTRUMENT_FUTSTK, Exchange::NationalStockExchange) => UnderlyingKind::Stock,
            _ => continue,
        };

        seen.insert(
            row.underlying_symbol.clone(),
            UnlinkedUnderlying {
                underlying_symbol: row.underlying_symbol.clone(),
                underlying_security_id: row.underlying_security_id,
                kind,
                lot_size: row.lot_size,
                derivative_exchange: row.exchange,
            },
        );
    }

    info!(
        underlying_count = seen.len(),
        "Pass 3: F&O underlyings discovered"
    );
    seen.into_values().collect()
}

// ---------------------------------------------------------------------------
// Pass 4: Link Price IDs
// ---------------------------------------------------------------------------

/// Link each F&O underlying to its live price feed security ID.
///
/// For indices: looks up in `index_lookup` with alias fallback via
/// [`INDEX_SYMBOL_ALIASES`].
/// For stocks: looks up in `equity_lookup`.
///
/// Falls back to `underlying_security_id` if no match is found (with warning).
fn link_price_ids(
    unlinked: Vec<UnlinkedUnderlying>,
    index_lookup: &HashMap<String, IndexEntry>,
    equity_lookup: &HashMap<String, SecurityId>,
) -> HashMap<String, FnoUnderlying> {
    let mut underlyings = HashMap::with_capacity(unlinked.len());

    for item in unlinked {
        let (price_feed_security_id, price_feed_segment, derivative_segment) = match item.kind {
            UnderlyingKind::NseIndex | UnderlyingKind::BseIndex => {
                // Try direct lookup first, then alias lookup
                let index_entry = index_lookup.get(&item.underlying_symbol).or_else(|| {
                    INDEX_SYMBOL_ALIASES
                        .iter()
                        .find(|(fno_sym, _)| *fno_sym == item.underlying_symbol)
                        .and_then(|(_, idx_sym)| index_lookup.get(*idx_sym))
                });

                let derivative_seg = match item.derivative_exchange {
                    Exchange::NationalStockExchange => ExchangeSegment::NseFno,
                    Exchange::BombayStockExchange => ExchangeSegment::BseFno,
                };

                match index_entry {
                    Some(entry) => (entry.security_id, ExchangeSegment::IdxI, derivative_seg),
                    None => {
                        warn!(
                            symbol = %item.underlying_symbol,
                            fallback_id = item.underlying_security_id,
                            "Pass 4: no IDX_I price ID found, using underlying_security_id"
                        );
                        (
                            item.underlying_security_id,
                            ExchangeSegment::IdxI,
                            derivative_seg,
                        )
                    }
                }
            }
            UnderlyingKind::Stock => {
                let price_id = equity_lookup
                    .get(&item.underlying_symbol)
                    .copied()
                    .unwrap_or_else(|| {
                        warn!(
                            symbol = %item.underlying_symbol,
                            fallback_id = item.underlying_security_id,
                            "Pass 4: no NSE_EQ price ID found, using underlying_security_id"
                        );
                        item.underlying_security_id
                    });
                (
                    price_id,
                    ExchangeSegment::NseEquity,
                    ExchangeSegment::NseFno,
                )
            }
        };

        underlyings.insert(
            item.underlying_symbol.clone(),
            FnoUnderlying {
                underlying_symbol: item.underlying_symbol,
                underlying_security_id: item.underlying_security_id,
                price_feed_security_id,
                price_feed_segment,
                derivative_segment,
                kind: item.kind,
                lot_size: item.lot_size,
                contract_count: 0, // Filled in Pass 5
            },
        );
    }

    info!(linked_count = underlyings.len(), "Pass 4: price IDs linked");
    underlyings
}

// ---------------------------------------------------------------------------
// Pass 5: Derivatives, Option Chains, Expiry Calendars, InstrumentInfo
// ---------------------------------------------------------------------------

/// Output of Pass 5.
struct Pass5Result {
    derivative_contracts: HashMap<SecurityId, DerivativeContract>,
    instrument_info: HashMap<SecurityId, InstrumentInfo>,
    option_chains: HashMap<OptionChainKey, OptionChain>,
    expiry_calendars: HashMap<String, ExpiryCalendar>,
}

/// Build all derivative contracts, option chains, expiry calendars, and the
/// global instrument_info lookup.
///
/// Also populates instrument_info for indices (segment='I') and equities
/// (segment='E'), and updates `contract_count` on each underlying in-place.
///
/// Contracts with `expiry_date < today` are filtered out as expired.
fn build_derivatives_and_chains(
    rows: &[ParsedInstrumentRow],
    underlyings: &mut HashMap<String, FnoUnderlying>,
    today: NaiveDate,
) -> Pass5Result {
    // Estimate: ~150K derivative contracts, ~160K instrument_info (indices + equities + derivatives)
    let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> =
        HashMap::with_capacity(160_000);
    let mut instrument_info: HashMap<SecurityId, InstrumentInfo> = HashMap::with_capacity(170_000);

    // Intermediate: option chain accumulator — (calls, puts) per chain key
    let mut chain_builders: HashMap<
        OptionChainKey,
        (Vec<OptionChainEntry>, Vec<OptionChainEntry>),
    > = HashMap::with_capacity(2048);

    // Track futures per (underlying, expiry) for linking to option chains
    let mut futures_by_key: HashMap<OptionChainKey, SecurityId> = HashMap::with_capacity(1024);

    // Track expiry dates per underlying (BTreeSet for automatic sorting)
    let mut expiry_sets: HashMap<String, BTreeSet<NaiveDate>> = HashMap::with_capacity(256);

    // Track contract count per underlying
    let mut contract_counts: HashMap<String, usize> = HashMap::with_capacity(256);

    // --- Step 1: Instrument info for indices and equities ---
    for row in rows {
        match row.segment {
            'I' => {
                instrument_info.insert(
                    row.security_id,
                    InstrumentInfo::Index {
                        security_id: row.security_id,
                        symbol: row.underlying_symbol.clone(),
                        exchange: row.exchange,
                    },
                );
            }
            'E' if row.exchange == Exchange::NationalStockExchange
                && row.series == CSV_SERIES_EQUITY =>
            {
                instrument_info.insert(
                    row.security_id,
                    InstrumentInfo::Equity {
                        security_id: row.security_id,
                        symbol: row.underlying_symbol.clone(),
                    },
                );
            }
            _ => {} // Derivatives handled in step 2
        }
    }

    // --- Step 2: Scan derivative rows ---
    let mut skipped_expired: usize = 0;
    let mut skipped_unknown_underlying: usize = 0;
    let mut skipped_test: usize = 0;
    let mut skipped_unknown_instrument: usize = 0;
    let mut skipped_bse_stock_derivative: usize = 0;

    for row in rows {
        if row.segment != 'D' {
            continue;
        }

        // Classify instrument kind
        let instrument_kind = match row.instrument.as_str() {
            CSV_INSTRUMENT_FUTIDX => DhanInstrumentKind::FutureIndex,
            CSV_INSTRUMENT_FUTSTK => DhanInstrumentKind::FutureStock,
            CSV_INSTRUMENT_OPTIDX => DhanInstrumentKind::OptionIndex,
            CSV_INSTRUMENT_OPTSTK => DhanInstrumentKind::OptionStock,
            _ => {
                skipped_unknown_instrument += 1;
                continue;
            }
        };

        // Skip TEST instruments
        if row.underlying_symbol.contains(CSV_TEST_SYMBOL_MARKER) {
            skipped_test += 1;
            continue;
        }

        // Skip BSE stock derivatives — BSE_FNO only allowed for index derivatives
        // (SENSEX, BANKEX, SENSEX50). BSE stock futures/options are not traded.
        if row.exchange == Exchange::BombayStockExchange
            && matches!(
                row.instrument.as_str(),
                CSV_INSTRUMENT_FUTSTK | CSV_INSTRUMENT_OPTSTK
            )
        {
            skipped_bse_stock_derivative += 1;
            continue;
        }

        // Skip if underlying not in our universe
        if !underlyings.contains_key(&row.underlying_symbol) {
            skipped_unknown_underlying += 1;
            continue;
        }

        // Expiry date is required for derivatives
        let expiry_date = match row.expiry_date {
            Some(date) => date,
            None => {
                debug!(
                    security_id = row.security_id,
                    symbol = %row.underlying_symbol,
                    "Pass 5: derivative without expiry date, skipping"
                );
                continue;
            }
        };

        // Skip expired contracts (expiry < today)
        if expiry_date < today {
            skipped_expired += 1;
            continue;
        }

        // Determine exchange segment
        let exchange_segment = match row.exchange {
            Exchange::NationalStockExchange => ExchangeSegment::NseFno,
            Exchange::BombayStockExchange => ExchangeSegment::BseFno,
        };

        // Normalize strike price: Dhan uses -0.01 for futures (no strike)
        let strike_price = if row.strike_price < 0.0 {
            0.0
        } else {
            row.strike_price
        };

        // Create DerivativeContract
        let contract = DerivativeContract {
            security_id: row.security_id,
            underlying_symbol: row.underlying_symbol.clone(),
            instrument_kind,
            exchange_segment,
            expiry_date,
            strike_price,
            option_type: row.option_type,
            lot_size: row.lot_size,
            tick_size: row.tick_size,
            symbol_name: row.symbol_name.clone(),
            display_name: row.display_name.clone(),
        };

        derivative_contracts.insert(row.security_id, contract);

        // Add to instrument_info
        instrument_info.insert(
            row.security_id,
            InstrumentInfo::Derivative {
                security_id: row.security_id,
                underlying_symbol: row.underlying_symbol.clone(),
                instrument_kind,
                exchange_segment,
                expiry_date,
                strike_price,
                option_type: row.option_type,
            },
        );

        // Track expiry date for calendar
        expiry_sets
            .entry(row.underlying_symbol.clone())
            .or_default()
            .insert(expiry_date);

        // Increment per-underlying contract count
        *contract_counts
            .entry(row.underlying_symbol.clone())
            .or_default() += 1;

        // Build chain key
        let chain_key = OptionChainKey {
            underlying_symbol: row.underlying_symbol.clone(),
            expiry_date,
        };

        // Accumulate into option chains or track futures
        match instrument_kind {
            DhanInstrumentKind::FutureIndex | DhanInstrumentKind::FutureStock => {
                futures_by_key.insert(chain_key, row.security_id);
            }
            DhanInstrumentKind::OptionIndex | DhanInstrumentKind::OptionStock => {
                let entry = OptionChainEntry {
                    security_id: row.security_id,
                    strike_price,
                    lot_size: row.lot_size,
                };

                let (calls, puts) = chain_builders.entry(chain_key).or_default();
                match row.option_type {
                    Some(OptionType::Call) => calls.push(entry),
                    Some(OptionType::Put) => puts.push(entry),
                    None => {
                        warn!(
                            security_id = row.security_id,
                            "Pass 5: option without option_type, skipping chain entry"
                        );
                    }
                }
            }
        }
    }

    info!(
        derivative_count = derivative_contracts.len(),
        skipped_expired,
        skipped_unknown_underlying,
        skipped_test,
        skipped_unknown_instrument,
        skipped_bse_stock_derivative,
        "Pass 5: derivative scanning complete"
    );

    // --- Step 3: Finalize option chains (sort by strike) ---
    let mut option_chains: HashMap<OptionChainKey, OptionChain> =
        HashMap::with_capacity(chain_builders.len());

    for (key, (mut calls, mut puts)) in chain_builders {
        calls.sort_by(|a, b| {
            a.strike_price
                .partial_cmp(&b.strike_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        puts.sort_by(|a, b| {
            a.strike_price
                .partial_cmp(&b.strike_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let future_security_id = futures_by_key.get(&key).copied();

        option_chains.insert(
            key.clone(),
            OptionChain {
                underlying_symbol: key.underlying_symbol.clone(),
                expiry_date: key.expiry_date,
                calls,
                puts,
                future_security_id,
            },
        );
    }

    info!(
        option_chain_count = option_chains.len(),
        "Pass 5: option chains built"
    );

    // --- Step 4: Build expiry calendars (already sorted via BTreeSet) ---
    let mut expiry_calendars: HashMap<String, ExpiryCalendar> =
        HashMap::with_capacity(expiry_sets.len());

    for (symbol, dates) in expiry_sets {
        expiry_calendars.insert(
            symbol.clone(),
            ExpiryCalendar {
                underlying_symbol: symbol,
                expiry_dates: dates.into_iter().collect(), // BTreeSet → sorted Vec
            },
        );
    }

    info!(
        expiry_calendar_count = expiry_calendars.len(),
        "Pass 5: expiry calendars built"
    );

    // --- Step 5: Update contract_count on underlyings ---
    for (symbol, count) in &contract_counts {
        if let Some(underlying) = underlyings.get_mut(symbol) {
            underlying.contract_count = *count;
        }
    }

    Pass5Result {
        derivative_contracts,
        instrument_info,
        option_chains,
        expiry_calendars,
    }
}

// ---------------------------------------------------------------------------
// Subscribed Indices Builder (8 F&O + 23 Display = 31)
// ---------------------------------------------------------------------------

/// Build the list of 31 subscribed indices from F&O underlyings and display index constants.
///
/// F&O indices (8): extracted from the linked underlyings where kind is NseIndex or BseIndex.
/// Display indices (23): from the `DISPLAY_INDEX_ENTRIES` compile-time constant.
///
/// All indices use the IDX_I exchange segment for WebSocket subscriptions.
fn build_subscribed_indices(underlyings: &HashMap<String, FnoUnderlying>) -> Vec<SubscribedIndex> {
    let mut indices = Vec::with_capacity(TOTAL_SUBSCRIBED_INDEX_COUNT);

    // 8 F&O index underlyings — extract from the linked underlyings map
    for underlying in underlyings.values() {
        let exchange = match underlying.kind {
            UnderlyingKind::NseIndex => Exchange::NationalStockExchange,
            UnderlyingKind::BseIndex => Exchange::BombayStockExchange,
            UnderlyingKind::Stock => continue, // Skip stocks, only indices
        };

        indices.push(SubscribedIndex {
            symbol: underlying.underlying_symbol.clone(),
            security_id: underlying.price_feed_security_id,
            exchange,
            category: IndexCategory::FnoUnderlying,
            subcategory: IndexSubcategory::Fno,
        });
    }

    // 23 display indices — from compile-time constant
    for &(name, security_id, subcategory_str) in DISPLAY_INDEX_ENTRIES {
        let subcategory = match subcategory_str {
            "Volatility" => IndexSubcategory::Volatility,
            "BroadMarket" => IndexSubcategory::BroadMarket,
            "MidCap" => IndexSubcategory::MidCap,
            "SmallCap" => IndexSubcategory::SmallCap,
            "Sectoral" => IndexSubcategory::Sectoral,
            "Thematic" => IndexSubcategory::Thematic,
            _ => {
                warn!(
                    index_name = name,
                    subcategory = subcategory_str,
                    "unknown display index subcategory, defaulting to Thematic"
                );
                IndexSubcategory::Thematic
            }
        };

        indices.push(SubscribedIndex {
            symbol: name.to_string(),
            security_id,
            exchange: Exchange::NationalStockExchange, // All display indices are NSE
            category: IndexCategory::DisplayIndex,
            subcategory,
        });
    }

    info!(
        fno_index_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::FnoUnderlying)
            .count(),
        display_index_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .count(),
        total = indices.len(),
        "subscribed indices built"
    );

    indices
}

// ---------------------------------------------------------------------------
// Public Orchestrator
// ---------------------------------------------------------------------------

/// Build the complete F&O universe from scratch.
///
/// Downloads the instrument CSV (with retry, fallback, and cache), parses it,
/// runs the 5-pass mapping algorithm, validates the result, and returns the
/// fully-built [`FnoUniverse`].
///
/// # Arguments
/// * `config` — Instrument configuration (URLs, cache paths, timeouts).
/// * `dhan_config` — Dhan API configuration (CSV URLs).
///
/// # Errors
/// Returns an error if:
/// - All CSV download sources fail (primary, fallback, cache).
/// - CSV parsing fails (missing columns, malformed data).
/// - Validation fails (must-exist checks, count checks).
pub async fn build_fno_universe(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
) -> Result<FnoUniverse> {
    let build_start = Instant::now();

    // Step 1: Download CSV
    info!("starting instrument CSV download");
    let download_result = download_instrument_csv(
        dhan_csv_url,
        dhan_csv_fallback_url,
        &instrument_config.csv_cache_directory,
        &instrument_config.csv_cache_filename,
        instrument_config.csv_download_timeout_secs,
    )
    .await
    .context("instrument CSV download failed")?;

    info!(
        source = %download_result.source,
        bytes = download_result.csv_text.len(),
        "instrument CSV obtained"
    );

    // Step 2: Parse CSV
    let (csv_row_count, parsed_rows) =
        parse_instrument_csv(&download_result.csv_text).context("instrument CSV parsing failed")?;

    info!(
        csv_row_count,
        parsed_row_count = parsed_rows.len(),
        "instrument CSV parsed"
    );

    // Step 3: Pass 1 — Index lookup
    let index_lookup = build_index_lookup(&parsed_rows);

    // Step 4: Pass 2 — Equity lookup
    let equity_lookup = build_equity_lookup(&parsed_rows);

    // Step 5: Pass 3 — Discover F&O underlyings
    let unlinked = discover_fno_underlyings(&parsed_rows);

    // Step 6: Pass 4 — Link price IDs
    let mut underlyings = link_price_ids(unlinked, &index_lookup, &equity_lookup);

    // Step 7: Pass 5 — Build derivatives, option chains, expiry calendars
    let ist_offset =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).context("invalid IST offset seconds")?;
    let today = Utc::now().with_timezone(&ist_offset).date_naive();

    let pass5_result = build_derivatives_and_chains(&parsed_rows, &mut underlyings, today);

    // Step 7b: Build subscribed indices (8 F&O + 23 Display)
    let subscribed_indices = build_subscribed_indices(&underlyings);

    // Assemble the universe
    let build_duration = build_start.elapsed();
    let build_timestamp = Utc::now().with_timezone(&ist_offset);

    let universe = FnoUniverse {
        build_metadata: UniverseBuildMetadata {
            csv_source: download_result.source,
            csv_row_count,
            parsed_row_count: parsed_rows.len(),
            index_count: index_lookup.len(),
            equity_count: equity_lookup.len(),
            underlying_count: underlyings.len(),
            derivative_count: pass5_result.derivative_contracts.len(),
            option_chain_count: pass5_result.option_chains.len(),
            build_duration,
            build_timestamp,
        },
        underlyings,
        derivative_contracts: pass5_result.derivative_contracts,
        instrument_info: pass5_result.instrument_info,
        option_chains: pass5_result.option_chains,
        expiry_calendars: pass5_result.expiry_calendars,
        subscribed_indices,
    };

    // Step 8: Validate
    validate_fno_universe(&universe).context("F&O universe validation failed")?;

    info!(
        underlyings = universe.underlyings.len(),
        derivatives = universe.derivative_contracts.len(),
        option_chains = universe.option_chains.len(),
        expiry_calendars = universe.expiry_calendars.len(),
        instrument_info = universe.instrument_info.len(),
        subscribed_indices = universe.subscribed_indices.len(),
        build_ms = build_duration.as_millis(),
        "F&O universe built successfully"
    );

    Ok(universe)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test Helpers — build ParsedInstrumentRow instances for each segment
    // -----------------------------------------------------------------------

    fn make_index_row(
        security_id: SecurityId,
        symbol: &str,
        exchange: Exchange,
    ) -> ParsedInstrumentRow {
        ParsedInstrumentRow {
            exchange,
            segment: 'I',
            security_id,
            instrument: "INDEX".to_owned(),
            underlying_security_id: security_id,
            underlying_symbol: symbol.to_owned(),
            symbol_name: symbol.to_owned(),
            display_name: format!("{} Index", symbol),
            series: "NA".to_owned(),
            lot_size: 1,
            expiry_date: None,
            strike_price: 0.0,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: "N".to_owned(),
        }
    }

    fn make_equity_row(security_id: SecurityId, symbol: &str) -> ParsedInstrumentRow {
        ParsedInstrumentRow {
            exchange: Exchange::NationalStockExchange,
            segment: 'E',
            security_id,
            instrument: "EQUITY".to_owned(),
            underlying_security_id: 0,
            underlying_symbol: symbol.to_owned(),
            symbol_name: format!("{} LTD", symbol),
            display_name: symbol.to_owned(),
            series: "EQ".to_owned(),
            lot_size: 1,
            expiry_date: None,
            strike_price: 0.0,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: "NA".to_owned(),
        }
    }

    fn make_futidx_row(
        security_id: SecurityId,
        underlying_id: SecurityId,
        symbol: &str,
        expiry: &str,
        lot_size: u32,
        exchange: Exchange,
    ) -> ParsedInstrumentRow {
        ParsedInstrumentRow {
            exchange,
            segment: 'D',
            security_id,
            instrument: "FUTIDX".to_owned(),
            underlying_security_id: underlying_id,
            underlying_symbol: symbol.to_owned(),
            symbol_name: format!("{}-{}-FUT", symbol, expiry),
            display_name: format!("{} FUT", symbol),
            series: "NA".to_owned(),
            lot_size,
            expiry_date: NaiveDate::parse_from_str(expiry, "%Y-%m-%d").ok(),
            strike_price: -0.01,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: "M".to_owned(),
        }
    }

    fn make_futstk_row(
        security_id: SecurityId,
        underlying_id: SecurityId,
        symbol: &str,
        expiry: &str,
        lot_size: u32,
    ) -> ParsedInstrumentRow {
        ParsedInstrumentRow {
            exchange: Exchange::NationalStockExchange,
            segment: 'D',
            security_id,
            instrument: "FUTSTK".to_owned(),
            underlying_security_id: underlying_id,
            underlying_symbol: symbol.to_owned(),
            symbol_name: format!("{}-{}-FUT", symbol, expiry),
            display_name: format!("{} FUT", symbol),
            series: "NA".to_owned(),
            lot_size,
            expiry_date: NaiveDate::parse_from_str(expiry, "%Y-%m-%d").ok(),
            strike_price: -0.01,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: "M".to_owned(),
        }
    }

    fn make_optidx_row(
        security_id: SecurityId,
        underlying_id: SecurityId,
        symbol: &str,
        expiry: &str,
        strike: f64,
        opt_type: OptionType,
        lot_size: u32,
    ) -> ParsedInstrumentRow {
        let option_suffix = match opt_type {
            OptionType::Call => "CE",
            OptionType::Put => "PE",
        };
        ParsedInstrumentRow {
            exchange: Exchange::NationalStockExchange,
            segment: 'D',
            security_id,
            instrument: "OPTIDX".to_owned(),
            underlying_security_id: underlying_id,
            underlying_symbol: symbol.to_owned(),
            symbol_name: format!("{}-{}-{:.0}-{}", symbol, expiry, strike, option_suffix),
            display_name: format!("{} {} {:.0} {}", symbol, expiry, strike, option_suffix),
            series: "NA".to_owned(),
            lot_size,
            expiry_date: NaiveDate::parse_from_str(expiry, "%Y-%m-%d").ok(),
            strike_price: strike,
            option_type: Some(opt_type),
            tick_size: 0.05,
            expiry_flag: "M".to_owned(),
        }
    }

    /// Build a representative set of parsed rows for integration testing.
    fn build_test_rows() -> Vec<ParsedInstrumentRow> {
        vec![
            // --- Pass 1: Indices ---
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_index_row(25, "BANKNIFTY", Exchange::NationalStockExchange),
            make_index_row(27, "FINNIFTY", Exchange::NationalStockExchange),
            make_index_row(442, "MIDCPNIFTY", Exchange::NationalStockExchange),
            make_index_row(38, "NIFTY NEXT 50", Exchange::NationalStockExchange),
            make_index_row(51, "SENSEX", Exchange::BombayStockExchange),
            make_index_row(69, "BANKEX", Exchange::BombayStockExchange),
            make_index_row(83, "SNSX50", Exchange::BombayStockExchange),
            // --- Pass 2: Equities ---
            make_equity_row(2885, "RELIANCE"),
            make_equity_row(1333, "HDFCBANK"),
            make_equity_row(1594, "INFY"),
            make_equity_row(11536, "TCS"),
            make_equity_row(5258, "SBIN"),
            // --- Pass 3: NSE FUTIDX ---
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            make_futidx_row(
                51701,
                26009,
                "BANKNIFTY",
                "2026-03-30",
                30,
                Exchange::NationalStockExchange,
            ),
            make_futidx_row(
                51712,
                26037,
                "FINNIFTY",
                "2026-03-30",
                60,
                Exchange::NationalStockExchange,
            ),
            make_futidx_row(
                51713,
                26074,
                "MIDCPNIFTY",
                "2026-03-30",
                120,
                Exchange::NationalStockExchange,
            ),
            // NIFTYNXT50 — alias case
            make_futidx_row(
                51714,
                26041,
                "NIFTYNXT50",
                "2026-03-30",
                40,
                Exchange::NationalStockExchange,
            ),
            // --- Pass 3: BSE FUTIDX ---
            make_futidx_row(
                60000,
                1,
                "SENSEX",
                "2026-03-30",
                20,
                Exchange::BombayStockExchange,
            ),
            make_futidx_row(
                60001,
                2,
                "BANKEX",
                "2026-03-30",
                30,
                Exchange::BombayStockExchange,
            ),
            // SENSEX50 — alias case
            make_futidx_row(
                60002,
                3,
                "SENSEX50",
                "2026-03-30",
                25,
                Exchange::BombayStockExchange,
            ),
            // --- Pass 3: NSE FUTSTK ---
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
            make_futstk_row(52024, 1333, "HDFCBANK", "2026-03-30", 550),
            make_futstk_row(52025, 1594, "INFY", "2026-03-30", 400),
            make_futstk_row(52026, 11536, "TCS", "2026-03-30", 175),
            make_futstk_row(52027, 5258, "SBIN", "2026-03-30", 1500),
            // --- TEST instrument (should be skipped) ---
            make_futstk_row(99998, 99999, "TESTSTOCK", "2026-03-30", 100),
            // --- Options: NIFTY March (2 strikes × 2 types = 4) ---
            make_optidx_row(
                70001,
                26000,
                "NIFTY",
                "2026-03-30",
                22000.0,
                OptionType::Call,
                75,
            ),
            make_optidx_row(
                70002,
                26000,
                "NIFTY",
                "2026-03-30",
                22000.0,
                OptionType::Put,
                75,
            ),
            make_optidx_row(
                70003,
                26000,
                "NIFTY",
                "2026-03-30",
                22500.0,
                OptionType::Call,
                75,
            ),
            make_optidx_row(
                70004,
                26000,
                "NIFTY",
                "2026-03-30",
                22500.0,
                OptionType::Put,
                75,
            ),
            // --- Options: NIFTY April (1 strike × 2 types = 2) ---
            make_optidx_row(
                70005,
                26000,
                "NIFTY",
                "2026-04-30",
                22000.0,
                OptionType::Call,
                75,
            ),
            make_optidx_row(
                70006,
                26000,
                "NIFTY",
                "2026-04-30",
                22000.0,
                OptionType::Put,
                75,
            ),
        ]
    }

    // -----------------------------------------------------------------------
    // Pass 1 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_index_lookup_finds_nse_indices() {
        let rows = build_test_rows();
        let lookup = build_index_lookup(&rows);

        let nifty = lookup.get("NIFTY").expect("NIFTY must be in index lookup");
        assert_eq!(nifty.security_id, 13);
        assert_eq!(nifty.exchange, Exchange::NationalStockExchange);

        let banknifty = lookup
            .get("BANKNIFTY")
            .expect("BANKNIFTY must be in index lookup");
        assert_eq!(banknifty.security_id, 25);
    }

    #[test]
    fn test_build_index_lookup_finds_bse_indices() {
        let rows = build_test_rows();
        let lookup = build_index_lookup(&rows);

        let sensex = lookup
            .get("SENSEX")
            .expect("SENSEX must be in index lookup");
        assert_eq!(sensex.security_id, 51);
        assert_eq!(sensex.exchange, Exchange::BombayStockExchange);

        let snsx50 = lookup
            .get("SNSX50")
            .expect("SNSX50 must be in index lookup");
        assert_eq!(snsx50.security_id, 83);
    }

    #[test]
    fn test_build_index_lookup_skips_non_index_rows() {
        let rows = build_test_rows();
        let lookup = build_index_lookup(&rows);

        // Equities should not appear
        assert!(lookup.get("RELIANCE").is_none());
        // Derivatives should not appear
        assert!(lookup.get("NIFTY-2026-03-30-FUT").is_none());
    }

    #[test]
    fn test_build_index_lookup_empty_input() {
        let lookup = build_index_lookup(&[]);
        assert!(lookup.is_empty());
    }

    #[test]
    fn test_build_index_lookup_correct_count() {
        let rows = build_test_rows();
        let lookup = build_index_lookup(&rows);
        // 5 NSE indices + 3 BSE indices = 8
        assert_eq!(lookup.len(), 8);
    }

    // -----------------------------------------------------------------------
    // Pass 2 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_equity_lookup_finds_nse_equities() {
        let rows = build_test_rows();
        let lookup = build_equity_lookup(&rows);

        assert_eq!(lookup.get("RELIANCE"), Some(&2885));
        assert_eq!(lookup.get("HDFCBANK"), Some(&1333));
        assert_eq!(lookup.get("INFY"), Some(&1594));
        assert_eq!(lookup.get("TCS"), Some(&11536));
        assert_eq!(lookup.get("SBIN"), Some(&5258));
    }

    #[test]
    fn test_build_equity_lookup_skips_indices() {
        let rows = build_test_rows();
        let lookup = build_equity_lookup(&rows);

        assert!(lookup.get("NIFTY").is_none());
        assert!(lookup.get("SENSEX").is_none());
    }

    #[test]
    fn test_build_equity_lookup_correct_count() {
        let rows = build_test_rows();
        let lookup = build_equity_lookup(&rows);
        assert_eq!(lookup.len(), 5);
    }

    #[test]
    fn test_build_equity_lookup_empty_input() {
        let lookup = build_equity_lookup(&[]);
        assert!(lookup.is_empty());
    }

    #[test]
    fn test_build_equity_lookup_skips_bse_equities() {
        let rows = vec![ParsedInstrumentRow {
            exchange: Exchange::BombayStockExchange,
            segment: 'E',
            security_id: 9999,
            instrument: "EQUITY".to_owned(),
            underlying_security_id: 0,
            underlying_symbol: "RELIANCE".to_owned(),
            symbol_name: "RELIANCE".to_owned(),
            display_name: "RELIANCE".to_owned(),
            series: "EQ".to_owned(),
            lot_size: 1,
            expiry_date: None,
            strike_price: 0.0,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: "NA".to_owned(),
        }];
        let lookup = build_equity_lookup(&rows);
        assert!(lookup.is_empty());
    }

    #[test]
    fn test_build_equity_lookup_skips_non_eq_series() {
        let mut row = make_equity_row(9999, "SOMETICKER");
        row.series = "BE".to_owned(); // Not "EQ"
        let lookup = build_equity_lookup(&[row]);
        assert!(lookup.is_empty());
    }

    // -----------------------------------------------------------------------
    // Pass 3 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_finds_nse_index_futures() {
        let rows = build_test_rows();
        let underlyings = discover_fno_underlyings(&rows);

        let nifty = underlyings
            .iter()
            .find(|u| u.underlying_symbol == "NIFTY")
            .expect("NIFTY must be discovered");
        assert_eq!(nifty.kind, UnderlyingKind::NseIndex);
        assert_eq!(nifty.underlying_security_id, 26000);
        assert_eq!(nifty.lot_size, 75);
    }

    #[test]
    fn test_discover_fno_underlyings_finds_bse_index_futures() {
        let rows = build_test_rows();
        let underlyings = discover_fno_underlyings(&rows);

        let sensex = underlyings
            .iter()
            .find(|u| u.underlying_symbol == "SENSEX")
            .expect("SENSEX must be discovered");
        assert_eq!(sensex.kind, UnderlyingKind::BseIndex);
        assert_eq!(sensex.lot_size, 20);
    }

    #[test]
    fn test_discover_fno_underlyings_finds_stock_futures() {
        let rows = build_test_rows();
        let underlyings = discover_fno_underlyings(&rows);

        let reliance = underlyings
            .iter()
            .find(|u| u.underlying_symbol == "RELIANCE")
            .expect("RELIANCE must be discovered");
        assert_eq!(reliance.kind, UnderlyingKind::Stock);
        assert_eq!(reliance.underlying_security_id, 2885);
        assert_eq!(reliance.lot_size, 500);
    }

    #[test]
    fn test_discover_fno_underlyings_skips_test_instruments() {
        let rows = build_test_rows();
        let underlyings = discover_fno_underlyings(&rows);

        let test_found = underlyings
            .iter()
            .any(|u| u.underlying_symbol.contains("TEST"));
        assert!(!test_found, "TEST instruments must be skipped");
    }

    #[test]
    fn test_discover_fno_underlyings_deduplicates() {
        // Add duplicate NIFTY futures with different expiries
        let rows = vec![
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            make_futidx_row(
                51800,
                26000,
                "NIFTY",
                "2026-04-30",
                75,
                Exchange::NationalStockExchange,
            ),
            make_futidx_row(
                51900,
                26000,
                "NIFTY",
                "2026-06-30",
                75,
                Exchange::NationalStockExchange,
            ),
        ];

        let underlyings = discover_fno_underlyings(&rows);
        let nifty_count = underlyings
            .iter()
            .filter(|u| u.underlying_symbol == "NIFTY")
            .count();
        assert_eq!(nifty_count, 1, "NIFTY must appear exactly once");
    }

    #[test]
    fn test_discover_fno_underlyings_skips_option_rows() {
        let rows = vec![make_optidx_row(
            70001,
            26000,
            "NIFTY",
            "2026-03-30",
            22000.0,
            OptionType::Call,
            75,
        )];

        let underlyings = discover_fno_underlyings(&rows);
        assert!(
            underlyings.is_empty(),
            "options-only input should yield no underlyings"
        );
    }

    #[test]
    fn test_discover_fno_underlyings_correct_total_count() {
        let rows = build_test_rows();
        let underlyings = discover_fno_underlyings(&rows);

        // 4 NSE indices + 1 NIFTYNXT50 + 3 BSE indices + 5 stocks - 1 TEST = 12
        // NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, NIFTYNXT50,
        // SENSEX, BANKEX, SENSEX50,
        // RELIANCE, HDFCBANK, INFY, TCS, SBIN
        assert_eq!(underlyings.len(), 13);
    }

    // -----------------------------------------------------------------------
    // Pass 4 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_link_price_ids_direct_index_match() {
        let rows = build_test_rows();
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let nifty = linked.get("NIFTY").expect("NIFTY must be linked");
        assert_eq!(nifty.price_feed_security_id, 13); // IDX_I ID, not phantom 26000
        assert_eq!(nifty.price_feed_segment, ExchangeSegment::IdxI);
        assert_eq!(nifty.derivative_segment, ExchangeSegment::NseFno);
        assert_eq!(nifty.underlying_security_id, 26000); // Phantom ID preserved
    }

    #[test]
    fn test_link_price_ids_alias_niftynxt50() {
        let rows = build_test_rows();
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        // NIFTYNXT50 alias → "NIFTY NEXT 50" → security_id 38
        let nxt50 = linked.get("NIFTYNXT50").expect("NIFTYNXT50 must be linked");
        assert_eq!(nxt50.price_feed_security_id, 38);
        assert_eq!(nxt50.price_feed_segment, ExchangeSegment::IdxI);
        assert_eq!(nxt50.kind, UnderlyingKind::NseIndex);
    }

    #[test]
    fn test_link_price_ids_alias_sensex50() {
        let rows = build_test_rows();
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        // SENSEX50 alias → "SNSX50" → security_id 83
        let snsx50 = linked.get("SENSEX50").expect("SENSEX50 must be linked");
        assert_eq!(snsx50.price_feed_security_id, 83);
        assert_eq!(snsx50.price_feed_segment, ExchangeSegment::IdxI);
        assert_eq!(snsx50.derivative_segment, ExchangeSegment::BseFno);
        assert_eq!(snsx50.kind, UnderlyingKind::BseIndex);
    }

    #[test]
    fn test_link_price_ids_stock_match() {
        let rows = build_test_rows();
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let reliance = linked.get("RELIANCE").expect("RELIANCE must be linked");
        assert_eq!(reliance.price_feed_security_id, 2885); // NSE_EQ ID
        assert_eq!(reliance.price_feed_segment, ExchangeSegment::NseEquity);
        assert_eq!(reliance.derivative_segment, ExchangeSegment::NseFno);
        assert_eq!(reliance.kind, UnderlyingKind::Stock);
        assert_eq!(reliance.lot_size, 500);
    }

    #[test]
    fn test_link_price_ids_bse_index_direct_match() {
        let rows = build_test_rows();
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let sensex = linked.get("SENSEX").expect("SENSEX must be linked");
        assert_eq!(sensex.price_feed_security_id, 51);
        assert_eq!(sensex.derivative_segment, ExchangeSegment::BseFno);
        assert_eq!(sensex.kind, UnderlyingKind::BseIndex);
    }

    #[test]
    fn test_link_price_ids_missing_index_uses_fallback() {
        // Create an underlying that has no matching index entry
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "UNKNOWNIDX".to_owned(),
            underlying_security_id: 99999,
            kind: UnderlyingKind::NseIndex,
            lot_size: 50,
            derivative_exchange: Exchange::NationalStockExchange,
        }];

        let index_lookup = HashMap::new();
        let equity_lookup = HashMap::new();

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let unknown = linked
            .get("UNKNOWNIDX")
            .expect("UNKNOWNIDX must be in result");
        // Falls back to underlying_security_id
        assert_eq!(unknown.price_feed_security_id, 99999);
    }

    #[test]
    fn test_link_price_ids_missing_stock_uses_fallback() {
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "UNKNOWNSTK".to_owned(),
            underlying_security_id: 88888,
            kind: UnderlyingKind::Stock,
            lot_size: 100,
            derivative_exchange: Exchange::NationalStockExchange,
        }];

        let index_lookup = HashMap::new();
        let equity_lookup = HashMap::new();

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let unknown = linked
            .get("UNKNOWNSTK")
            .expect("UNKNOWNSTK must be in result");
        assert_eq!(unknown.price_feed_security_id, 88888);
    }

    #[test]
    fn test_link_price_ids_preserves_contract_count_zero() {
        let rows = build_test_rows();
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);

        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        // All contract_count should be 0 (filled later in Pass 5)
        for underlying in linked.values() {
            assert_eq!(
                underlying.contract_count, 0,
                "contract_count must be 0 after Pass 4 (filled in Pass 5)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Integration: Passes 1-4 combined
    // -----------------------------------------------------------------------

    #[test]
    fn test_passes_1_through_4_end_to_end() {
        let rows = build_test_rows();

        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);
        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        // All 13 underlyings should be present
        assert_eq!(linked.len(), 13);

        // Verify classification counts
        let nse_index_count = linked
            .values()
            .filter(|u| u.kind == UnderlyingKind::NseIndex)
            .count();
        let bse_index_count = linked
            .values()
            .filter(|u| u.kind == UnderlyingKind::BseIndex)
            .count();
        let stock_count = linked
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();

        assert_eq!(nse_index_count, 5); // NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, NIFTYNXT50
        assert_eq!(bse_index_count, 3); // SENSEX, BANKEX, SENSEX50
        assert_eq!(stock_count, 5); // RELIANCE, HDFCBANK, INFY, TCS, SBIN

        // Verify all have price_feed_security_id != 0
        for (symbol, underlying) in &linked {
            assert_ne!(
                underlying.price_feed_security_id, 0,
                "{} must have a valid price_feed_security_id",
                symbol
            );
        }
    }

    // -----------------------------------------------------------------------
    // Pass 5 Helpers
    // -----------------------------------------------------------------------

    /// Run passes 1-4 and return underlyings ready for Pass 5.
    fn run_passes_1_through_4(rows: &[ParsedInstrumentRow]) -> HashMap<String, FnoUnderlying> {
        let index_lookup = build_index_lookup(rows);
        let equity_lookup = build_equity_lookup(rows);
        let unlinked = discover_fno_underlyings(rows);
        link_price_ids(unlinked, &index_lookup, &equity_lookup)
    }

    fn test_today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 2, 25).unwrap()
    }

    // -----------------------------------------------------------------------
    // Pass 5 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_builds_derivative_contracts() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // All non-expired derivatives should be present
        // Futures: NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY, NIFTYNXT50 (NSE) + SENSEX, BANKEX, SENSEX50 (BSE) + RELIANCE, HDFCBANK, INFY, TCS, SBIN = 13
        // Options: 2 NIFTY calls + 2 NIFTY puts = no, we have:
        //   NIFTY 2026-03-30 22000 CE, 22000 PE, 22500 CE, 22500 PE
        //   NIFTY 2026-04-30 22000 CE, 22000 PE
        // = 6 options
        // 1 expired future (2025-01-30) should be skipped
        // TESTSTOCK future should be skipped
        // Total: 13 futures + 6 options = 19
        assert_eq!(result.derivative_contracts.len(), 19);
    }

    #[test]
    fn test_pass5_skips_expired_contracts() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // The expired NIFTY future (2025-01-30) should not be in contracts
        let expired = result.derivative_contracts.get(&51600);
        assert!(
            expired.is_none(),
            "expired contract (2025-01-30) must be filtered out"
        );
    }

    #[test]
    fn test_pass5_skips_test_instruments() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let has_test = result
            .derivative_contracts
            .values()
            .any(|c| c.underlying_symbol.contains("TEST"));
        assert!(!has_test, "TEST instruments must be skipped in Pass 5");
    }

    #[test]
    fn test_pass5_builds_option_chains_sorted_by_strike() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let march_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };

        let chain = result
            .option_chains
            .get(&march_key)
            .expect("NIFTY March option chain must exist");

        // Should have 2 calls (22000, 22500) and 2 puts (22000, 22500)
        assert_eq!(chain.calls.len(), 2);
        assert_eq!(chain.puts.len(), 2);

        // Calls sorted ascending by strike
        assert_eq!(chain.calls[0].strike_price, 22000.0);
        assert_eq!(chain.calls[1].strike_price, 22500.0);

        // Puts sorted ascending by strike
        assert_eq!(chain.puts[0].strike_price, 22000.0);
        assert_eq!(chain.puts[1].strike_price, 22500.0);
    }

    #[test]
    fn test_pass5_option_chain_links_future() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let march_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };

        let chain = result
            .option_chains
            .get(&march_key)
            .expect("NIFTY March chain must exist");

        // Should be linked to the NIFTY March future (security_id 51700)
        assert_eq!(chain.future_security_id, Some(51700));
    }

    #[test]
    fn test_pass5_option_chain_no_future_for_orphan_expiry() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let april_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 30).unwrap(),
        };

        let chain = result
            .option_chains
            .get(&april_key)
            .expect("NIFTY April chain must exist");

        // No April future in test data → future_security_id should be None
        assert_eq!(chain.future_security_id, None);
    }

    #[test]
    fn test_pass5_builds_expiry_calendars_sorted() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_calendar = result
            .expiry_calendars
            .get("NIFTY")
            .expect("NIFTY expiry calendar must exist");

        // NIFTY has March (future+options) and April (options only) expiries
        assert_eq!(nifty_calendar.expiry_dates.len(), 2);

        // Must be sorted ascending
        assert_eq!(
            nifty_calendar.expiry_dates[0],
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()
        );
        assert_eq!(
            nifty_calendar.expiry_dates[1],
            NaiveDate::from_ymd_opt(2026, 4, 30).unwrap()
        );
    }

    #[test]
    fn test_pass5_instrument_info_includes_indices() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // NIFTY index (security_id 13) should be in instrument_info
        let nifty_info = result
            .instrument_info
            .get(&13)
            .expect("NIFTY must be in instrument_info");
        match nifty_info {
            InstrumentInfo::Index {
                security_id,
                symbol,
                exchange,
            } => {
                assert_eq!(*security_id, 13);
                assert_eq!(symbol, "NIFTY");
                assert_eq!(*exchange, Exchange::NationalStockExchange);
            }
            _ => panic!("NIFTY (13) must be an Index variant"),
        }
    }

    #[test]
    fn test_pass5_instrument_info_includes_equities() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let reliance_info = result
            .instrument_info
            .get(&2885)
            .expect("RELIANCE must be in instrument_info");
        match reliance_info {
            InstrumentInfo::Equity {
                security_id,
                symbol,
            } => {
                assert_eq!(*security_id, 2885);
                assert_eq!(symbol, "RELIANCE");
            }
            _ => panic!("RELIANCE (2885) must be an Equity variant"),
        }
    }

    #[test]
    fn test_pass5_instrument_info_includes_derivatives() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // NIFTY March 22000 CE (security_id 70001)
        let opt_info = result
            .instrument_info
            .get(&70001)
            .expect("NIFTY option must be in instrument_info");
        match opt_info {
            InstrumentInfo::Derivative {
                security_id,
                underlying_symbol,
                instrument_kind,
                strike_price,
                option_type,
                ..
            } => {
                assert_eq!(*security_id, 70001);
                assert_eq!(underlying_symbol, "NIFTY");
                assert_eq!(*instrument_kind, DhanInstrumentKind::OptionIndex);
                assert_eq!(*strike_price, 22000.0);
                assert_eq!(*option_type, Some(OptionType::Call));
            }
            _ => panic!("70001 must be a Derivative variant"),
        }
    }

    #[test]
    fn test_pass5_updates_contract_count_on_underlyings() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let _result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // NIFTY should have: 1 future + 4 options (March) + 2 options (April) = 7
        // But the expired future (2025-01-30) is filtered, so = 7
        let nifty = underlyings.get("NIFTY").expect("NIFTY must exist");
        assert_eq!(nifty.contract_count, 7);

        // RELIANCE should have: 1 future
        let reliance = underlyings.get("RELIANCE").expect("RELIANCE must exist");
        assert_eq!(reliance.contract_count, 1);
    }

    #[test]
    fn test_pass5_normalizes_negative_strike_price() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Futures have strike_price = -0.01 in CSV, should be normalized to 0.0
        let nifty_future = result
            .derivative_contracts
            .get(&51700)
            .expect("NIFTY future must exist");
        assert_eq!(
            nifty_future.strike_price, 0.0,
            "negative strike must be normalized to 0.0"
        );
    }

    // -----------------------------------------------------------------------
    // Integration: All 5 passes combined
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_five_passes_end_to_end() {
        let rows = build_test_rows();

        // Passes 1-4
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);
        let mut underlyings = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        // Pass 5
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Verify final counts
        assert_eq!(underlyings.len(), 13);
        assert_eq!(result.derivative_contracts.len(), 19);

        // instrument_info = 8 indices + 5 equities + 19 derivatives = 32
        assert_eq!(result.instrument_info.len(), 32);

        // Option chains: NIFTY March + NIFTY April = 2
        assert_eq!(result.option_chains.len(), 2);

        // Expiry calendars: one per underlying that has >=1 non-expired contract
        assert!(result.expiry_calendars.len() >= 13);

        // All underlyings should have contract_count > 0 (except those with no derivatives)
        let nifty = underlyings.get("NIFTY").unwrap();
        assert!(nifty.contract_count > 0, "NIFTY must have contracts");
    }

    // -----------------------------------------------------------------------
    // Subscribed Indices Builder Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_fno_count() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        // Test data has 8 index underlyings: 5 NSE + 3 BSE
        let fno_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::FnoUnderlying)
            .count();
        assert_eq!(fno_count, 8, "test data has 8 F&O index underlyings");
    }

    #[test]
    fn test_build_subscribed_indices_display_count() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        let display_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .count();
        assert_eq!(
            display_count, DISPLAY_INDEX_COUNT,
            "display indices must match constant"
        );
    }

    #[test]
    fn test_build_subscribed_indices_total_count() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        // 8 F&O indices (from test data) + 23 display = 31
        assert_eq!(indices.len(), 8 + DISPLAY_INDEX_COUNT);
    }

    #[test]
    fn test_build_subscribed_indices_skips_stock_underlyings() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        // No stock underlyings should appear (RELIANCE, HDFCBANK, etc. are stocks)
        for index in &indices {
            assert_ne!(index.symbol, "RELIANCE", "stock must not appear as index");
            assert_ne!(index.symbol, "HDFCBANK", "stock must not appear as index");
        }
    }

    #[test]
    fn test_build_subscribed_indices_fno_uses_price_feed_id() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        // NIFTY F&O index should use price_feed_security_id (IDX_I:13), not underlying_security_id (26000)
        let nifty = indices
            .iter()
            .find(|i| i.symbol == "NIFTY")
            .expect("NIFTY must be in subscribed indices");
        assert_eq!(
            nifty.security_id, 13,
            "must use IDX_I price feed ID, not FNO phantom ID"
        );
        assert_eq!(nifty.category, IndexCategory::FnoUnderlying);
        assert_eq!(nifty.subcategory, IndexSubcategory::Fno);
    }

    #[test]
    fn test_build_subscribed_indices_bse_index_has_correct_exchange() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        let sensex = indices
            .iter()
            .find(|i| i.symbol == "SENSEX")
            .expect("SENSEX must be in subscribed indices");
        assert_eq!(sensex.exchange, Exchange::BombayStockExchange);
    }

    #[test]
    fn test_build_subscribed_indices_display_subcategories() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);

        let indices = build_subscribed_indices(&underlyings);

        let vix = indices
            .iter()
            .find(|i| i.symbol == "INDIA VIX")
            .expect("INDIA VIX must be in subscribed indices");
        assert_eq!(vix.subcategory, IndexSubcategory::Volatility);
        assert_eq!(vix.category, IndexCategory::DisplayIndex);

        let nifty_auto = indices
            .iter()
            .find(|i| i.symbol == "NIFTY AUTO")
            .expect("NIFTY AUTO must be in subscribed indices");
        assert_eq!(nifty_auto.subcategory, IndexSubcategory::Sectoral);
    }

    // -----------------------------------------------------------------------
    // BSE Stock Derivative Filter — Test Helpers
    // -----------------------------------------------------------------------

    /// Create a BSE stock future row (FUTSTK with BSE exchange).
    fn make_futstk_row_bse(
        security_id: SecurityId,
        underlying_id: SecurityId,
        symbol: &str,
        expiry: &str,
        lot_size: u32,
    ) -> ParsedInstrumentRow {
        ParsedInstrumentRow {
            exchange: Exchange::BombayStockExchange,
            segment: 'D',
            security_id,
            instrument: "FUTSTK".to_owned(),
            underlying_security_id: underlying_id,
            underlying_symbol: symbol.to_owned(),
            symbol_name: format!("{}-{}-FUT", symbol, expiry),
            display_name: format!("{} FUT", symbol),
            series: "NA".to_owned(),
            lot_size,
            expiry_date: NaiveDate::parse_from_str(expiry, "%Y-%m-%d").ok(),
            strike_price: -0.01,
            option_type: None,
            tick_size: 0.05,
            expiry_flag: "M".to_owned(),
        }
    }

    /// Create a BSE stock option row (OPTSTK with BSE exchange).
    fn make_optstk_row_bse(
        security_id: SecurityId,
        underlying_id: SecurityId,
        symbol: &str,
        expiry: &str,
        strike: f64,
        opt_type: OptionType,
        lot_size: u32,
    ) -> ParsedInstrumentRow {
        let option_suffix = match opt_type {
            OptionType::Call => "CE",
            OptionType::Put => "PE",
        };
        ParsedInstrumentRow {
            exchange: Exchange::BombayStockExchange,
            segment: 'D',
            security_id,
            instrument: "OPTSTK".to_owned(),
            underlying_security_id: underlying_id,
            underlying_symbol: symbol.to_owned(),
            symbol_name: format!("{}-{}-{:.0}-{}", symbol, expiry, strike, option_suffix),
            display_name: format!("{} {} {:.0} {}", symbol, expiry, strike, option_suffix),
            series: "NA".to_owned(),
            lot_size,
            expiry_date: NaiveDate::parse_from_str(expiry, "%Y-%m-%d").ok(),
            strike_price: strike,
            option_type: Some(opt_type),
            tick_size: 0.05,
            expiry_flag: "M".to_owned(),
        }
    }

    // -----------------------------------------------------------------------
    // BSE Stock Derivative Filter — Pass 3 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_skips_bse_stock_futures() {
        // A row set with ONLY BSE FUTSTK → should produce zero stock underlyings
        let rows = vec![
            // Need an index row so equity lookup works, but the point is BSE FUTSTK
            make_futstk_row_bse(80001, 9001, "FORTIS", "2026-03-30", 200),
            make_futstk_row_bse(80002, 9002, "CAMS", "2026-03-30", 100),
            make_futstk_row_bse(80003, 9003, "TITAN", "2026-03-30", 50),
        ];

        let underlyings = discover_fno_underlyings(&rows);

        let stock_count = underlyings
            .iter()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert_eq!(
            stock_count, 0,
            "BSE FUTSTK rows must be filtered — zero stock underlyings expected"
        );
    }

    #[test]
    fn test_discover_fno_underlyings_keeps_bse_index_futures() {
        // BSE FUTIDX (SENSEX) should still pass through
        let rows = vec![make_futidx_row(
            60000,
            1,
            "SENSEX",
            "2026-03-30",
            20,
            Exchange::BombayStockExchange,
        )];

        let underlyings = discover_fno_underlyings(&rows);

        assert_eq!(underlyings.len(), 1, "BSE FUTIDX must not be filtered");
        assert_eq!(underlyings[0].kind, UnderlyingKind::BseIndex);
        assert_eq!(underlyings[0].underlying_symbol, "SENSEX");
    }

    #[test]
    fn test_discover_fno_underlyings_keeps_nse_stock_futures() {
        // NSE FUTSTK (RELIANCE) should still pass through
        let rows = vec![make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500)];

        let underlyings = discover_fno_underlyings(&rows);

        assert_eq!(underlyings.len(), 1, "NSE FUTSTK must not be filtered");
        assert_eq!(underlyings[0].kind, UnderlyingKind::Stock);
        assert_eq!(underlyings[0].underlying_symbol, "RELIANCE");
    }

    // -----------------------------------------------------------------------
    // BSE Stock Derivative Filter — Pass 5 Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_skips_bse_stock_futures() {
        // Build a universe with ONLY NSE underlyings + inject a BSE FUTSTK row
        // that references a known underlying. The BSE FUTSTK should be filtered.
        let mut rows = build_test_rows();
        // Add a BSE FUTSTK for RELIANCE (which is an NSE stock underlying)
        rows.push(make_futstk_row_bse(
            80010,
            2885,
            "RELIANCE",
            "2026-03-30",
            500,
        ));

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // The BSE FUTSTK (security_id 80010) must NOT be in contracts
        assert!(
            result.derivative_contracts.get(&80010).is_none(),
            "BSE FUTSTK must be filtered from derivative contracts"
        );
    }

    #[test]
    fn test_pass5_skips_bse_stock_options() {
        // Inject a BSE OPTSTK row for a known underlying
        let mut rows = build_test_rows();
        rows.push(make_optstk_row_bse(
            80020,
            2885,
            "RELIANCE",
            "2026-03-30",
            2000.0,
            OptionType::Call,
            500,
        ));

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // The BSE OPTSTK (security_id 80020) must NOT be in contracts
        assert!(
            result.derivative_contracts.get(&80020).is_none(),
            "BSE OPTSTK must be filtered from derivative contracts"
        );
    }

    #[test]
    fn test_pass5_keeps_bse_index_derivatives() {
        // BSE FUTIDX (SENSEX) should still pass through in Pass 5
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // SENSEX future (security_id 60000) should be present
        assert!(
            result.derivative_contracts.get(&60000).is_some(),
            "BSE FUTIDX (SENSEX) must be present in derivative contracts"
        );

        // BANKEX future (security_id 60001) should be present
        assert!(
            result.derivative_contracts.get(&60001).is_some(),
            "BSE FUTIDX (BANKEX) must be present in derivative contracts"
        );

        // SENSEX50 future (security_id 60002) should be present
        assert!(
            result.derivative_contracts.get(&60002).is_some(),
            "BSE FUTIDX (SENSEX50) must be present in derivative contracts"
        );
    }

    #[test]
    fn test_no_bse_stock_underlyings_in_universe() {
        // Add BSE FUTSTK rows to the standard test data — they should be fully
        // filtered out, leaving zero BSE stock underlyings in the final universe.
        let mut rows = build_test_rows();
        rows.push(make_futstk_row_bse(
            80001,
            9001,
            "FORTIS",
            "2026-03-30",
            200,
        ));
        rows.push(make_futstk_row_bse(80002, 9002, "CAMS", "2026-03-30", 100));
        rows.push(make_futstk_row_bse(80003, 9003, "TITAN", "2026-03-30", 50));
        rows.push(make_futstk_row_bse(
            80004,
            1333,
            "HDFCBANK",
            "2026-03-30",
            550,
        ));

        let underlyings = discover_fno_underlyings(&rows);

        // Count BSE stock underlyings — must be zero
        let bse_stock_count = underlyings
            .iter()
            .filter(|u| {
                u.kind == UnderlyingKind::Stock
                    && u.derivative_exchange == Exchange::BombayStockExchange
            })
            .count();
        assert_eq!(
            bse_stock_count, 0,
            "no BSE stock underlyings should exist in the universe"
        );

        // BSE index underlyings should still exist
        let bse_index_count = underlyings
            .iter()
            .filter(|u| u.kind == UnderlyingKind::BseIndex)
            .count();
        assert_eq!(
            bse_index_count, 3,
            "BSE index underlyings (SENSEX, BANKEX, SENSEX50) must still exist"
        );
    }
}
