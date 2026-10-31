//! Dedicated read-only helper; it cannot start the trading application.
#![deny(unsafe_code, clippy::unwrap_used, clippy::expect_used)]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![deny(unused_must_use)]

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tickvault_app::operator_movers::run().await
}
