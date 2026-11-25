//! `tv-live-fill-replay`: replays one recorded IST day through the live
//! paper fill chain, offline (plan item LF-8). Usage, outputs, exit codes and
//! honest limits: `tickvault_app::live_fill::replay`.
//!
//! PAPER ONLY: it reads recorded frames (a local folder, or the cold
//! bucket's `raw-frames/` copy with List and Get only) and writes files to a
//! new local folder. It places no order, calls no broker, writes no database
//! row and writes nothing to the bucket. No tracing subscriber is installed:
//! the tool talks on stdout and stderr only.
#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![deny(unused_must_use)]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(tickvault_app::live_fill::replay::run_cli(&args));
}
