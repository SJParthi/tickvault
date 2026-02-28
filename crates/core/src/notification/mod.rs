//! Telegram notification module.
//!
//! ONE source (AWS SSM), ONE code path, everywhere.
//! Reads bot-token and chat-id from SSM Parameter Store, sends
//! fire-and-forget alerts via the Telegram Bot API (reqwest POST).
//!
//! # Boot Sequence Position
//! Config → Logging → **Notification** → Auth → QuestDB → ...
//!
//! # Usage
//! ```ignore
//! let notifier = NotificationService::initialize(&config.notification).await;
//! notifier.notify(NotificationEvent::StartupComplete { mode: "LIVE" });
//! ```

pub mod events;
pub mod service;

pub use events::NotificationEvent;
pub use service::NotificationService;
