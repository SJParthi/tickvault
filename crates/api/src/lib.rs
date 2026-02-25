//! HTTP API server — axum endpoints for monitoring, control, and data access.
//!
//! # Key Modules (to be built)
//! - `routes` — API endpoint definitions
//! - `handlers` — Request handlers
//! - `middleware` — tower middleware (auth, logging, compression)
//!
//! # Boot Sequence Position
//! Cache -> **HTTP API** -> Metrics -> Dashboards
