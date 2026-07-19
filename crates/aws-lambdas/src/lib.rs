//! tickvault-aws-lambdas — the operational AWS Lambda fleet in Rust.
//!
//! Rust-only purge phase 2b-1 (2026-07-18): ports of
//! - `deploy/aws/lambda/budget-killswitch/handler.py` (the pilot rewrite)
//! - the `budget-guards.tf` daily-budget-digest inline Python heredoc
//! - the `boot-heartbeat-alarm.tf` boot-window gate inline Python heredoc
//! - the `market-hours-liveness-alarm.tf` market-hours gate inline heredoc
//!
//! Rust-only purge phase 2b-2 wave 2 (2026-07-18): ports of
//! - `deploy/aws/lambda/start-watchdog/handler.py` (EC2 lifecycle watchdog)
//! - `deploy/aws/lambda/hard-stop-guard/handler.py` (hourly budget/schedule guard)
//!
//! Behavior parity is the contract: same env vars, same SNS message shapes,
//! same alarm-name/window logic, same IST time math as the Python sources.
//! Every deviation is deliberate and documented at the deviating line.
//!
//! Cold path only — these run 1-4 times per day on EventBridge crons / SNS
//! pushes; none of the hot-path (zero-alloc / O(1)) constraints apply, but
//! the charter lints below do.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]

pub mod alarm_gate;
pub mod budget_digest;
pub mod budget_killswitch;
pub mod clients;
pub mod deploy_watchdog;
pub mod events;
pub mod hard_stop_guard;
pub mod logging;
pub mod market_hours_gate;
pub mod market_open_readiness;
pub mod operator_control;
pub mod operator_control_action_commands;
pub mod operator_control_commands;
pub mod start_watchdog;
pub mod telegram_webhook;
pub mod time;
