//! Authentication & token management for Dhan API.
//!
//! # Boot Chain Position
//!
//! `Config → ★ Auth ★ → WebSocket → Parse → Route`
//!
//! # Modules
//!
//! - [`fast_boot_validation`] — AUTH-GAP-06 fast-boot cached-token validation
//! - [`secret_manager`] — Fetches credentials from AWS SSM Parameter Store
//! - [`totp_generator`] — Generates TOTP codes for 2FA
//! - [`token_manager`] — JWT lifecycle with O(1) arc-swap reads and renewal
//! - [`token_health_gauge`] — Live 15s token-health gauge poller (AUTH-GAP-05)
//! - [`types`] — Authentication types (TokenState, DhanCredentials, API structs)
//!
//! # Usage
//!
//! ```ignore
//! // At startup (boot chain):
//! let token_manager = TokenManager::initialize(
//!     &config.dhan, &config.token, &config.network,
//! ).await?;
//!
//! // Hand token handle to downstream consumers:
//! let token_handle = token_manager.token_handle();
//!
//! // Spawn background renewal task:
//! let _renewal_task = token_manager.spawn_renewal_task();
//!
//! // In WebSocket client or REST caller (O(1) read):
//! let guard = token_handle.load();
//! let token = guard.as_ref().as_ref().expect("token must exist"); // APPROVED: doc example
//! let auth_header = token.access_token().expose_secret();
//! ```

pub mod fast_boot_validation;
pub mod mid_session_watchdog;
pub mod secret_manager;
pub mod token_cache;
pub mod token_health_gauge;
pub mod token_manager;
pub mod totp_generator;
pub mod types;

pub use token_cache::FastCacheResult;
pub use token_manager::{TokenHandle, TokenManager};
pub use types::{DhanCredentials, QuestDbCredentials, TelegramCredentials, TokenState};
