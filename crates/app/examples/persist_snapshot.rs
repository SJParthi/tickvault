//! Integration example: downloads instrument CSV, builds FnoUniverse,
//! and persists a daily snapshot to QuestDB.
//!
//! Run from the project root:
//! ```
//! cargo run --example persist_snapshot -p dhan-live-trader-app
//! ```
//!
//! Prerequisites: QuestDB running on localhost:9009 (dlt-questdb container).

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use dhan_live_trader_common::config::{InstrumentConfig, QuestDbConfig};
use dhan_live_trader_core::instrument::build_fno_universe;
use dhan_live_trader_storage::instrument_persistence::persist_instrument_snapshot;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info"))
        .init();

    tracing::info!("=== Instrument Snapshot Persistence Example ===");

    // Config: use localhost since we're running from the host, not inside Docker.
    let questdb_config = QuestDbConfig {
        host: "localhost".to_string(),
        http_port: 9000,
        pg_port: 8812,
        ilp_port: 9009,
    };

    let instrument_config = InstrumentConfig {
        daily_download_time: "08:45:00".to_string(),
        csv_cache_directory: "/tmp/dlt-instrument-cache".to_string(),
        csv_cache_filename: "api-scrip-master-detailed.csv".to_string(),
        csv_download_timeout_secs: 120,
    };

    // Step 1: Download CSV and build universe.
    tracing::info!("Step 1: Building F&O universe from Dhan CSV...");
    let universe = build_fno_universe(
        "https://images.dhan.co/api-data/api-scrip-master-detailed.csv",
        "https://images.dhan.co/api-data/api-scrip-master.csv",
        &instrument_config,
    )
    .await?;

    tracing::info!(
        underlyings = universe.underlyings.len(),
        derivatives = universe.derivative_contracts.len(),
        option_chains = universe.option_chains.len(),
        "Universe built successfully"
    );

    // Step 2: Persist to QuestDB.
    tracing::info!("Step 2: Persisting instrument snapshot to QuestDB...");
    persist_instrument_snapshot(&universe, &questdb_config)?;

    tracing::info!("=== Done! Check QuestDB console at http://localhost:9000 ===");
    tracing::info!("Try these queries:");
    tracing::info!("  SELECT * FROM instrument_build_metadata;");
    tracing::info!("  SELECT * FROM fno_underlyings LIMIT 20;");
    tracing::info!("  SELECT count() FROM derivative_contracts;");

    Ok(())
}
