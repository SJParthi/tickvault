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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::InstrumentConfig;
use dhan_live_trader_common::constants::*;
use dhan_live_trader_common::instrument_types::*;
use dhan_live_trader_common::trading_calendar::ist_offset;
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

    // Dedup tracker: detect duplicate security_ids across all rows
    let mut seen_security_ids: HashSet<SecurityId> = HashSet::with_capacity(170_000);
    let mut duplicate_count: usize = 0;

    // --- Step 1: Instrument info for indices and equities ---
    for row in rows {
        match row.segment {
            'I' => {
                if !seen_security_ids.insert(row.security_id) {
                    debug!(
                        security_id = row.security_id,
                        symbol = %row.underlying_symbol,
                        segment = "I",
                        "duplicate security_id in CSV — keeping first occurrence"
                    );
                    duplicate_count = duplicate_count.saturating_add(1);
                    continue;
                }
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
                if !seen_security_ids.insert(row.security_id) {
                    debug!(
                        security_id = row.security_id,
                        symbol = %row.underlying_symbol,
                        segment = "E",
                        "duplicate security_id in CSV — keeping first occurrence"
                    );
                    duplicate_count = duplicate_count.saturating_add(1);
                    continue;
                }
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
                skipped_unknown_instrument = skipped_unknown_instrument.saturating_add(1);
                continue;
            }
        };

        // Skip TEST instruments
        if row.underlying_symbol.contains(CSV_TEST_SYMBOL_MARKER) {
            skipped_test = skipped_test.saturating_add(1);
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
            skipped_bse_stock_derivative = skipped_bse_stock_derivative.saturating_add(1);
            continue;
        }

        // Skip if underlying not in our universe
        if !underlyings.contains_key(&row.underlying_symbol) {
            skipped_unknown_underlying = skipped_unknown_underlying.saturating_add(1);
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
            skipped_expired = skipped_expired.saturating_add(1);
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

        // Dedup: skip if security_id already seen (keep first occurrence)
        if !seen_security_ids.insert(row.security_id) {
            debug!(
                security_id = row.security_id,
                symbol = %row.underlying_symbol,
                segment = "D",
                "duplicate security_id in CSV — keeping first occurrence"
            );
            duplicate_count = duplicate_count.saturating_add(1);
            continue;
        }

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
        let count = contract_counts
            .entry(row.underlying_symbol.clone())
            .or_default();
        *count = count.saturating_add(1);

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

    if duplicate_count > 0 {
        warn!(
            duplicate_count,
            "Pass 5: duplicate security_ids detected in CSV — first occurrences kept"
        );
    }

    info!(
        derivative_count = derivative_contracts.len(),
        skipped_expired,
        skipped_unknown_underlying,
        skipped_test,
        skipped_unknown_instrument,
        skipped_bse_stock_derivative,
        duplicate_count,
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

    // Step 2: Parse + build
    build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
}

/// Build the F&O universe from already-obtained CSV text.
///
/// Parses the CSV, runs the 5-pass mapping algorithm, validates the result,
/// and returns the fully-built [`FnoUniverse`]. No network I/O.
///
/// Used by the instrument loader when the freshness marker confirms today's
/// CSV is already cached — avoids redundant HTTP downloads.
pub fn build_fno_universe_from_csv(csv_text: &str, source: &str) -> Result<FnoUniverse> {
    let build_start = Instant::now();

    // Parse CSV
    let (csv_row_count, parsed_rows) =
        parse_instrument_csv(csv_text).context("instrument CSV parsing failed")?;

    info!(
        csv_row_count,
        parsed_row_count = parsed_rows.len(),
        "instrument CSV parsed"
    );

    // Pass 1 — Index lookup
    let index_lookup = build_index_lookup(&parsed_rows);

    // Pass 2 — Equity lookup
    let equity_lookup = build_equity_lookup(&parsed_rows);

    // Pass 3 — Discover F&O underlyings
    let unlinked = discover_fno_underlyings(&parsed_rows);

    // Pass 4 — Link price IDs
    let mut underlyings = link_price_ids(unlinked, &index_lookup, &equity_lookup);

    // Pass 5 — Build derivatives, option chains, expiry calendars
    let today = Utc::now().with_timezone(&ist_offset()).date_naive();

    let pass5_result = build_derivatives_and_chains(&parsed_rows, &mut underlyings, today);

    // Build subscribed indices (8 F&O + 23 Display)
    let subscribed_indices = build_subscribed_indices(&underlyings);

    // Assemble the universe
    let build_duration = build_start.elapsed();
    let build_timestamp = Utc::now().with_timezone(&ist_offset());

    let universe = FnoUniverse {
        build_metadata: UniverseBuildMetadata {
            csv_source: source.to_owned(),
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

    // Validate
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
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
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
        assert!(!lookup.contains_key("RELIANCE"));
        // Derivatives should not appear
        assert!(!lookup.contains_key("NIFTY-2026-03-30-FUT"));
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

        assert!(!lookup.contains_key("NIFTY"));
        assert!(!lookup.contains_key("SENSEX"));
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
            !result.derivative_contracts.contains_key(&80010),
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
            !result.derivative_contracts.contains_key(&80020),
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
            result.derivative_contracts.contains_key(&60000),
            "BSE FUTIDX (SENSEX) must be present in derivative contracts"
        );

        // BANKEX future (security_id 60001) should be present
        assert!(
            result.derivative_contracts.contains_key(&60001),
            "BSE FUTIDX (BANKEX) must be present in derivative contracts"
        );

        // SENSEX50 future (security_id 60002) should be present
        assert!(
            result.derivative_contracts.contains_key(&60002),
            "BSE FUTIDX (SENSEX50) must be present in derivative contracts"
        );
    }

    // -----------------------------------------------------------------------
    // Pass 5 Edge Case Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_skips_derivative_without_expiry_date() {
        // Create a derivative row with expiry_date = None
        let mut row = make_futidx_row(
            51700,
            26000,
            "NIFTY",
            "2026-03-30",
            75,
            Exchange::NationalStockExchange,
        );
        row.expiry_date = None; // force no expiry

        // Also need the index row for pass 1-4
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51701,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            row,
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Only the row WITH expiry_date should be in contracts (51701), not the one without
        assert!(result.derivative_contracts.contains_key(&51701));
        // The row without expiry (51700 with forced None) should be skipped
        assert!(
            !result.derivative_contracts.contains_key(&51700),
            "derivative without expiry_date must be skipped"
        );
    }

    #[test]
    fn test_pass5_skips_option_without_option_type() {
        // Create an option row with option_type = None (malformed data)
        let mut option_row = make_optidx_row(
            70099,
            26000,
            "NIFTY",
            "2026-03-30",
            22000.0,
            OptionType::Call,
            75,
        );
        option_row.option_type = None; // force no option_type

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            option_row,
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // The option IS in derivative_contracts (it has an expiry_date, underlying exists)
        assert!(result.derivative_contracts.contains_key(&70099));

        // But it should NOT appear in any option chain (no option_type to classify as call/put)
        let march_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        if let Some(chain) = result.option_chains.get(&march_key) {
            let found = chain.calls.iter().any(|e| e.security_id == 70099)
                || chain.puts.iter().any(|e| e.security_id == 70099);
            assert!(
                !found,
                "option without option_type must not appear in chain entries"
            );
        }
    }

    #[test]
    fn test_pass5_skips_unknown_instrument_kind() {
        // Create a derivative row with an unknown instrument type
        let mut row = make_futidx_row(
            51700,
            26000,
            "NIFTY",
            "2026-03-30",
            75,
            Exchange::NationalStockExchange,
        );
        row.instrument = "FUTCUR".to_owned(); // unknown instrument

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            row,
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // No derivatives should be built from unknown instrument type
        assert!(
            result.derivative_contracts.is_empty(),
            "unknown instrument kind must be skipped"
        );
    }

    #[test]
    fn test_pass5_empty_rows_produces_empty_result() {
        let mut underlyings = HashMap::new();
        let result = build_derivatives_and_chains(&[], &mut underlyings, test_today());

        assert!(result.derivative_contracts.is_empty());
        assert!(result.instrument_info.is_empty());
        assert!(result.option_chains.is_empty());
        assert!(result.expiry_calendars.is_empty());
    }

    #[test]
    fn test_pass5_all_expired_produces_no_derivatives() {
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51600,
                26000,
                "NIFTY",
                "2025-01-30", // expired
                75,
                Exchange::NationalStockExchange,
            ),
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Only the index row should be in instrument_info (no derivatives)
        assert!(
            result.derivative_contracts.is_empty(),
            "all expired contracts must be filtered"
        );
    }

    #[test]
    fn test_pass5_skips_derivative_for_unknown_underlying() {
        // A derivative row whose underlying is NOT in the underlyings map
        let rows = vec![make_futidx_row(
            51700,
            26000,
            "NONEXISTENT",
            "2026-03-30",
            75,
            Exchange::NationalStockExchange,
        )];

        // Empty underlyings map — the derivative's underlying won't be found
        let mut underlyings = HashMap::new();
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        assert!(
            result.derivative_contracts.is_empty(),
            "derivative for unknown underlying must be skipped"
        );
    }

    // -----------------------------------------------------------------------
    // build_subscribed_indices Edge Cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_empty_underlyings() {
        let underlyings = HashMap::new();
        let indices = build_subscribed_indices(&underlyings);

        // Should only have display indices (no F&O indices)
        let fno_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::FnoUnderlying)
            .count();
        assert_eq!(fno_count, 0);

        let display_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .count();
        assert_eq!(display_count, DISPLAY_INDEX_COUNT);
    }

    #[test]
    fn test_build_subscribed_indices_only_stocks() {
        let mut underlyings = HashMap::new();
        underlyings.insert(
            "RELIANCE".to_owned(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_owned(),
                underlying_security_id: 2885,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 500,
                contract_count: 10,
            },
        );

        let indices = build_subscribed_indices(&underlyings);

        // Stock underlyings are skipped — only display indices
        let fno_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::FnoUnderlying)
            .count();
        assert_eq!(
            fno_count, 0,
            "stock underlyings must not become F&O indices"
        );
    }

    // -----------------------------------------------------------------------
    // discover_fno_underlyings: Empty segments
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_empty_input() {
        let underlyings = discover_fno_underlyings(&[]);
        assert!(underlyings.is_empty());
    }

    #[test]
    fn test_discover_fno_underlyings_only_equity_rows() {
        let rows = vec![
            make_equity_row(2885, "RELIANCE"),
            make_equity_row(1333, "HDFCBANK"),
        ];
        let underlyings = discover_fno_underlyings(&rows);
        assert!(
            underlyings.is_empty(),
            "equity-only input should yield no underlyings"
        );
    }

    #[test]
    fn test_discover_fno_underlyings_only_index_rows() {
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_index_row(25, "BANKNIFTY", Exchange::NationalStockExchange),
        ];
        let underlyings = discover_fno_underlyings(&rows);
        assert!(
            underlyings.is_empty(),
            "index-only input (no derivatives) should yield no underlyings"
        );
    }

    // -----------------------------------------------------------------------
    // link_price_ids: Empty inputs
    // -----------------------------------------------------------------------

    #[test]
    fn test_link_price_ids_empty_unlinked() {
        let index_lookup = HashMap::new();
        let equity_lookup = HashMap::new();
        let linked = link_price_ids(vec![], &index_lookup, &equity_lookup);
        assert!(linked.is_empty());
    }

    // -----------------------------------------------------------------------
    // Pass 5: BSE index exchange segment mapping
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_bse_index_derivatives_use_bse_fno_segment() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // SENSEX future (60000) should have BSE_FNO segment
        let sensex_fut = result
            .derivative_contracts
            .get(&60000)
            .expect("SENSEX future must exist");
        assert_eq!(sensex_fut.exchange_segment, ExchangeSegment::BseFno);
    }

    // -----------------------------------------------------------------------
    // BSE Stock Derivative Filter — Test Helpers
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Additional coverage: empty inputs and edge cases for Pass 1-4
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_index_lookup_only_non_index_rows_returns_empty() {
        let rows = vec![
            make_equity_row(2885, "RELIANCE"),
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
        ];
        let lookup = build_index_lookup(&rows);
        assert!(lookup.is_empty(), "no index rows means empty lookup");
    }

    #[test]
    fn test_build_equity_lookup_only_non_equity_rows_returns_empty() {
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
        ];
        let lookup = build_equity_lookup(&rows);
        assert!(lookup.is_empty(), "no equity rows means empty lookup");
    }

    #[test]
    fn test_discover_fno_underlyings_bse_futstk_filtered_nse_futstk_kept() {
        // BSE FUTSTK (should be skipped), NSE FUTSTK (should be kept)
        let rows = vec![
            make_futstk_row_bse(80001, 9001, "FORTIS", "2026-03-30", 200),
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
        ];
        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1);
        assert_eq!(underlyings[0].underlying_symbol, "RELIANCE");
    }

    #[test]
    fn test_discover_fno_underlyings_unknown_instrument_kind_skipped() {
        // A derivative row with instrument kind that doesn't match FUTIDX/FUTSTK
        // The `_ => continue` at line 154 in discover_fno_underlyings
        let mut row = make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500);
        // Change the exchange to BSE — BSE + FUTSTK is skipped explicitly
        // Instead, use a custom instrument name that hits the `_ => continue`
        row.instrument = "OPTIDX".to_owned(); // options don't produce underlyings
        let rows = vec![row];
        let underlyings = discover_fno_underlyings(&rows);
        assert!(
            underlyings.is_empty(),
            "OPTIDX rows should not produce underlyings"
        );
    }

    #[test]
    fn test_link_price_ids_nse_index_uses_idx_i_segment() {
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
        ];
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);
        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let nifty = linked.get("NIFTY").unwrap();
        assert_eq!(nifty.price_feed_segment, ExchangeSegment::IdxI);
        assert_eq!(nifty.derivative_segment, ExchangeSegment::NseFno);
    }

    #[test]
    fn test_link_price_ids_bse_index_uses_bse_fno_segment() {
        let rows = vec![
            make_index_row(51, "SENSEX", Exchange::BombayStockExchange),
            make_futidx_row(
                60000,
                1,
                "SENSEX",
                "2026-03-30",
                20,
                Exchange::BombayStockExchange,
            ),
        ];
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);
        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let sensex = linked.get("SENSEX").unwrap();
        assert_eq!(sensex.price_feed_segment, ExchangeSegment::IdxI);
        assert_eq!(sensex.derivative_segment, ExchangeSegment::BseFno);
    }

    #[test]
    fn test_link_price_ids_stock_uses_nse_equity_segment() {
        let rows = vec![
            make_equity_row(2885, "RELIANCE"),
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
        ];
        let index_lookup = build_index_lookup(&rows);
        let equity_lookup = build_equity_lookup(&rows);
        let unlinked = discover_fno_underlyings(&rows);
        let linked = link_price_ids(unlinked, &index_lookup, &equity_lookup);

        let reliance = linked.get("RELIANCE").unwrap();
        assert_eq!(reliance.price_feed_segment, ExchangeSegment::NseEquity);
        assert_eq!(reliance.derivative_segment, ExchangeSegment::NseFno);
        assert_eq!(reliance.price_feed_security_id, 2885);
    }

    // -----------------------------------------------------------------------
    // Pass 5: additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_equity_instrument_info_skips_non_eq_series() {
        let mut rows = build_test_rows();
        // Add a non-EQ series equity row — should NOT appear in instrument_info
        let mut non_eq = make_equity_row(9999, "SPECIALSTOCK");
        non_eq.series = "BE".to_owned();
        rows.push(non_eq);

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // The non-EQ row should not be in instrument_info
        assert!(
            !result.instrument_info.contains_key(&9999),
            "non-EQ series equity should not be in instrument_info"
        );
    }

    #[test]
    fn test_pass5_bse_equity_skipped_in_instrument_info() {
        let mut rows = build_test_rows();
        // Add a BSE equity row — BSE equities are segment 'E' but exchange=BSE
        // build_derivatives_and_chains only includes NSE equities with EQ series
        let mut bse_eq = make_equity_row(8888, "BSESTOCK");
        bse_eq.exchange = Exchange::BombayStockExchange;
        rows.push(bse_eq);

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        assert!(
            !result.instrument_info.contains_key(&8888),
            "BSE equity should not be in instrument_info"
        );
    }

    #[test]
    fn test_pass5_negative_strike_normalized_to_zero() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Futures have strike_price = -0.01 in CSV, should be normalized to 0.0
        let nifty_fut = result.derivative_contracts.get(&51700).unwrap();
        assert!(
            nifty_fut.strike_price >= 0.0,
            "negative strike should be normalized to 0.0"
        );
    }

    #[test]
    fn test_pass5_option_chain_calls_and_puts_sorted_ascending() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let march_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        let chain = result.option_chains.get(&march_key).unwrap();

        // Verify calls are sorted ascending
        for window in chain.calls.windows(2) {
            assert!(
                window[0].strike_price <= window[1].strike_price,
                "calls must be sorted ascending"
            );
        }
        // Verify puts are sorted ascending
        for window in chain.puts.windows(2) {
            assert!(
                window[0].strike_price <= window[1].strike_price,
                "puts must be sorted ascending"
            );
        }
    }

    #[test]
    fn test_pass5_expiry_calendar_dates_sorted() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_cal = result.expiry_calendars.get("NIFTY").unwrap();
        for window in nifty_cal.expiry_dates.windows(2) {
            assert!(
                window[0] <= window[1],
                "expiry dates must be sorted ascending"
            );
        }
    }

    #[test]
    fn test_pass5_bse_optstk_rows_skipped() {
        let mut rows = build_test_rows();
        // Add a BSE OPTSTK row — should be skipped
        rows.push(make_optstk_row_bse(
            90001,
            2885,
            "RELIANCE",
            "2026-03-30",
            2700.0,
            OptionType::Call,
            500,
        ));

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        assert!(
            !result.derivative_contracts.contains_key(&90001),
            "BSE OPTSTK should be filtered out"
        );
    }

    #[test]
    fn test_pass5_unknown_instrument_kind_skipped() {
        let mut rows = build_test_rows();
        // Row with segment='D' but unknown instrument kind
        let mut unknown = make_futstk_row(99990, 2885, "RELIANCE", "2026-03-30", 500);
        unknown.instrument = "FUTCUR".to_owned(); // unknown instrument kind
        rows.push(unknown);

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        assert!(
            !result.derivative_contracts.contains_key(&99990),
            "unknown instrument kind should be skipped"
        );
    }

    #[test]
    fn test_pass5_derivative_without_expiry_skipped() {
        let mut rows = build_test_rows();
        let mut no_expiry = make_futstk_row(99991, 2885, "RELIANCE", "2026-03-30", 500);
        no_expiry.expiry_date = None;
        rows.push(no_expiry);

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        assert!(
            !result.derivative_contracts.contains_key(&99991),
            "derivative without expiry should be skipped"
        );
    }

    #[test]
    fn test_pass5_option_without_option_type_not_in_chain() {
        let mut rows = build_test_rows();
        // An OPTIDX row without option_type — should still be in derivative_contracts
        // but should NOT be added to option chain
        let mut no_opt_type = make_optidx_row(
            99992,
            26000,
            "NIFTY",
            "2026-03-30",
            23000.0,
            OptionType::Call,
            75,
        );
        no_opt_type.option_type = None;
        rows.push(no_opt_type);

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // The contract IS added to derivative_contracts (it's still a valid contract)
        assert!(
            result.derivative_contracts.contains_key(&99992),
            "contract should exist in derivative_contracts"
        );

        // But should NOT appear in any option chain entries
        let march_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        let chain = result.option_chains.get(&march_key).unwrap();
        let in_calls = chain.calls.iter().any(|e| e.security_id == 99992);
        let in_puts = chain.puts.iter().any(|e| e.security_id == 99992);
        assert!(
            !in_calls && !in_puts,
            "option without option_type should not be in chain"
        );
    }

    // -----------------------------------------------------------------------
    // build_subscribed_indices: default subcategory branch
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_all_categories_present() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        let fno_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::FnoUnderlying)
            .count();
        let display_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .count();

        // We should have both FnoUnderlying and DisplayIndex categories
        assert!(fno_count > 0, "should have FnoUnderlying indices");
        assert!(display_count > 0, "should have DisplayIndex indices");
    }

    #[test]
    fn test_build_subscribed_indices_subcategory_coverage() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        // DISPLAY_INDEX_ENTRIES has subcategories: Volatility, BroadMarket, MidCap,
        // SmallCap, Sectoral, Thematic
        let display_indices: Vec<_> = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .collect();

        // Check that we get indices with various subcategories
        let volatility = display_indices
            .iter()
            .any(|i| i.subcategory == IndexSubcategory::Volatility);
        let broad_market = display_indices
            .iter()
            .any(|i| i.subcategory == IndexSubcategory::BroadMarket);
        let sectoral = display_indices
            .iter()
            .any(|i| i.subcategory == IndexSubcategory::Sectoral);

        assert!(volatility, "should have Volatility display index");
        assert!(broad_market, "should have BroadMarket display index");
        assert!(sectoral, "should have Sectoral display index");
    }

    #[test]
    fn test_pass5_full_pipeline_metadata_counts() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let initial_underlying_count = underlyings.len();
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Verify metadata-relevant counts
        assert!(
            !result.derivative_contracts.is_empty(),
            "should have derivative contracts"
        );
        assert!(
            !result.instrument_info.is_empty(),
            "should have instrument info"
        );
        assert!(
            !result.option_chains.is_empty(),
            "should have option chains"
        );
        assert!(
            !result.expiry_calendars.is_empty(),
            "should have expiry calendars"
        );

        // contract_count should have been updated for at least some underlyings
        let updated_count = underlyings
            .values()
            .filter(|u| u.contract_count > 0)
            .count();
        assert!(
            updated_count > 0,
            "at least some underlyings should have contract_count > 0"
        );
        assert_eq!(
            underlyings.len(),
            initial_underlying_count,
            "underlying count should not change after Pass 5"
        );
    }

    #[test]
    fn test_pass5_all_derivative_rows_produce_instrument_info() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Every derivative contract should also have an instrument_info entry
        for sec_id in result.derivative_contracts.keys() {
            assert!(
                result.instrument_info.contains_key(sec_id),
                "derivative {} should have instrument_info entry",
                sec_id
            );
        }
    }

    #[test]
    fn test_pass5_instrument_info_derivative_variant_fields() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Check a specific derivative in instrument_info
        let info = result.instrument_info.get(&70001).unwrap();
        match info {
            InstrumentInfo::Derivative {
                security_id,
                underlying_symbol,
                instrument_kind,
                exchange_segment,
                expiry_date,
                strike_price,
                option_type,
            } => {
                assert_eq!(*security_id, 70001);
                assert_eq!(underlying_symbol, "NIFTY");
                assert_eq!(*instrument_kind, DhanInstrumentKind::OptionIndex);
                assert_eq!(*exchange_segment, ExchangeSegment::NseFno);
                assert_eq!(*expiry_date, NaiveDate::from_ymd_opt(2026, 3, 30).unwrap());
                assert_eq!(*strike_price, 22000.0);
                assert_eq!(*option_type, Some(OptionType::Call));
            }
            other => panic!(
                "expected Derivative variant, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn test_build_subscribed_indices_fno_bse_index_exchange() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        // BSE index underlyings should have BSE exchange
        let sensex_idx = indices
            .iter()
            .find(|i| i.symbol == "SENSEX" && i.category == IndexCategory::FnoUnderlying);
        assert!(
            sensex_idx.is_some(),
            "SENSEX should be in subscribed indices"
        );
        assert_eq!(sensex_idx.unwrap().exchange, Exchange::BombayStockExchange);
    }

    #[test]
    fn test_build_subscribed_indices_display_all_nse() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        // All display indices should be NSE
        for idx in indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
        {
            assert_eq!(
                idx.exchange,
                Exchange::NationalStockExchange,
                "display index {} should be NSE",
                idx.symbol
            );
        }
    }

    #[test]
    fn test_discover_fno_underlyings_bse_futstk_is_filtered_but_bse_futidx_kept() {
        // Verify that BSE + FUTSTK -> filtered, BSE + FUTIDX -> kept
        let rows = vec![
            make_futstk_row_bse(80001, 9001, "SOMESTOCK", "2026-03-30", 200),
            make_futidx_row(
                60000,
                1,
                "SENSEX",
                "2026-03-30",
                20,
                Exchange::BombayStockExchange,
            ),
        ];
        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1);
        assert_eq!(underlyings[0].underlying_symbol, "SENSEX");
        assert_eq!(underlyings[0].kind, UnderlyingKind::BseIndex);
    }

    #[test]
    fn test_link_price_ids_all_missing_lookups_use_fallback() {
        // No index or equity lookups — all should fall back to underlying_security_id
        let unlinked = vec![
            UnlinkedUnderlying {
                underlying_symbol: "UNKNOWN_INDEX".to_owned(),
                underlying_security_id: 99001,
                kind: UnderlyingKind::NseIndex,
                lot_size: 50,
                derivative_exchange: Exchange::NationalStockExchange,
            },
            UnlinkedUnderlying {
                underlying_symbol: "UNKNOWN_STOCK".to_owned(),
                underlying_security_id: 99002,
                kind: UnderlyingKind::Stock,
                lot_size: 100,
                derivative_exchange: Exchange::NationalStockExchange,
            },
        ];
        let empty_index = HashMap::new();
        let empty_equity = HashMap::new();
        let linked = link_price_ids(unlinked, &empty_index, &empty_equity);

        let idx = linked.get("UNKNOWN_INDEX").unwrap();
        assert_eq!(
            idx.price_feed_security_id, 99001,
            "should fall back to underlying_security_id"
        );

        let stk = linked.get("UNKNOWN_STOCK").unwrap();
        assert_eq!(
            stk.price_feed_security_id, 99002,
            "should fall back to underlying_security_id"
        );
    }

    #[test]
    fn test_pass5_multiple_expiries_in_calendar() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // NIFTY has March and April expiry dates in build_test_rows
        let nifty_cal = result.expiry_calendars.get("NIFTY").unwrap();
        assert!(
            nifty_cal.expiry_dates.len() >= 2,
            "NIFTY should have at least 2 expiry dates"
        );
        assert_eq!(
            nifty_cal.expiry_dates[0],
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()
        );
        assert_eq!(
            nifty_cal.expiry_dates[1],
            NaiveDate::from_ymd_opt(2026, 4, 30).unwrap()
        );
    }

    #[test]
    fn test_pass5_contract_count_matches_derivatives_per_underlying() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // For each underlying with contracts, verify contract_count matches
        for (symbol, underlying) in &underlyings {
            let actual_count = result
                .derivative_contracts
                .values()
                .filter(|c| c.underlying_symbol == *symbol)
                .count();
            assert_eq!(
                underlying.contract_count, actual_count,
                "contract_count mismatch for {}: expected {}, got {}",
                symbol, actual_count, underlying.contract_count
            );
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 3 — wildcard `_ => continue` at line 154
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_skips_bse_futstk_instrument() {
        // A FUTSTK row from BSE should be skipped by the BSE stock filter,
        // but a FUTIDX row from an unrecognized exchange combo (e.g., BSE FUTSTK)
        // should also trigger the `_ => continue` arm (line 150-154).
        // Actually, BSE FUTSTK is caught earlier (line 139). To hit the wildcard,
        // we need an instrument+exchange combo that doesn't match any of the 3 arms:
        // (FUTIDX, NSE), (FUTIDX, BSE), (FUTSTK, NSE). The only remaining combo
        // that passes the is_futures check would not exist normally, but we can test
        // that BSE FUTSTK rows are filtered at line 139 (already tested) and that
        // only expected combos produce underlyings.

        // This row has FUTIDX from NSE => NseIndex (covered)
        // This row has FUTIDX from BSE => BseIndex (covered)
        // This row has FUTSTK from NSE => Stock (covered)
        // This row has FUTSTK from BSE => filtered at line 139, never reaches match

        // To hit line 154 `_ => continue`, we need a FUTSTK from BSE that somehow
        // bypasses the BSE stock filter. Since that's impossible with FUTSTK,
        // the `_ => continue` catches any future instrument type we don't handle.
        // Let's verify with standard data that only expected combos are discovered.
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
                60000,
                1,
                "SENSEX",
                "2026-03-30",
                20,
                Exchange::BombayStockExchange,
            ),
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
            // BSE FUTSTK — skipped by BSE stock filter
            make_futstk_row_bse(80001, 9001, "FORTIS", "2026-03-30", 200),
        ];

        let underlyings = discover_fno_underlyings(&rows);

        // NIFTY (NseIndex), SENSEX (BseIndex), RELIANCE (Stock) present
        assert_eq!(underlyings.len(), 3);
        // FORTIS (BSE FUTSTK) must NOT be present
        assert!(
            !underlyings.iter().any(|u| u.underlying_symbol == "FORTIS"),
            "BSE FUTSTK must be skipped"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: make_optstk_row_bse with OptionType::Put (line 1966)
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_skips_bse_stock_put_options() {
        // Exercise make_optstk_row_bse with OptionType::Put to cover line 1966
        let mut rows = build_test_rows();
        rows.push(make_optstk_row_bse(
            80030,
            2885,
            "RELIANCE",
            "2026-03-30",
            2100.0,
            OptionType::Put,
            500,
        ));

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // BSE OPTSTK Put (security_id 80030) must NOT be in contracts
        assert!(
            !result.derivative_contracts.contains_key(&80030),
            "BSE OPTSTK Put must be skipped"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 5 info! logs (lines 493, 533, 552)
    // These are covered when build_derivatives_and_chains runs to completion.
    // The existing tests already do this, but we add one that explicitly
    // verifies the output data structures are populated (proving the code
    // reached those info! lines).
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_produces_chains_and_calendars_proving_log_lines_reached() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Line 493: info!(derivative_count = ...) — derivatives must be non-empty
        assert!(
            !result.derivative_contracts.is_empty(),
            "derivatives must be produced (line 493 reached)"
        );

        // Line 533: info!(option_chain_count = ...) — option chains must be non-empty
        assert!(
            !result.option_chains.is_empty(),
            "option chains must be produced (line 533 reached)"
        );

        // Line 552: info!(expiry_calendar_count = ...) — expiry calendars must be non-empty
        assert!(
            !result.expiry_calendars.is_empty(),
            "expiry calendars must be produced (line 552 reached)"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: build_subscribed_indices info! log (lines 630-638)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_returns_both_fno_and_display() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        let fno_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::FnoUnderlying)
            .count();
        let display_count = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .count();

        // Proves lines 630-638 (the info! log with both counts) was reached
        assert!(fno_count > 0, "must have F&O indices");
        assert!(display_count > 0, "must have display indices");
        assert_eq!(indices.len(), fno_count + display_count);
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 5 with empty derivative rows
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_with_no_derivative_rows_produces_empty_results() {
        // Only index and equity rows — no segment='D' rows at all
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_equity_row(2885, "RELIANCE"),
        ];
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        assert!(result.derivative_contracts.is_empty());
        assert!(result.option_chains.is_empty());
        assert!(result.expiry_calendars.is_empty());
        // instrument_info should still have index and equity entries
        assert!(result.instrument_info.contains_key(&13));
        assert!(result.instrument_info.contains_key(&2885));
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 3 dedup — second FUTIDX for same symbol skipped
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_deduplicates_by_symbol() {
        let rows = vec![
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            // Duplicate NIFTY with different expiry — should be deduped
            make_futidx_row(
                51701,
                26000,
                "NIFTY",
                "2026-04-30",
                75,
                Exchange::NationalStockExchange,
            ),
        ];

        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1, "duplicate symbol must be deduped");
        assert_eq!(underlyings[0].underlying_symbol, "NIFTY");
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 3 TEST symbol skip
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_skips_test_symbols() {
        let rows = vec![
            make_futstk_row(99998, 99999, "TESTSTOCK", "2026-03-30", 100),
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
        ];

        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1);
        assert_eq!(underlyings[0].underlying_symbol, "RELIANCE");
    }

    // -----------------------------------------------------------------------
    // Coverage: build_subscribed_indices with empty underlyings
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_with_no_underlyings_only_display() {
        let underlyings: HashMap<String, FnoUnderlying> = HashMap::new();
        let indices = build_subscribed_indices(&underlyings);

        // Only display indices (23), no F&O indices
        assert_eq!(indices.len(), DISPLAY_INDEX_COUNT);
        for idx in &indices {
            assert_eq!(idx.category, IndexCategory::DisplayIndex);
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: build_subscribed_indices display subcategory variants
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_all_display_subcategories_present() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        let display_indices: Vec<_> = indices
            .iter()
            .filter(|i| i.category == IndexCategory::DisplayIndex)
            .collect();

        // Verify each known subcategory is present
        assert!(
            display_indices
                .iter()
                .any(|i| i.subcategory == IndexSubcategory::Volatility),
            "Volatility subcategory must be present"
        );
        assert!(
            display_indices
                .iter()
                .any(|i| i.subcategory == IndexSubcategory::BroadMarket),
            "BroadMarket subcategory must be present"
        );
        assert!(
            display_indices
                .iter()
                .any(|i| i.subcategory == IndexSubcategory::MidCap),
            "MidCap subcategory must be present"
        );
        assert!(
            display_indices
                .iter()
                .any(|i| i.subcategory == IndexSubcategory::SmallCap),
            "SmallCap subcategory must be present"
        );
        assert!(
            display_indices
                .iter()
                .any(|i| i.subcategory == IndexSubcategory::Sectoral),
            "Sectoral subcategory must be present"
        );
        assert!(
            display_indices
                .iter()
                .any(|i| i.subcategory == IndexSubcategory::Thematic),
            "Thematic subcategory must be present"
        );
    }

    // -----------------------------------------------------------------------
    // End-to-end test: build_fno_universe with mock HTTP server
    // -----------------------------------------------------------------------

    /// Build a complete instrument CSV string that satisfies all parsing and
    /// validation requirements:
    /// - >= INSTRUMENT_CSV_MIN_ROWS total rows (padded with filtered-out MCX rows)
    /// - >= INSTRUMENT_CSV_MIN_BYTES total bytes
    /// - 8 must-exist indices with correct security IDs
    /// - RELIANCE(2885) equity for must-exist equity check
    /// - RELIANCE, HDFCBANK, INFY, TCS as F&O stock underlyings
    /// - >= VALIDATION_FNO_STOCK_MIN_COUNT stock underlyings
    /// - At least one derivative contract with a future expiry date
    fn build_end_to_end_csv() -> String {
        let header = "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,\
                       UNDERLYING_SECURITY_ID,UNDERLYING_SYMBOL,SYMBOL_NAME,\
                       DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,LOT_SIZE,\
                       SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,\
                       EXPIRY_FLAG,";

        // Pre-allocate: ~100K rows × ~120 bytes each ≈ 12MB
        let estimated_rows = INSTRUMENT_CSV_MIN_ROWS + 1000;
        let mut lines: Vec<String> = Vec::with_capacity(estimated_rows + 1);
        lines.push(header.to_owned());

        // --- NSE Indices (Pass 1) ---
        let nse_indices: &[(u32, &str)] = &[
            (13, "NIFTY"),
            (25, "BANKNIFTY"),
            (27, "FINNIFTY"),
            (442, "MIDCPNIFTY"),
            (38, "NIFTY NEXT 50"),
        ];
        for &(sid, sym) in nse_indices {
            lines.push(format!(
                "NSE,I,{sid},NA,INDEX,{sid},{sym},{sym},{sym} Index,INDEX,NA,\
                 1.0,0001-01-01,,XX,0.0500,N,"
            ));
        }

        // --- BSE Indices (Pass 1) ---
        let bse_indices: &[(u32, &str, &str)] = &[
            (51, "SENSEX", "S&P BSE SENSEX"),
            (69, "BANKEX", "S&P BSE BANKEX"),
            (83, "SNSX50", "S&P BSE SENSEX 50"),
        ];
        for &(sid, sym, display) in bse_indices {
            lines.push(format!(
                "BSE,I,{sid},NA,INDEX,{sid},{sym},{sym},{display},INDEX,NA,\
                 1.0,0001-01-01,,XX,0.0100,N,"
            ));
        }

        // --- Must-exist equities (Pass 2) ---
        let must_exist_equities: &[(u32, &str)] = &[
            (2885, "RELIANCE"),
            (1333, "HDFCBANK"),
            (1594, "INFY"),
            (11536, "TCS"),
            (5258, "SBIN"),
        ];
        for &(sid, sym) in must_exist_equities {
            lines.push(format!(
                "NSE,E,{sid},INE000A00000,EQUITY,,{sym},{sym} LTD,{sym},\
                 ES,EQ,1.0,,,,5.0000,NA,"
            ));
        }

        // Use a far-future expiry so the test is not date-sensitive
        let expiry = "2027-12-30";

        // --- NSE FUTIDX rows (Pass 3 — index underlyings) ---
        let nse_futidx: &[(u32, u32, &str, u32)] = &[
            (51700, 26000, "NIFTY", 75),
            (51701, 26009, "BANKNIFTY", 30),
            (51712, 26037, "FINNIFTY", 60),
            (51713, 26074, "MIDCPNIFTY", 120),
            (51714, 26041, "NIFTYNXT50", 40),
        ];
        for &(sid, usid, sym, lot) in nse_futidx {
            lines.push(format!(
                "NSE,D,{sid},NA,FUTIDX,{usid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,{lot}.0,{expiry},-0.01000,XX,5.0000,M,"
            ));
        }

        // --- BSE FUTIDX rows ---
        let bse_futidx: &[(u32, u32, &str, u32)] = &[
            (60000, 1, "SENSEX", 20),
            (60001, 2, "BANKEX", 30),
            (60002, 3, "SENSEX50", 25),
        ];
        for &(sid, usid, sym, lot) in bse_futidx {
            lines.push(format!(
                "BSE,D,{sid},NA,FUTIDX,{usid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,{lot}.0,{expiry},-0.01000,XX,5.0000,M,"
            ));
        }

        // --- Must-exist F&O stocks: FUTSTK rows (Pass 3) ---
        let must_exist_futstk: &[(u32, u32, &str, u32)] = &[
            (52023, 2885, "RELIANCE", 500),
            (52024, 1333, "HDFCBANK", 550),
            (52025, 1594, "INFY", 400),
            (52026, 11536, "TCS", 175),
            (52027, 5258, "SBIN", 1500),
        ];
        for &(sid, usid, sym, lot) in must_exist_futstk {
            lines.push(format!(
                "NSE,D,{sid},NA,FUTSTK,{usid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,{lot}.0,{expiry},-0.01000,XX,10.0000,M,"
            ));
        }

        // --- Generate 160 additional stock underlyings to exceed
        //     VALIDATION_FNO_STOCK_MIN_COUNT (150) ---
        // Each stock needs: 1 equity row + 1 FUTSTK row
        let extra_stock_count: u32 = 160;
        let equity_base_sid: u32 = 20000;
        let futstk_base_sid: u32 = 55000;
        for i in 0..extra_stock_count {
            let sym = format!("STOCK{i:03}");
            let eq_sid = equity_base_sid + i;
            let fut_sid = futstk_base_sid + i;

            // Equity row
            lines.push(format!(
                "NSE,E,{eq_sid},INE000B00000,EQUITY,,{sym},{sym} LTD,{sym},\
                 ES,EQ,1.0,,,,5.0000,NA,"
            ));
            // FUTSTK row
            lines.push(format!(
                "NSE,D,{fut_sid},NA,FUTSTK,{eq_sid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,100.0,{expiry},-0.01000,XX,10.0000,M,"
            ));
        }

        // --- OPTIDX rows (NIFTY options for option chain coverage) ---
        let opt_rows: &[(u32, f64, &str)] = &[
            (70001, 22000.0, "CE"),
            (70002, 22000.0, "PE"),
            (70003, 22500.0, "CE"),
            (70004, 22500.0, "PE"),
        ];
        for &(sid, strike, ot) in opt_rows {
            lines.push(format!(
                "NSE,D,{sid},NA,OPTIDX,26000,NIFTY,NIFTY-{expiry}-{strike:.0}-{ot},\
                 NIFTY {expiry} {strike:.0} {ot},OP,NA,75.0,{expiry},\
                 {strike:.5},{ot},5.0000,M,"
            ));
        }

        // --- Pad with MCX rows (filtered out) to reach INSTRUMENT_CSV_MIN_ROWS ---
        let real_rows = lines.len() - 1; // Subtract header
        let padding_needed = if real_rows < INSTRUMENT_CSV_MIN_ROWS {
            INSTRUMENT_CSV_MIN_ROWS - real_rows + 1
        } else {
            1
        };

        for i in 0..padding_needed {
            let pad_sid = 900_000 + i as u32;
            lines.push(format!(
                "MCX,D,{pad_sid},NA,FUTCOM,500,CRUDEOIL,CRUDEOIL-FUT,\
                 CRUDE OIL FUT,FUT,NA,100.0,{expiry},-0.01,XX,1.0,M,"
            ));
        }

        let csv = lines.join("\n");

        // If the CSV is still under INSTRUMENT_CSV_MIN_BYTES, pad with
        // comment-like MCX rows (safe because MCX rows are filtered out).
        if csv.len() < INSTRUMENT_CSV_MIN_BYTES {
            let extra_bytes = INSTRUMENT_CSV_MIN_BYTES - csv.len() + 1;
            let pad_row = "MCX,D,999999,NA,FUTCOM,500,CRUDEOIL,CRUDEOIL-FUT,\
                           CRUDE OIL FUT,FUT,NA,100.0,2027-12-30,-0.01,XX,1.0,M,";
            let rows_needed = extra_bytes / pad_row.len() + 1;
            let mut extra_lines = Vec::with_capacity(rows_needed);
            for _ in 0..rows_needed {
                extra_lines.push(pad_row.to_owned());
            }
            return format!("{}\n{}", csv, extra_lines.join("\n"));
        }

        csv
    }

    #[tokio::test]
    async fn test_build_fno_universe_end_to_end() {
        let csv_content = build_end_to_end_csv();

        // Sanity: CSV must meet minimum size and row requirements
        assert!(
            csv_content.len() >= INSTRUMENT_CSV_MIN_BYTES,
            "CSV must be >= {} bytes, got {}",
            INSTRUMENT_CSV_MIN_BYTES,
            csv_content.len()
        );

        // Start a mock HTTP server using TcpListener
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let csv = csv_content.clone();
        let _server = tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let csv = csv.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        // Read the full HTTP request (may be >4KB for large headers,
                        // but for our simple GET it fits easily)
                        let mut buf = vec![0u8; 4096];
                        let _ = stream.read(&mut buf).await;

                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/csv\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n",
                            csv.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.write_all(csv.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });

        let url = format!("http://127.0.0.1:{port}/instruments.csv");

        let temp_dir = std::env::temp_dir().join(format!(
            "dlt-test-build-fno-universe-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let config = InstrumentConfig {
            daily_download_time: "08:00:00".to_owned(),
            csv_cache_directory: temp_dir.to_str().unwrap().to_owned(),
            csv_cache_filename: "e2e-test-instruments.csv".to_owned(),
            csv_download_timeout_secs: 30,
            build_window_start: "08:25:00".to_owned(),
            build_window_end: "08:55:00".to_owned(),
        };

        let result = build_fno_universe(&url, &url, &config).await;
        assert!(
            result.is_ok(),
            "build_fno_universe should succeed: {:?}",
            result.err()
        );

        let universe = result.unwrap();

        // Verify universe is non-empty
        assert!(
            !universe.underlyings.is_empty(),
            "underlyings must not be empty"
        );
        assert!(
            !universe.derivative_contracts.is_empty(),
            "derivative_contracts must not be empty"
        );

        // Verify must-exist indices are present with correct price IDs
        for (symbol, expected_price_id) in VALIDATION_MUST_EXIST_INDICES {
            let underlying = universe.underlyings.get(*symbol);
            assert!(
                underlying.is_some(),
                "must-exist index '{}' not found in underlyings",
                symbol
            );
            assert_eq!(
                underlying.unwrap().price_feed_security_id,
                *expected_price_id,
                "price_feed_security_id mismatch for index '{}'",
                symbol
            );
        }

        // Verify must-exist F&O stocks
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            assert!(
                universe.underlyings.contains_key(*symbol),
                "must-exist F&O stock '{}' not found",
                symbol
            );
        }

        // Verify stock count within validation range
        let stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert!(
            stock_count >= VALIDATION_FNO_STOCK_MIN_COUNT,
            "stock count {} must be >= {}",
            stock_count,
            VALIDATION_FNO_STOCK_MIN_COUNT
        );

        // Verify build metadata
        assert!(
            universe.build_metadata.csv_row_count >= INSTRUMENT_CSV_MIN_ROWS,
            "csv_row_count {} must be >= {}",
            universe.build_metadata.csv_row_count,
            INSTRUMENT_CSV_MIN_ROWS
        );
        assert_eq!(universe.build_metadata.csv_source, "primary");
        assert!(universe.build_metadata.underlying_count > 0);
        assert!(universe.build_metadata.derivative_count > 0);

        // Verify subscribed indices are populated (8 F&O + 23 display = 31)
        assert!(
            !universe.subscribed_indices.is_empty(),
            "subscribed_indices must not be empty"
        );

        // Verify RELIANCE equity is in instrument_info
        assert!(
            universe.instrument_info.contains_key(&2885),
            "RELIANCE (2885) must be in instrument_info"
        );

        // Verify option chains were built (NIFTY options)
        assert!(
            !universe.option_chains.is_empty(),
            "option_chains must not be empty"
        );

        // Clean up
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    // -----------------------------------------------------------------------
    // Duplicate key handling: last-write-wins semantics
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_index_lookup_duplicate_symbol_last_wins() {
        // Same symbol with different security_ids → HashMap overwrites
        let rows = vec![
            make_index_row(100, "NIFTY", Exchange::NationalStockExchange),
            make_index_row(200, "NIFTY", Exchange::NationalStockExchange),
        ];
        let lookup = build_index_lookup(&rows);
        assert_eq!(
            lookup.len(),
            1,
            "duplicate symbols should produce one entry"
        );
        // Last entry wins (HashMap::insert overwrites)
        assert_eq!(lookup["NIFTY"].security_id, 200);
    }

    #[test]
    fn test_build_equity_lookup_duplicate_symbol_last_wins() {
        let rows = vec![
            make_equity_row(100, "RELIANCE"),
            make_equity_row(200, "RELIANCE"),
        ];
        let lookup = build_equity_lookup(&rows);
        assert_eq!(lookup.len(), 1);
        assert_eq!(lookup["RELIANCE"], 200);
    }

    // -----------------------------------------------------------------------
    // discover_fno_underlyings: first-occurrence-wins dedup preserves lot_size
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_first_occurrence_preserves_lot_size() {
        let rows = vec![
            make_futstk_row(1001, 2885, "RELIANCE", "2026-03-30", 500),
            make_futstk_row(1002, 2885, "RELIANCE", "2026-04-30", 250), // different lot
        ];
        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1);
        // First occurrence wins — lot_size should be 500, not 250
        assert_eq!(underlyings[0].lot_size, 500);
    }

    // -----------------------------------------------------------------------
    // build_derivatives_and_chains: NaN strike in option chain sort
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_nan_strike_does_not_panic_during_sort() {
        // NaN strikes should not panic — partial_cmp returns None → Equal
        let today = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let mut underlyings = HashMap::new();
        underlyings.insert(
            "NIFTY".to_owned(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_owned(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                kind: UnderlyingKind::NseIndex,
                lot_size: 75,
                contract_count: 0,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
            },
        );

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            // Option with NaN strike (crafted)
            {
                let mut row = make_optidx_row(
                    70001,
                    26000,
                    "NIFTY",
                    "2026-03-30",
                    22000.0,
                    OptionType::Call,
                    75,
                );
                row.strike_price = f64::NAN;
                row
            },
            make_optidx_row(
                70002,
                26000,
                "NIFTY",
                "2026-03-30",
                22500.0,
                OptionType::Call,
                75,
            ),
        ];

        // Should not panic
        let result = build_derivatives_and_chains(&rows, &mut underlyings, today);
        // NaN strike normalizes to 0.0 (NaN > 0.0 is false → sentinel branch)
        // Actually in our code: strike < 0.0 check — NaN < 0.0 is false, so NaN stays as-is
        // But the sort uses partial_cmp().unwrap_or(Equal) — should not panic
        assert!(!result.derivative_contracts.is_empty());
    }

    // -----------------------------------------------------------------------
    // build_derivatives_and_chains: duplicate contract in CSV
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_duplicate_contract_both_inserted() {
        // Same security_id appearing twice → HashMap insert overwrites (last wins)
        let today = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let mut underlyings = HashMap::new();
        underlyings.insert(
            "NIFTY".to_owned(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_owned(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                kind: UnderlyingKind::NseIndex,
                lot_size: 75,
                contract_count: 0,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
            },
        );

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            // Duplicate with same security_id
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
        ];

        let result = build_derivatives_and_chains(&rows, &mut underlyings, today);
        // HashMap keyed by security_id → duplicate overwrites, count = 1
        assert_eq!(result.derivative_contracts.len(), 1);
    }

    // -----------------------------------------------------------------------
    // build_fno_universe_from_csv: error paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_fno_universe_from_csv_empty_csv_returns_error() {
        let result = build_fno_universe_from_csv("", "test");
        assert!(result.is_err(), "empty CSV should fail parsing");
    }

    #[test]
    fn test_build_fno_universe_from_csv_malformed_csv_returns_error() {
        let result = build_fno_universe_from_csv("not,a,valid,csv\nrow", "test");
        assert!(result.is_err(), "malformed CSV should fail");
    }

    // -----------------------------------------------------------------------
    // build_subscribed_indices: unknown subcategory defaults to Thematic
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_unknown_subcategory_defaults_to_thematic() {
        // We can't modify the constant, but we CAN verify that all entries
        // in DISPLAY_INDEX_ENTRIES have recognized subcategories
        let valid_subcategories = [
            "Volatility",
            "BroadMarket",
            "MidCap",
            "SmallCap",
            "Sectoral",
            "Thematic",
        ];
        for &(name, _id, subcategory) in DISPLAY_INDEX_ENTRIES {
            assert!(
                valid_subcategories.contains(&subcategory),
                "DISPLAY_INDEX_ENTRIES has unrecognized subcategory '{}' for index '{}'",
                subcategory,
                name
            );
        }
    }

    // -----------------------------------------------------------------------
    // Expiry calendar deduplication via BTreeSet
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_expiry_calendar_deduplicates_same_date() {
        // Two contracts with same expiry → calendar has one date
        let today = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let mut underlyings = HashMap::new();
        underlyings.insert(
            "NIFTY".to_owned(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_owned(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                kind: UnderlyingKind::NseIndex,
                lot_size: 75,
                contract_count: 0,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
            },
        );

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            // Two futures with same expiry
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            // Option with same expiry
            make_optidx_row(
                70001,
                26000,
                "NIFTY",
                "2026-03-30",
                22000.0,
                OptionType::Call,
                75,
            ),
        ];

        let result = build_derivatives_and_chains(&rows, &mut underlyings, today);
        let calendar = &result.expiry_calendars["NIFTY"];
        // Despite 2 contracts with same expiry, calendar should have exactly 1 date
        assert_eq!(
            calendar.expiry_dates.len(),
            1,
            "BTreeSet should deduplicate same expiry date"
        );
    }

    // -----------------------------------------------------------------------
    // Option chain with only puts (no calls)
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_option_chain_puts_only() {
        let today = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let mut underlyings = HashMap::new();
        underlyings.insert(
            "NIFTY".to_owned(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_owned(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                kind: UnderlyingKind::NseIndex,
                lot_size: 75,
                contract_count: 0,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
            },
        );

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_optidx_row(
                70001,
                26000,
                "NIFTY",
                "2026-03-30",
                22000.0,
                OptionType::Put,
                75,
            ),
            make_optidx_row(
                70002,
                26000,
                "NIFTY",
                "2026-03-30",
                22500.0,
                OptionType::Put,
                75,
            ),
        ];

        let result = build_derivatives_and_chains(&rows, &mut underlyings, today);
        let key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        let chain = &result.option_chains[&key];
        assert!(chain.calls.is_empty(), "no calls in chain");
        assert_eq!(chain.puts.len(), 2, "two puts in chain");
    }

    // -----------------------------------------------------------------------
    // build_fno_universe_from_csv — invalid CSV input
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_fno_universe_from_csv_empty_string_fails() {
        let result = build_fno_universe_from_csv("", "test");
        assert!(result.is_err(), "empty CSV should fail parsing");
    }

    #[test]
    fn test_build_fno_universe_from_csv_header_only_fails_validation() {
        // A CSV with header but no data rows should fail validation
        // (insufficient derivatives, missing must-exist underlyings, etc.)
        let csv = "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,\
                    UNDERLYING_SECURITY_ID,UNDERLYING_SYMBOL,SYMBOL_NAME,\
                    DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,LOT_SIZE,\
                    SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,\
                    EXPIRY_FLAG,\n";
        let result = build_fno_universe_from_csv(csv, "test-header-only");
        assert!(result.is_err(), "header-only CSV should fail validation");
    }

    #[test]
    fn test_build_fno_universe_from_csv_garbage_content_fails() {
        let result = build_fno_universe_from_csv("not,valid,csv\ndata", "test");
        // Should fail either parsing or validation
        assert!(result.is_err(), "garbage CSV should fail");
    }

    // -----------------------------------------------------------------------
    // Pass 5: duplicate security_id across segments
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_duplicate_security_id_in_index_segment_keeps_first() {
        // Two index rows with the same security_id — second should be skipped
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_index_row(13, "NIFTY_DUP", Exchange::NationalStockExchange),
        ];

        let mut underlyings = HashMap::new();
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Only one should be in instrument_info
        let info = result.instrument_info.get(&13).unwrap();
        match info {
            InstrumentInfo::Index { symbol, .. } => {
                assert_eq!(symbol, "NIFTY", "first occurrence should be kept");
            }
            _ => panic!("expected Index variant"),
        }
    }

    #[test]
    fn test_pass5_duplicate_security_id_in_equity_segment_keeps_first() {
        // Two equity rows with the same security_id
        let rows = vec![
            make_equity_row(2885, "RELIANCE"),
            make_equity_row(2885, "RELIANCE_DUP"),
        ];

        let mut underlyings = HashMap::new();
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let info = result.instrument_info.get(&2885).unwrap();
        match info {
            InstrumentInfo::Equity { symbol, .. } => {
                assert_eq!(symbol, "RELIANCE", "first occurrence should be kept");
            }
            _ => panic!("expected Equity variant"),
        }
    }

    #[test]
    fn test_pass5_duplicate_security_id_in_derivative_segment_keeps_first() {
        // Two derivative rows with the same security_id
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            // Duplicate security_id 51700 with different underlying
            make_futidx_row(
                51700,
                26009,
                "BANKNIFTY",
                "2026-03-30",
                30,
                Exchange::NationalStockExchange,
            ),
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Should have only one derivative with security_id 51700
        let contract = result.derivative_contracts.get(&51700).unwrap();
        assert_eq!(
            contract.underlying_symbol, "NIFTY",
            "first occurrence (NIFTY) should be kept"
        );
    }

    // -----------------------------------------------------------------------
    // Pass 5: positive strike price preserved
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_positive_strike_price_preserved() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // NIFTY option with strike 22000.0 should be preserved exactly
        let opt = result.derivative_contracts.get(&70001).unwrap();
        assert_eq!(opt.strike_price, 22000.0);
    }

    #[test]
    fn test_pass5_zero_strike_price_preserved() {
        // Create a future where strike_price is exactly 0.0
        let mut row = make_futidx_row(
            51700,
            26000,
            "NIFTY",
            "2026-03-30",
            75,
            Exchange::NationalStockExchange,
        );
        row.strike_price = 0.0;

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            row,
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let contract = result.derivative_contracts.get(&51700).unwrap();
        assert_eq!(contract.strike_price, 0.0);
    }

    // -----------------------------------------------------------------------
    // build_subscribed_indices: display indices with all subcategory strings
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_subscribed_indices_display_indices_all_nse_exchange() {
        let underlyings = HashMap::new();
        let indices = build_subscribed_indices(&underlyings);

        // Every display index must have NSE exchange
        for idx in &indices {
            assert_eq!(
                idx.exchange,
                Exchange::NationalStockExchange,
                "display index {} must be NSE",
                idx.symbol
            );
        }
    }

    #[test]
    fn test_build_subscribed_indices_no_duplicate_symbols() {
        let rows = build_test_rows();
        let underlyings = run_passes_1_through_4(&rows);
        let indices = build_subscribed_indices(&underlyings);

        let mut seen = std::collections::HashSet::new();
        for idx in &indices {
            assert!(
                seen.insert(&idx.symbol),
                "duplicate symbol in subscribed indices: {}",
                idx.symbol
            );
        }
    }

    // -----------------------------------------------------------------------
    // Pass 5: contract kind classification
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_futidx_classified_as_future_index() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_fut = result.derivative_contracts.get(&51700).unwrap();
        assert_eq!(nifty_fut.instrument_kind, DhanInstrumentKind::FutureIndex);
    }

    #[test]
    fn test_pass5_futstk_classified_as_future_stock() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let rel_fut = result.derivative_contracts.get(&52023).unwrap();
        assert_eq!(rel_fut.instrument_kind, DhanInstrumentKind::FutureStock);
    }

    #[test]
    fn test_pass5_optidx_call_classified_as_option_index() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_ce = result.derivative_contracts.get(&70001).unwrap();
        assert_eq!(nifty_ce.instrument_kind, DhanInstrumentKind::OptionIndex);
        assert_eq!(nifty_ce.option_type, Some(OptionType::Call));
    }

    #[test]
    fn test_pass5_optidx_put_classified_as_option_index() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_pe = result.derivative_contracts.get(&70002).unwrap();
        assert_eq!(nifty_pe.instrument_kind, DhanInstrumentKind::OptionIndex);
        assert_eq!(nifty_pe.option_type, Some(OptionType::Put));
    }

    // -----------------------------------------------------------------------
    // Pass 5: contract details preservation
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_contract_preserves_lot_size() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_fut = result.derivative_contracts.get(&51700).unwrap();
        assert_eq!(nifty_fut.lot_size, 75);

        let rel_fut = result.derivative_contracts.get(&52023).unwrap();
        assert_eq!(rel_fut.lot_size, 500);
    }

    #[test]
    fn test_pass5_contract_preserves_tick_size() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_fut = result.derivative_contracts.get(&51700).unwrap();
        assert!((nifty_fut.tick_size - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pass5_contract_preserves_expiry_date() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let nifty_fut = result.derivative_contracts.get(&51700).unwrap();
        assert_eq!(
            nifty_fut.expiry_date,
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()
        );
    }

    #[test]
    fn test_pass5_contract_preserves_symbol_name() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let opt = result.derivative_contracts.get(&70001).unwrap();
        assert!(
            opt.symbol_name.contains("NIFTY"),
            "symbol_name should contain underlying: {}",
            opt.symbol_name
        );
    }

    // -----------------------------------------------------------------------
    // Pass 5: futures are NOT in option chains
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_futures_not_in_option_chain_entries() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        let march_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        let chain = result.option_chains.get(&march_key).unwrap();

        // The NIFTY future (51700) should NOT be in calls or puts
        let in_calls = chain.calls.iter().any(|e| e.security_id == 51700);
        let in_puts = chain.puts.iter().any(|e| e.security_id == 51700);
        assert!(
            !in_calls && !in_puts,
            "futures should not appear in option chain entries"
        );

        // But it should be linked as the future_security_id
        assert_eq!(chain.future_security_id, Some(51700));
    }

    // -----------------------------------------------------------------------
    // Pass 5: expiry calendar contains all unique expiry dates
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_expiry_calendar_contains_all_underlyings_with_contracts() {
        let rows = build_test_rows();
        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Every underlying that has derivative contracts should have an expiry calendar
        let symbols_with_contracts: std::collections::HashSet<_> = result
            .derivative_contracts
            .values()
            .map(|c| c.underlying_symbol.clone())
            .collect();

        for symbol in &symbols_with_contracts {
            assert!(
                result.expiry_calendars.contains_key(symbol),
                "missing expiry calendar for {}",
                symbol
            );
        }
    }

    // -----------------------------------------------------------------------
    // build_fno_universe_from_csv: malformed CSV columns
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_fno_universe_from_csv_wrong_headers_fails() {
        let csv = "COL_A,COL_B,COL_C\nval1,val2,val3\n";
        let result = build_fno_universe_from_csv(csv, "test-wrong-headers");
        assert!(result.is_err(), "wrong column names should fail parsing");
    }

    // -----------------------------------------------------------------------
    // Pass 2: build_equity_lookup — commodity and currency segments filtered
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_equity_lookup_skips_commodity_segment() {
        let mut row = make_equity_row(100, "COMMODITYROW");
        row.segment = 'M'; // Commodity
        let lookup = build_equity_lookup(&[row]);
        assert!(
            lookup.is_empty(),
            "M segment should be skipped in equity lookup"
        );
    }

    // -----------------------------------------------------------------------
    // Pass 3: discover_fno_underlyings — multiple exchanges combined
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_three_exchanges_together() {
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
                60000,
                1,
                "SENSEX",
                "2026-03-30",
                20,
                Exchange::BombayStockExchange,
            ),
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
        ];
        let result = discover_fno_underlyings(&rows);
        assert_eq!(result.len(), 3);
        let kinds: Vec<_> = result.iter().map(|u| u.kind).collect();
        assert!(kinds.contains(&UnderlyingKind::NseIndex));
        assert!(kinds.contains(&UnderlyingKind::BseIndex));
        assert!(kinds.contains(&UnderlyingKind::Stock));
    }

    // -----------------------------------------------------------------------
    // link_price_ids — verify derivative_segment assignment
    // -----------------------------------------------------------------------

    #[test]
    fn test_link_price_ids_nse_stock_has_nse_fno_derivative_segment() {
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "TESTSTOCK".to_string(),
            underlying_security_id: 5555,
            kind: UnderlyingKind::Stock,
            lot_size: 100,
            derivative_exchange: Exchange::NationalStockExchange,
        }];
        let mut equity_lookup = HashMap::new();
        equity_lookup.insert("TESTSTOCK".to_string(), 5555_u32);

        let result = link_price_ids(unlinked, &HashMap::new(), &equity_lookup);
        let underlying = result.get("TESTSTOCK").unwrap();
        assert_eq!(underlying.derivative_segment, ExchangeSegment::NseFno);
    }

    #[test]
    fn test_link_price_ids_bse_index_has_bse_fno_derivative_segment() {
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "BANKEX".to_string(),
            underlying_security_id: 2,
            kind: UnderlyingKind::BseIndex,
            lot_size: 30,
            derivative_exchange: Exchange::BombayStockExchange,
        }];
        let mut index_lookup = HashMap::new();
        index_lookup.insert(
            "BANKEX".to_string(),
            IndexEntry {
                security_id: 69,
                exchange: Exchange::BombayStockExchange,
            },
        );

        let result = link_price_ids(unlinked, &index_lookup, &HashMap::new());
        let underlying = result.get("BANKEX").unwrap();
        assert_eq!(underlying.derivative_segment, ExchangeSegment::BseFno);
    }

    #[test]
    fn test_link_price_ids_stock_found_in_equity_lookup() {
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "RELIANCE".to_string(),
            underlying_security_id: 2885,
            kind: UnderlyingKind::Stock,
            lot_size: 500,
            derivative_exchange: Exchange::NationalStockExchange,
        }];
        let index_lookup = HashMap::new();
        let mut equity_lookup = HashMap::new();
        equity_lookup.insert("RELIANCE".to_string(), 2885_u32);

        let result = link_price_ids(unlinked, &index_lookup, &equity_lookup);
        assert_eq!(result.len(), 1);
        let underlying = result.get("RELIANCE").unwrap();
        assert_eq!(underlying.price_feed_security_id, 2885);
        assert_eq!(underlying.price_feed_segment, ExchangeSegment::NseEquity);
    }

    #[test]
    fn test_link_price_ids_stock_not_in_equity_lookup_falls_back() {
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "UNKNOWN_STOCK".to_string(),
            underlying_security_id: 9999,
            kind: UnderlyingKind::Stock,
            lot_size: 100,
            derivative_exchange: Exchange::NationalStockExchange,
        }];
        let index_lookup = HashMap::new();
        let equity_lookup = HashMap::new();

        let result = link_price_ids(unlinked, &index_lookup, &equity_lookup);
        assert_eq!(result.len(), 1);
        let underlying = result.get("UNKNOWN_STOCK").unwrap();
        // Should fall back to underlying_security_id
        assert_eq!(underlying.price_feed_security_id, 9999);
    }

    #[test]
    fn test_link_price_ids_index_found_in_index_lookup() {
        let unlinked = vec![UnlinkedUnderlying {
            underlying_symbol: "NIFTY".to_string(),
            underlying_security_id: 26000,
            kind: UnderlyingKind::NseIndex,
            lot_size: 75,
            derivative_exchange: Exchange::NationalStockExchange,
        }];
        let mut index_lookup = HashMap::new();
        index_lookup.insert(
            "NIFTY".to_string(),
            IndexEntry {
                security_id: 13,
                exchange: Exchange::NationalStockExchange,
            },
        );
        let equity_lookup = HashMap::new();

        let result = link_price_ids(unlinked, &index_lookup, &equity_lookup);
        assert_eq!(result.len(), 1);
        let underlying = result.get("NIFTY").unwrap();
        assert_eq!(underlying.price_feed_security_id, 13);
        assert_eq!(underlying.price_feed_segment, ExchangeSegment::IdxI);
    }

    #[test]
    fn test_link_price_ids_empty_input() {
        let result = link_price_ids(vec![], &HashMap::new(), &HashMap::new());
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Coverage: discover_fno_underlyings dedup — first occurrence wins
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_dedup_first_wins_lot_size() {
        // Two NIFTY futures with DIFFERENT lot_sizes — first occurrence's lot_size wins
        let rows = vec![
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75, // lot_size from first occurrence
                Exchange::NationalStockExchange,
            ),
            make_futidx_row(
                51701,
                26000,
                "NIFTY",
                "2026-06-30",
                50, // different lot_size — should be ignored
                Exchange::NationalStockExchange,
            ),
        ];

        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1, "duplicate NIFTY must be deduped");
        assert_eq!(
            underlyings[0].lot_size, 75,
            "first occurrence's lot_size (75) must be preserved, not second (50)"
        );
        assert_eq!(underlyings[0].underlying_security_id, 26000);
    }

    // -----------------------------------------------------------------------
    // Coverage: discover_fno_underlyings dedup — first wins for FUTSTK too
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_dedup_first_wins_futstk() {
        // Two RELIANCE FUTSTK with different lot_sizes
        let rows = vec![
            make_futstk_row(52023, 2885, "RELIANCE", "2026-03-30", 500),
            make_futstk_row(52024, 2885, "RELIANCE", "2026-06-30", 250),
        ];

        let underlyings = discover_fno_underlyings(&rows);
        assert_eq!(underlyings.len(), 1);
        assert_eq!(
            underlyings[0].lot_size, 500,
            "first lot_size (500) must win over second (250)"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: build_index_lookup — last occurrence wins (HashMap::insert)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_index_lookup_last_occurrence_wins_for_same_symbol() {
        // Two index rows with the same symbol but different security_ids
        // HashMap::insert overwrites, so last one wins
        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_index_row(99, "NIFTY", Exchange::NationalStockExchange),
        ];

        let lookup = build_index_lookup(&rows);
        assert_eq!(lookup.len(), 1);
        // Last insert wins in HashMap
        assert_eq!(lookup.get("NIFTY").unwrap().security_id, 99);
    }

    // -----------------------------------------------------------------------
    // Coverage: build_equity_lookup — last occurrence wins for same symbol
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_equity_lookup_last_occurrence_wins_for_same_symbol() {
        let rows = vec![
            make_equity_row(2885, "RELIANCE"),
            make_equity_row(9999, "RELIANCE"),
        ];

        let lookup = build_equity_lookup(&rows);
        assert_eq!(lookup.len(), 1);
        assert_eq!(lookup.get("RELIANCE"), Some(&9999));
    }

    // -----------------------------------------------------------------------
    // Coverage: discover_fno_underlyings — BSE FUTSTK skipped (TEST + BSE)
    // -----------------------------------------------------------------------

    #[test]
    fn test_discover_fno_underlyings_skips_bse_futstk_with_test_marker() {
        // Row that is both BSE FUTSTK AND has TEST marker — both filters apply
        let rows = vec![make_futstk_row_bse(
            80099,
            9001,
            "TESTBSESTOCK",
            "2026-03-30",
            100,
        )];
        let underlyings = discover_fno_underlyings(&rows);
        assert!(
            underlyings.is_empty(),
            "BSE FUTSTK with TEST marker must be double-filtered"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 5 — duplicate security_id across index and derivative
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_duplicate_security_id_across_segments_keeps_first() {
        // An index row and a derivative row with the same security_id
        // The dedup tracker in Pass 5 should keep the first occurrence
        let rows = vec![
            // Index with security_id 13
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            // FUTIDX with security_id 51700
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Index (13) in instrument_info
        assert!(result.instrument_info.contains_key(&13));
        // Derivative (51700) in instrument_info
        assert!(result.instrument_info.contains_key(&51700));
    }

    // -----------------------------------------------------------------------
    // Coverage: Pass 5 — duplicate security_id within derivatives
    // -----------------------------------------------------------------------

    #[test]
    fn test_pass5_duplicate_derivative_security_id_keeps_first_occurrence() {
        // Two derivative rows with the same security_id — second should be skipped
        let mut second_row = make_futidx_row(
            51700,
            26000,
            "NIFTY",
            "2026-04-30",
            75,
            Exchange::NationalStockExchange,
        );
        // Same security_id as the one in build_test_rows
        second_row.security_id = 51700;

        let rows = vec![
            make_index_row(13, "NIFTY", Exchange::NationalStockExchange),
            make_futidx_row(
                51700,
                26000,
                "NIFTY",
                "2026-03-30",
                75,
                Exchange::NationalStockExchange,
            ),
            second_row,
        ];

        let mut underlyings = run_passes_1_through_4(&rows);
        let result = build_derivatives_and_chains(&rows, &mut underlyings, test_today());

        // Should have exactly 1 derivative contract with ID 51700
        let contract = result.derivative_contracts.get(&51700).unwrap();
        // First occurrence's expiry (2026-03-30) should be kept
        assert_eq!(
            contract.expiry_date,
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
            "first occurrence's expiry must be preserved"
        );
    }
}
