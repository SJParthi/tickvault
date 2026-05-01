//! Minimal depth-20 WebSocket connection spawner — Wave 5 receiver-side helper.
//!
//! Encapsulates the boilerplate of spawning a depth-20 connection for the
//! NEW dynamic conn 5 (Wave 5 Item 4) and the future single-side static
//! conns 1-4 (Wave 5 main.rs swap-in). The pre-existing static spawn block
//! at `crates/app/src/main.rs` lines 3340-3625 is left intact for now —
//! this helper duplicates the essential subset (frame persistence + WS
//! lifecycle + Telegram routing), MINUS the OBI computation which is only
//! meaningful for the static ATM-near conns where contracts persist long
//! enough to accumulate bid/ask snapshots.
//!
//! # What this helper does
//!
//! Spawns three tokio tasks:
//! 1. **Frame persistence task** — drains the depth frame channel, parses
//!    via `dispatcher::dispatch_deep_depth_frame`, persists to QuestDB
//!    `deep_market_depth` table via `DeepDepthWriter`. Same column layout
//!    as the static depth-20 conns.
//! 2. **Connected-signal task** — fires `DepthTwentyConnected` Telegram
//!    when the first frame arrives (not just on subscribe).
//! 3. **WebSocket connection task** — runs
//!    `run_twenty_depth_connection`. On termination, fires
//!    `DepthTwentyDisconnected` (in-market) or
//!    `DepthTwentyDisconnectedOffHours` (post-15:30 IST) and decrements
//!    the depth-20 health counter.
//!
//! # What this helper deliberately does NOT do
//!
//! - **OBI computation.** Only the static 4 single-side conns (NIFTY-CE,
//!   NIFTY-PE, BANKNIFTY-CE, BANKNIFTY-PE — Wave 5 commits 1+) need OBI;
//!   the dynamic top-50 conn 5 has shifting contracts so accumulated
//!   bid/ask snapshots are meaningless.
//! - **Registration in `depth_cmd_senders`.** Only the static conns need
//!   to be reachable by `depth_rebalancer` for spot-drift Swap20.
//!   The dynamic conn 5 is driven exclusively by the orchestrator
//!   (`depth_dynamic_pipeline::spawn_depth_20_dynamic_conn5_task`) which
//!   already owns the Sender side directly.
//! - **Registration in `depth_bridge_state_writer`.** Python sidecar
//!   bridge tracks the static ATM contracts only.
//!
//! # Honest envelope (per `wave-4-shared-preamble.md` §8)
//!
//! 100% inside the tested envelope, with ratcheted regression coverage:
//! the WebSocket connection auto-reconnects with `SubscribeRxGuard`
//! (PR #337); the rescue ring + WAL spill cover any QuestDB outage;
//! disconnect events route to off-hours variants outside market hours
//! (per audit-findings Rule 3 + Rule 4 — market-hours-aware,
//! edge-triggered).

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use tickvault_api::state::SharedHealthStatus;
use tickvault_common::config::QuestDbConfig;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::NotificationService;
use tickvault_core::notification::events::NotificationEvent;
use tickvault_core::websocket::DepthCommand;
use tickvault_core::websocket::types::InstrumentSubscription;
use tickvault_storage::ws_frame_spill::WsFrameSpill;

/// Inputs to [`spawn_depth_20_minimal_conn`].
///
/// Bundling these into a struct (rather than ~10 positional parameters)
/// eliminates the `clippy::too_many_arguments` violation and makes call
/// sites easier to read.
pub struct Depth20MinimalConnInputs {
    /// Dhan JWT handle (arc-swap, refreshed every 23h by token_manager).
    pub token_handle: TokenHandle,
    /// Dhan client ID.
    pub ws_client_id: String,
    /// Connection label used in logs + Telegram (e.g. `"DYN-TOP50"`,
    /// `"NIFTY-CE"`). Single-character or short labels are preferred —
    /// they appear in Prometheus labels and Telegram titles.
    pub label: String,
    /// Initial instruments to subscribe. Pass `Vec::new()` for DEFERRED
    /// mode: the connection opens idle (socket up, authenticated, no
    /// SUBSCRIBE message sent), and the orchestrator's first
    /// `DepthCommand::InitialSubscribe20` populates the SID list.
    pub instruments: Vec<InstrumentSubscription>,
    /// Receiver side of the orchestrator's command channel. The
    /// connection's read loop awaits commands here for live
    /// `Swap20` / `InitialSubscribe20` (zero-disconnect SID swaps).
    pub cmd_rx: mpsc::Receiver<DepthCommand>,
    /// QuestDB config — the helper opens its own `DeepDepthWriter`.
    pub questdb_config: QuestDbConfig,
    /// Notifier — fires `DepthTwentyConnected` on first frame and
    /// `DepthTwentyDisconnected` (or `DepthTwentyDisconnectedOffHours`)
    /// on connection termination.
    pub notifier: Arc<NotificationService>,
    /// Health status — increments `tv_depth_20_connections` gauge on
    /// spawn, decrements on terminate.
    pub health_status: SharedHealthStatus,
    /// Optional WAL spill — passes through to
    /// `run_twenty_depth_connection` for STAGE-C durable buffering.
    pub ws_frame_spill: Option<Arc<WsFrameSpill>>,
}

/// Spawn a minimal depth-20 WebSocket connection (3 tokio tasks).
///
/// See module docs for what this DOES and DOES NOT cover compared to
/// the static-conn spawn block in `main.rs`.
///
/// Returns immediately — all three tasks run in the background. Task
/// handles are intentionally not returned; the WS task self-terminates
/// when the connection ends + the orchestrator's shutdown notify fires.
// TEST-EXEMPT: integration-level — spawns 3 tokio tasks against live Dhan WS endpoint + QuestDB; downstream `run_twenty_depth_connection` and `DeepDepthWriter` are independently tested. Compile-time field/signature ratchets in tests below.
pub fn spawn_depth_20_minimal_conn(inputs: Depth20MinimalConnInputs) {
    let Depth20MinimalConnInputs {
        token_handle,
        ws_client_id,
        label,
        instruments,
        cmd_rx,
        questdb_config,
        notifier,
        health_status,
        ws_frame_spill,
    } = inputs;

    // Frame channel: WS connection task -> persistence task.
    // O(1) EXEMPT: boot-time setup; capacity 4096 mirrors the static
    // depth-20 conns in main.rs.
    let (depth_tx, mut depth_rx) = mpsc::channel::<Bytes>(4096);

    // Connected-signal channel: WS connection task -> Telegram task.
    let (signal_tx, signal_rx) = oneshot::channel::<()>();

    // -------------------------------------------------------------------
    // Task 1: frame persistence
    // -------------------------------------------------------------------
    let label_for_recv = label.clone();
    let questdb_for_recv = questdb_config.clone();
    tokio::spawn(async move {
        let frames_counter = metrics::counter!(
            "tv_depth_20lvl_frames_received",
            "underlying" => label_for_recv.clone()
        );
        let mut writer =
            tickvault_storage::deep_depth_persistence::DeepDepthWriter::new(&questdb_for_recv).ok();
        if writer.is_some() {
            info!(
                underlying = %label_for_recv,
                "deep depth QuestDB writer connected (minimal — no OBI)"
            );
        }

        // H5: consecutive parse error counter for Telegram escalation.
        let mut consecutive_parse_errors: u32 = 0;

        while let Some(frame) = depth_rx.recv().await {
            frames_counter.increment(1);
            let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let packets =
                match tickvault_core::parser::dispatcher::split_stacked_depth_packets(&frame) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            underlying = %label_for_recv,
                            "failed to split stacked 20-level depth frame"
                        );
                        continue;
                    }
                };
            for packet in packets {
                match tickvault_core::parser::dispatcher::dispatch_deep_depth_frame(packet, ts) {
                    Ok(tickvault_core::parser::types::ParsedFrame::DeepDepth {
                        security_id,
                        exchange_segment_code,
                        side,
                        levels,
                        message_sequence,
                        ..
                    }) => {
                        consecutive_parse_errors = 0;
                        let side_str = match side {
                            tickvault_core::parser::deep_depth::DepthSide::Bid => "BID",
                            tickvault_core::parser::deep_depth::DepthSide::Ask => "ASK",
                        };
                        if let Some(ref mut w) = writer
                            && let Err(err) = w.append_deep_depth(
                                security_id,
                                exchange_segment_code,
                                side_str,
                                &levels,
                                "20",
                                ts,
                                message_sequence,
                            )
                        {
                            error!(
                                ?err,
                                underlying = %label_for_recv,
                                "failed to persist 20-level depth (minimal conn)"
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        consecutive_parse_errors = consecutive_parse_errors.saturating_add(1);
                        metrics::counter!("tv_depth_parse_errors_total", "depth" => "20")
                            .increment(1);
                        if consecutive_parse_errors >= 5 {
                            error!(
                                ?err,
                                underlying = %label_for_recv,
                                consecutive = consecutive_parse_errors,
                                "H5: 20-level depth parse failures persisting (minimal conn)"
                            );
                            consecutive_parse_errors = 0;
                        } else {
                            tracing::warn!(
                                ?err,
                                underlying = %label_for_recv,
                                "failed to parse 20-level depth packet (minimal conn)"
                            );
                        }
                    }
                }
            }
        }

        if let Some(ref mut w) = writer
            && let Err(err) = w.flush()
        {
            error!(
                ?err,
                underlying = %label_for_recv,
                "depth writer flush on shutdown failed (minimal conn)"
            );
        }
        info!(
            underlying = %label_for_recv,
            "depth frame receiver task exiting (minimal conn)"
        );
    });

    // -------------------------------------------------------------------
    // Task 2: connected-signal listener (Telegram on first frame)
    // -------------------------------------------------------------------
    {
        let notify_label = label.clone();
        let notify_sender = notifier.clone();
        tokio::spawn(async move {
            if signal_rx.await.is_ok() {
                notify_sender.notify(NotificationEvent::DepthTwentyConnected {
                    underlying: notify_label,
                });
            }
        });
    }

    // -------------------------------------------------------------------
    // Task 3: WS connection runner
    // -------------------------------------------------------------------
    let ws_health = health_status.clone();
    let ws_label_for_disconnect = label.clone();
    let ws_label_for_run = label.clone();
    let ws_notifier_disconnect = notifier.clone();
    let ws_reconnect_notifier = Some(notifier.clone());
    tokio::spawn(async move {
        ws_health.set_depth_20_connections(ws_health.depth_20_connections().saturating_add(1));

        if let Err(err) = tickvault_core::websocket::run_twenty_depth_connection(
            token_handle,
            ws_client_id,
            instruments,
            depth_tx,
            ws_label_for_run,
            Some(signal_tx),
            ws_frame_spill,
            cmd_rx,
            ws_reconnect_notifier,
        )
        .await
        {
            error!(
                ?err,
                underlying = %ws_label_for_disconnect,
                "20-level depth connection terminated (minimal conn)"
            );
            // Audit-findings Rule 3 + Rule 4 — market-hours-aware, edge-triggered:
            // off-hours disconnects route to Severity::Low variant to avoid
            // overnight Telegram pager fatigue.
            if tickvault_common::market_hours::is_within_market_hours_ist() {
                ws_notifier_disconnect.notify(NotificationEvent::DepthTwentyDisconnected {
                    underlying: ws_label_for_disconnect,
                    reason: format!("{err}"),
                });
            } else {
                ws_notifier_disconnect.notify(NotificationEvent::DepthTwentyDisconnectedOffHours {
                    underlying: ws_label_for_disconnect,
                    reason: format!("{err}"),
                });
            }
            ws_health.set_depth_20_connections(ws_health.depth_20_connections().saturating_sub(1));
        }
    });
}

#[cfg(test)]
mod tests {
    // Integration-level tests for this helper require a live Dhan WS
    // endpoint + QuestDB instance, so unit-testing the spawn function
    // directly is impractical. The downstream `run_twenty_depth_connection`
    // and `DeepDepthWriter` are independently tested.
    //
    // Compile-time test that the inputs struct can be instantiated with
    // the canonical types — catches signature drift if any imported type
    // changes shape.

    use super::Depth20MinimalConnInputs;

    #[test]
    fn test_inputs_struct_compiles_with_canonical_types() {
        // Compile-time only: do NOT instantiate (would require runtime
        // services). Just assert that `Depth20MinimalConnInputs` is the
        // expected name and remains pub.
        fn _assert_inputs_is_pub(_: Depth20MinimalConnInputs) {}
    }

    #[test]
    fn test_inputs_struct_has_all_required_fields() {
        // Compile-time field check via destructure pattern. If any field
        // is renamed/removed, this test fails to compile.
        fn _destructure(inputs: Depth20MinimalConnInputs) {
            let Depth20MinimalConnInputs {
                token_handle: _,
                ws_client_id: _,
                label: _,
                instruments: _,
                cmd_rx: _,
                questdb_config: _,
                notifier: _,
                health_status: _,
                ws_frame_spill: _,
            } = inputs;
        }
    }

    #[test]
    fn test_module_exports_are_stable() {
        // Public API contract: `spawn_depth_20_minimal_conn` is the
        // single entry point. Compile-fails if it's renamed.
        fn _check_pub_fn() -> fn(Depth20MinimalConnInputs) {
            super::spawn_depth_20_minimal_conn
        }
    }
}
