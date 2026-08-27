//! tickvault-logs MCP server (Rust port of
//! the retired reference implementation, formerly under scripts/mcp-servers/tickvault-logs/) — THIN binary; all
//! logic lives in the library (`tickvault_logs_mcp`).
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

fn main() {
    let ctx = tickvault_logs_mcp::config::Ctx::from_process_env();
    // legacy: if "--self-test" in sys.argv (any position).
    if std::env::args().any(|a| a == "--self-test") {
        std::process::exit(tickvault_logs_mcp::selftest::run(&ctx));
    }
    tickvault_logs_mcp::rpc::run_stdio_loop(&ctx);
}
