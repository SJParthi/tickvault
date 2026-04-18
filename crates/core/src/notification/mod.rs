//! Notification module — Telegram + SNS SMS.
//!
//! ONE source (AWS SSM), ONE code path, everywhere.
//! Reads credentials from SSM Parameter Store, sends fire-and-forget
//! alerts via Telegram Bot API (all events) and AWS SNS SMS
//! (Critical/High events only, when `sns_enabled`).
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
pub mod summary_writer;

pub use events::{NotificationEvent, Severity};
pub use service::NotificationService;
