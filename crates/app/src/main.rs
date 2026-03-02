//! Binary entry point for the dhan-live-trader application.
//!
//! Thin wrapper — all boot logic lives in `lib.rs` for testability.

use anyhow::{Context, Result};
use tracing::info;

use dhan_live_trader_core::notification::NotificationEvent;

use dhan_live_trader_app::{
    CONFIG_BASE_PATH, boot, init_tracing, install_crypto_provider, load_config,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Step 0: TLS bootstrap (must happen before ANY TLS usage)
    install_crypto_provider();

    // Step 1: Load and validate configuration
    let config = load_config()?;

    // Step 2: Initialize structured logging
    init_tracing(&config.logging.level, &config.logging.format);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config_file = CONFIG_BASE_PATH,
        "dhan-live-trader starting"
    );

    // Steps 3–10: Boot sequence
    let result = boot(config).await?;

    // Ready
    info!(
        mode = result.mode,
        "system ready — press Ctrl+C to stop\n\
         \n\
           Health:  http://{bind_addr}/health\n\
           Stats:   http://{bind_addr}/api/stats\n\
           Portal:  http://{bind_addr}/portal\n",
        bind_addr = result.bind_addr
    );

    result
        .notifier
        .notify(NotificationEvent::StartupComplete { mode: result.mode });

    // Step 11: Await shutdown signal
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    info!("shutdown signal received — stopping gracefully");
    result.notifier.notify(NotificationEvent::ShutdownInitiated);

    result.handles.abort_all();

    info!("dhan-live-trader stopped");
    Ok(())
}
