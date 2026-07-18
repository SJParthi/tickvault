//! tickvault-logs MCP server (Rust port of
//! scripts/mcp-servers/tickvault-logs/server.py) — THIN binary; all
//! logic lives in the library (`tickvault_logs_mcp`).

fn main() {
    let ctx = tickvault_logs_mcp::config::Ctx::from_process_env();
    // Python: if "--self-test" in sys.argv (any position).
    if std::env::args().any(|a| a == "--self-test") {
        std::process::exit(tickvault_logs_mcp::selftest::run(&ctx));
    }
    tickvault_logs_mcp::rpc::run_stdio_loop(&ctx);
}
