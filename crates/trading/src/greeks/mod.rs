//! Options Greeks engine — Black-Scholes pricing, IV solver, Greeks, PCR, Buildup.
//!
//! All calculations are O(1) per option contract (pure math, zero allocation).
//! Designed for real-time computation on every tick AND historical backtesting.
//!
//! # Modules
//! - `black_scholes` — Core pricing model: BS price, IV solver, all 4 Greeks
//! - `pcr` — Put-Call Ratio from live OI data
//! - `buildup` — Option activity classification (Long/Short Buildup/Unwinding)

pub mod black_scholes;
pub mod buildup;
pub mod pcr;
