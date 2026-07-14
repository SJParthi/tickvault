//! OMS/Risk → Telegram alert bridge (DHAN order-side alerting, 2026-07-14).
//!
//! Implements BOTH `OmsAlertSink` and `RiskAlertSink` over the shared
//! `NotificationService`, mapping each `OmsAlert` arm 1:1 to its
//! pre-existing `NotificationEvent` variant (no events.rs changes). This
//! is the FIRST production consumer of the engines' `set_alert_sink`
//! setters — wired from `run_trading_pipeline` (the sole OMS/Risk
//! construction site), gated on `TradingPipelineConfig::notifier`.
//!
//! Coded emits (the paging drift guard's real ERROR emit sites):
//! - `RateLimitExhausted` arm → `error!(code = OMS-GAP-04)` — the SEBI
//!   rate limiter denied an order (never auto-retried; regulatory).
//! - `fire_risk_halt` → `error!(code = RISK-GAP-01)` — the risk engine
//!   halted trading.
//! Both fire SYNCHRONOUSLY before the Telegram dispatch, so the
//! CloudWatch log route survives even a dropped Telegram task
//! (dual-route).
//!
//! # Panic guard (release profile `panic = "abort"`)
//! `NotificationService::notify` internally `tokio::spawn`s in Active
//! mode, which ABORTS the process outside a tokio runtime. The bridge is
//! only ever fired from the trading pipeline task (inside the runtime),
//! but `Handle::try_current()` guards the impossible-by-construction
//! no-runtime case anyway: the alert is counted + logged as dropped
//! (tracing + the metrics recorder are both runtime-independent — the
//! drop arm cannot panic).
//!
//! # Ordering caveat (honest envelope)
//! `notify`'s detached tasks can be dropped at runtime teardown — a
//! 15:30-close halt's Telegram may be lost. Same envelope as every
//! existing notify site; the synchronous coded `error!` + CloudWatch
//! alarm is the surviving route. No flush machinery.

use std::sync::Arc;

use tracing::error;

use tickvault_common::error_code::ErrorCode;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_trading::oms::engine::{OmsAlert, OmsAlertSink};
use tickvault_trading::risk::engine::RiskAlertSink;

/// Bridges OMS/Risk alert callbacks to the shared `NotificationService`.
/// Cold path — orders are ~1-100/day; a risk halt fires at most once/day.
pub struct OmsAlertBridge {
    notifier: Arc<NotificationService>,
}

impl OmsAlertBridge {
    /// Creates a bridge over the shared notifier.
    pub fn new(notifier: Arc<NotificationService>) -> Self {
        Self { notifier }
    }

    /// Dispatches an event via the notifier — sync, self-spawning, never
    /// blocks. Outside a tokio runtime the event is DROPPED loudly
    /// (counter + code-free error!) instead of letting notify's internal
    /// `tokio::spawn` abort the process (release `panic = "abort"`).
    fn dispatch(&self, event: NotificationEvent) {
        if tokio::runtime::Handle::try_current().is_ok() {
            self.notifier.notify(event);
        } else {
            metrics::counter!("tv_oms_alert_bridge_dropped_total", "reason" => "no_runtime")
                .increment(1);
            error!("order alert dropped — no async runtime at fire time; alert lost");
        }
    }
}

impl OmsAlertSink for OmsAlertBridge {
    fn fire(&self, alert: OmsAlert) {
        match alert {
            OmsAlert::OrderRejected {
                correlation_id,
                reason,
            } => {
                self.dispatch(NotificationEvent::OrderRejected {
                    correlation_id,
                    reason,
                });
            }
            OmsAlert::CircuitBreakerOpened {
                consecutive_failures,
            } => {
                self.dispatch(NotificationEvent::CircuitBreakerOpened {
                    consecutive_failures,
                });
            }
            OmsAlert::CircuitBreakerClosed => {
                self.dispatch(NotificationEvent::CircuitBreakerClosed);
            }
            OmsAlert::RateLimitExhausted { limit_type } => {
                // OMS-GAP-04: coded emit fires synchronously BEFORE the
                // Telegram dispatch — the CloudWatch log-filter alarm route
                // survives a dropped Telegram task (dual-route).
                error!(
                    code = ErrorCode::OmsGapRateLimit.code_str(),
                    severity = ErrorCode::OmsGapRateLimit.severity().as_str(),
                    limit_type = %limit_type,
                    "OMS-GAP-04: SEBI order rate limit exhausted — order denied, \
                     NEVER auto-retried (regulatory)."
                );
                self.dispatch(NotificationEvent::RateLimitExhausted { limit_type });
            }
        }
    }
}

impl RiskAlertSink for OmsAlertBridge {
    fn fire_risk_halt(&self, reason: &'static str) {
        // RISK-GAP-01: coded emit fires synchronously BEFORE the Telegram
        // dispatch (dual-route — see the module docs).
        error!(
            code = ErrorCode::RiskGapPreTrade.code_str(),
            severity = ErrorCode::RiskGapPreTrade.severity().as_str(),
            reason,
            "RISK-GAP-01: trading HALTED by the risk engine — all orders \
             rejected until operator/daily reset."
        );
        self.dispatch(NotificationEvent::RiskHalt {
            reason: reason.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime-free bridge over the NoOp notifier — NoOp `notify` is
    /// counter-only with no spawn, so every arm is exercisable without
    /// a tokio runtime (the drop arm) or with one (the notify arm).
    fn make_bridge() -> OmsAlertBridge {
        OmsAlertBridge::new(NotificationService::disabled())
    }

    #[tokio::test]
    async fn test_bridge_maps_order_rejected_arm() {
        let bridge = make_bridge();
        bridge.fire(OmsAlert::OrderRejected {
            correlation_id: "corr-1".to_owned(),
            reason: "DH-905 bad input".to_owned(),
        });
    }

    #[tokio::test]
    async fn test_bridge_maps_circuit_breaker_arms() {
        let bridge = make_bridge();
        bridge.fire(OmsAlert::CircuitBreakerOpened {
            consecutive_failures: 5,
        });
        bridge.fire(OmsAlert::CircuitBreakerClosed);
    }

    #[tokio::test]
    async fn test_bridge_maps_rate_limit_arm() {
        // The RateLimitExhausted arm carries the coded OMS-GAP-04 emit —
        // pin the source shape (error! opener + the enum code_str
        // convention the paging drift guard requires) and exercise the arm.
        let src = include_str!("oms_alert_bridge.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            prod.contains("ErrorCode::OmsGapRateLimit.code_str()"),
            "the RateLimitExhausted arm must carry the coded OMS-GAP-04 emit"
        );
        let bridge = make_bridge();
        bridge.fire(OmsAlert::RateLimitExhausted {
            limit_type: "per_second".to_owned(),
        });
    }

    #[tokio::test]
    async fn test_fire_risk_halt_maps_riskhalt_event() {
        // Pin the coded RISK-GAP-01 emit shape, then exercise the arm.
        let src = include_str!("oms_alert_bridge.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            prod.contains("ErrorCode::RiskGapPreTrade.code_str()"),
            "fire_risk_halt must carry the coded RISK-GAP-01 emit"
        );
        let bridge = make_bridge();
        bridge.fire_risk_halt("MaxDailyLossExceeded");
    }

    #[test]
    fn test_bridge_no_runtime_counts_drop() {
        // Plain #[test] — no tokio runtime, so Handle::try_current() errs
        // and every fire takes the counted-drop arm. The assertion is that
        // NOTHING panics (tracing + metrics are runtime-independent and
        // NoOp notify is never reached).
        let bridge = make_bridge();
        bridge.fire(OmsAlert::OrderRejected {
            correlation_id: "corr-2".to_owned(),
            reason: "no runtime".to_owned(),
        });
        bridge.fire(OmsAlert::RateLimitExhausted {
            limit_type: "daily".to_owned(),
        });
        bridge.fire_risk_halt("ManualHalt");
    }
}
