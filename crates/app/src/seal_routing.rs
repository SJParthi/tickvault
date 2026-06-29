//! Shared per-seal routing for BOTH market-data feeds (Dhan + Groww).
//!
//! Before C2 the per-seal `on_seal` closure body was duplicated: one copy inline
//! in `crates/app/src/main.rs::spawn_engine_b_aggregator` (Dhan) and one inline in
//! `crates/app/src/groww_bridge.rs` (Groww). The two copies did the SAME core
//! work — build a [`BufferedSeal`] and route it through the shared seal-writer
//! channel ([`global_seal_sender`]) — and differed only in a small set of
//! per-feed policies (D1-drop, prev-day pct-stamp, and the observability sinks).
//!
//! [`route_seal`] is the UNION of those two bodies, with the per-feed differences
//! hoisted into the explicit, borrowed, `Copy`, zero-alloc [`SealRouteParams`].
//! It is **strictly behavior-preserving**: every counter, drop label,
//! `BufferedSeal` field, the D1-drop early-return, and the prev-day pct-stamp is
//! byte-identical to the pre-C2 inline closures for each feed.
//!
//! [`global_seal_sender`]: tickvault_storage::seal_writer_runner::global_seal_sender

use tokio::sync::mpsc;

use tickvault_common::feed::Feed;
use tickvault_common::feed_health::FeedHealthRegistry;
use tickvault_trading::candles::{
    AggregatorHeartbeatCounters, BufferedSeal, LiveCandleState, TfIndex, stamp_seal_pct_fields,
};
use tickvault_trading::in_mem::PrevDayCache;

/// Per-feed routing policy for [`route_seal`].
///
/// Borrowed + `Copy` + zero-alloc — holds only references and the [`Feed`] tag.
/// The fields express EXACTLY the differences between the pre-C2 Dhan and Groww
/// `on_seal` closures, so passing the right values reproduces each feed's
/// behaviour bit-for-bit:
///
/// | Field | Dhan | Groww | Controls |
/// |---|---|---|---|
/// | `feed` | [`Feed::Dhan`] | [`Feed::Groww`] | the [`BufferedSeal::feed`] stamp AND the drop-counter label (Dhan = no label, Groww = `feed=groww`) |
/// | `drop_d1` | `true` | `false` | the `if tf == D1 { return }` early-return |
/// | `prev_day_cache` | `Some(&cache)` | `None` | the prev-day lookup + `prev_day_close` override + [`stamp_seal_pct_fields`] |
/// | `heartbeat` | `Some(&hb)` | `None` | the Dhan `tv_aggregator_*` counters + heartbeat emit/drop/close-pct |
/// | `feed_health_on_m1` | `None` | `Some(&fh)` | the Groww `record_candle` on the M1 seal |
#[derive(Clone, Copy)]
pub struct SealRouteParams<'a> {
    /// Source feed — stamped into [`BufferedSeal::feed`] and selects the
    /// drop-counter label.
    pub feed: Feed,
    /// When `true`, a [`TfIndex::D1`] seal is dropped at the write boundary
    /// (Dhan: 1d is historical-only per `live-feed-purity.md` rule 10). When
    /// `false`, every TF is routed (Groww never dropped D1).
    pub drop_d1: bool,
    /// When `Some`, the seal-time prev-day pct fields are stamped from this
    /// cache (Dhan). When `None`, the seal is sent as-is (Groww).
    pub prev_day_cache: Option<&'a PrevDayCache>,
    /// When `Some`, the Dhan aggregator heartbeat counters + the
    /// `tv_aggregator_seals_emitted_total` / `tv_aggregator_close_pct_nonzero_total`
    /// metrics fire (Dhan). When `None`, they do not (Groww).
    pub heartbeat: Option<&'a AggregatorHeartbeatCounters>,
    /// When `Some`, `record_candle(feed)` fires on the M1 seal (Groww live-feed
    /// health). When `None`, it does not (Dhan).
    pub feed_health_on_m1: Option<&'a FeedHealthRegistry>,
}

/// Outcome of a [`route_seal`] call — whether the seal was dropped at the D1
/// write boundary, dropped because the mpsc was full, or sent to the channel.
///
/// The IST-midnight force-seal caller uses this to keep its local `sealed` /
/// `dropped` running counts (the pre-C2 closure mutated them inline). The
/// per-tick callers ignore the return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealOutcome {
    /// `drop_d1=true` and `tf == D1` — the seal was suppressed at the write
    /// boundary (never built, never sent).
    DroppedD1,
    /// The seal-writer mpsc was full; the seal was dropped (counter only — the
    /// downstream ring→spill→DLQ chain is the durable absorber).
    DroppedFull,
    /// The seal reached the channel.
    Sent,
}

/// Routes one sealed bar to the shared seal-writer channel, applying the
/// per-feed policy in `params`. Returns the [`SealOutcome`] so callers that keep
/// local running counts (the IST-midnight force-seal) can.
///
/// This is the single shared body for BOTH feeds' seal-routing callbacks — the
/// per-tick Dhan + Groww `on_seal` closures AND the Dhan IST-midnight
/// `force_seal_all` closure. The caller resolves `global_seal_sender()` ONCE (the
/// `&'static` borrow stays at the call site so the lifetime is unchanged) and
/// passes it in. `state` is taken by value (`mut`) — the same move semantics as
/// the `FnMut(TfIndex, LiveCandleState)` callback — and [`BufferedSeal::new`]
/// consumes it.
///
/// Behaviour is byte-identical to the pre-C2 inline closures:
/// - Dhan per-tick: `drop_d1=true`, `prev_day_cache=Some`, `heartbeat=Some`,
///   `feed_health_on_m1=None` (ignores the return).
/// - Dhan IST-midnight: `drop_d1=true`, `prev_day_cache=Some`, `heartbeat=None`,
///   `feed_health_on_m1=None` (uses the return for `sealed`/`dropped`).
/// - Groww: `drop_d1=false`, `prev_day_cache=None`, `heartbeat=None`,
///   `feed_health_on_m1=Some` (ignores the return).
///
/// The `tv_seal_mpsc_dropped_total` / `tv_aggregator_seals_emitted_total` /
/// `tv_aggregator_close_pct_nonzero_total` metrics are a Dhan-feed concern (both
/// Dhan paths fired them; Groww fired none of them), so they are gated on
/// `feed == Feed::Dhan` — reproducing the pre-C2 series EXACTLY. The heartbeat
/// `record_*` methods are independent and gated on `heartbeat.is_some()` (only
/// the per-tick Dhan path carries one).
///
/// Zero-alloc: references + `&'static str` labels only; no `format!` / `String`
/// / `Vec` / `clone` on this hot-adjacent (per-seal) path.
#[inline]
pub fn route_seal(
    params: SealRouteParams<'_>,
    security_id: u64,
    exchange_segment_code: u8,
    tf: TfIndex,
    mut state: LiveCandleState,
    sender: &mpsc::Sender<BufferedSeal>,
) -> SealOutcome {
    // 1d historical-only (operator directive 2026-06-02, `live-feed-purity.md`
    // rule 10): the 1d timeframe is NEVER tick-calculated for the Dhan feed.
    // Drop any D1 seal the aggregator emits so it never reaches `candles_1d`.
    // The aggregator still seals all 21 TFs internally; only this write boundary
    // skips D1. Groww (drop_d1=false) routes D1.
    if params.drop_d1 && tf == TfIndex::D1 {
        return SealOutcome::DroppedD1;
    }

    // Dhan: stamp the seal-time prev-day pct fields. Operator decision
    // 2026-05-28: take the prev-day close straight from the live ticks `close`
    // column (captured into the candle state), not the QuestDB PrevDayCache
    // (which is empty in the Engine-B runtime). Drives close_pct_from_prev_day.
    let mut close_pct_nonzero = false;
    if let Some(cache) = params.prev_day_cache {
        let mut refs = cache
            .lookup(security_id, exchange_segment_code)
            .unwrap_or_default();
        refs.prev_day_close = state.prev_day_close;
        stamp_seal_pct_fields(&mut state, refs);
        // G3 real-time proof: capture the % BEFORE `state` is moved into the
        // seal. A non-zero value proves the percentage-change column is
        // populating live.
        close_pct_nonzero = state.close_pct_from_prev_day != 0.0;
    }

    let seal = BufferedSeal::new(security_id, exchange_segment_code, tf, state, params.feed);

    if sender.try_send(seal).is_err() {
        // The seal mpsc is full (writer behind). Counter only — the
        // ring→spill→DLQ chain downstream is the durable absorber; never panic.
        // The drop-counter label is per-feed, reproducing the pre-C2 series
        // EXACTLY: Dhan = NO label, Groww = `feed=groww`.
        match params.feed {
            Feed::Dhan => {
                metrics::counter!("tv_seal_mpsc_dropped_total").increment(1);
            }
            Feed::Groww => {
                metrics::counter!("tv_seal_mpsc_dropped_total", "feed" => "groww").increment(1);
            }
        }
        if let Some(hb) = params.heartbeat {
            hb.record_drop();
        }
        SealOutcome::DroppedFull
    } else {
        // Dhan aggregator counters (BOTH Dhan paths fire these — per-tick AND
        // IST-midnight; Groww fires none of them).
        if params.feed == Feed::Dhan {
            metrics::counter!("tv_aggregator_seals_emitted_total").increment(1);
            if close_pct_nonzero {
                metrics::counter!("tv_aggregator_close_pct_nonzero_total").increment(1);
            }
        }
        // Per-tick Dhan heartbeat (the IST-midnight path has no heartbeat).
        if let Some(hb) = params.heartbeat {
            hb.record_emit();
            if close_pct_nonzero {
                hb.record_close_pct_nonzero();
            }
        }
        // Groww live-feed health: count one candle per sealed 1-minute bar
        // (the cross-verified slice).
        if let Some(fh) = params.feed_health_on_m1
            && tf == TfIndex::M1
        {
            fh.record_candle(params.feed);
        }
        SealOutcome::Sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dhan policy: a non-D1 TF is routed to the channel with `feed=Dhan`; a D1
    /// seal is dropped at the write boundary (`drop_d1=true`).
    #[tokio::test]
    async fn test_route_seal_dhan_routes_non_d1_and_drops_d1() {
        let (tx, mut rx) = mpsc::channel::<BufferedSeal>(8);
        let params = SealRouteParams {
            feed: Feed::Dhan,
            drop_d1: true,
            prev_day_cache: None, // exercise the None-cache (no pct-stamp) arm
            heartbeat: None,      // counters are global; we assert routing only
            feed_health_on_m1: None,
        };

        // A non-D1 seal is routed.
        let out = route_seal(params, 13, 0, TfIndex::M1, LiveCandleState::empty(), &tx);
        assert_eq!(out, SealOutcome::Sent);
        let got = rx.try_recv().expect("non-D1 seal must be routed");
        assert_eq!(got.security_id, 13);
        assert_eq!(got.exchange_segment_code, 0);
        assert_eq!(got.tf, TfIndex::M1);
        assert_eq!(got.feed, Feed::Dhan);

        // A D1 seal is dropped (drop_d1=true) — nothing reaches the channel.
        let out = route_seal(params, 13, 0, TfIndex::D1, LiveCandleState::empty(), &tx);
        assert_eq!(out, SealOutcome::DroppedD1);
        assert!(
            rx.try_recv().is_err(),
            "D1 seal must be dropped when drop_d1=true"
        );
    }

    /// Groww policy: EVERY TF — including D1 — is routed with `feed=Groww`
    /// (`drop_d1=false`), with no pct-stamp.
    #[tokio::test]
    async fn test_route_seal_groww_routes_all_tfs() {
        let (tx, mut rx) = mpsc::channel::<BufferedSeal>(8);
        let params = SealRouteParams {
            feed: Feed::Groww,
            drop_d1: false,
            prev_day_cache: None,
            heartbeat: None,
            feed_health_on_m1: None,
        };

        let out = route_seal(params, 1333, 1, TfIndex::M1, LiveCandleState::empty(), &tx);
        assert_eq!(out, SealOutcome::Sent);
        let m1 = rx.try_recv().expect("Groww M1 seal must be routed");
        assert_eq!(m1.feed, Feed::Groww);
        assert_eq!(m1.security_id, 1333);
        assert_eq!(m1.tf, TfIndex::M1);

        // D1 is NOT dropped for Groww.
        let out = route_seal(params, 1333, 1, TfIndex::D1, LiveCandleState::empty(), &tx);
        assert_eq!(out, SealOutcome::Sent);
        let d1 = rx
            .try_recv()
            .expect("Groww D1 seal must be routed (no drop)");
        assert_eq!(d1.tf, TfIndex::D1);
        assert_eq!(d1.feed, Feed::Groww);
    }

    /// Send failure (full channel) does not panic — the row is dropped (counter
    /// only) per the ring→spill→DLQ durable-absorber design.
    #[tokio::test]
    async fn test_route_seal_full_channel_does_not_panic() {
        let (tx, _rx) = mpsc::channel::<BufferedSeal>(1);
        // Fill the single slot.
        tx.try_send(BufferedSeal::new(
            1,
            0,
            TfIndex::M1,
            LiveCandleState::empty(),
            Feed::Dhan,
        ))
        .expect("first send fills the channel");
        let params = SealRouteParams {
            feed: Feed::Dhan,
            drop_d1: true,
            prev_day_cache: None,
            heartbeat: None,
            feed_health_on_m1: None,
        };
        // Second route hits the full channel — must not panic, returns DroppedFull.
        let out = route_seal(params, 2, 0, TfIndex::M1, LiveCandleState::empty(), &tx);
        assert_eq!(out, SealOutcome::DroppedFull);
    }
}
