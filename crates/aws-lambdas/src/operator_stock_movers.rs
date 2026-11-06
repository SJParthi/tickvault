//! Authenticated filtered reader. At most one bounded cached filter result and
//! one command in flight per Lambda container. Never filters a truncated page.
use crate::operator_control::OpsShell;
use crate::operator_movers_universe::{
    MAX_UNIVERSE_BYTES, MoversUniverse, UNIVERSE_MAX_FUTURE_SKEW_SECS,
    VIEWER_HEARTBEAT_INTERVAL_SECS, ViewerHeartbeat, select,
};
use crate::operator_movers_wire::{BridgeError, MAX_WIRE_BYTES};
use crate::operator_stock_movers_wire::{Filter, Reply};
use std::sync::atomic::{AtomicI64, Ordering};
const CACHE_SECONDS: u64 = 20;
const BRIDGE_TIMEOUT_SECS: u64 = 22;
#[derive(Default)]
pub struct MoversCache {
    value: tokio::sync::Mutex<Option<(tokio::time::Instant, Filter, Reply)>>,
    /// Plan PR MV review: UTC second this container last wrote the viewer
    /// heartbeat (0 = never).
    heartbeat_at: AtomicI64,
}
fn decode(output: &str, now: i64, filter: &Filter) -> Reply {
    if output.is_empty() {
        return Reply::error(BridgeError::BridgeUnavailable, now);
    }
    if output.len() > MAX_WIRE_BYTES {
        return Reply::error(BridgeError::ResponseTooLarge, now);
    }
    match serde_json::from_str::<Reply>(output) {
        Ok(reply) if reply.valid_at(now, filter) => reply,
        _ => Reply::error(BridgeError::InvalidResponse, now),
    }
}
/// A complete stdout is accepted only after SSM reports Success. Used by both
/// read-only movers modes; mutating legacy operator actions remain unchanged.
pub(crate) async fn strict_read_payload<S: OpsShell + ?Sized>(
    shell: &S,
    command: String,
) -> Result<String, BridgeError> {
    const COMMAND_TIMEOUT_SECS: u64 = 18;
    const POLL_MILLIS: u64 = 400;
    let read = async {
        let cid = shell
            .ssm_shell(&[command])
            .await
            .map_err(|_| BridgeError::BridgeUnavailable)?;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(POLL_MILLIS)).await;
            let Ok((status, stdout, stderr)) = shell.command_invocation(&cid).await else {
                continue;
            };
            match status.as_str() {
                "Success" if stderr.is_empty() => return Ok(stdout),
                "Success" | "Failed" | "Cancelled" => return Err(BridgeError::BridgeUnavailable),
                "TimedOut" | "DeliveryTimedOut" => return Err(BridgeError::Timeout),
                "Pending" | "InProgress" | "Delayed" | "Cancelling" => (),
                _ => return Err(BridgeError::BridgeUnavailable),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS), read)
        .await
        .map_err(|_| BridgeError::Timeout)?
}

async fn capture<S: OpsShell>(shell: &S, filter: &Filter) -> Reply {
    match shell.instance_state().await {
        Ok(state) if state == "running" => (),
        Ok(_) => return Reply::error(BridgeError::InstanceStopped, shell.now_epoch()),
        Err(_) => return Reply::error(BridgeError::InstanceUnavailable, shell.now_epoch()),
    }
    match shell.stock_movers_payload(filter).await {
        Ok(output) => decode(&output, shell.now_epoch(), filter),
        Err(code) => Reply::error(code, shell.now_epoch()),
    }
}
async fn bounded_capture<S: OpsShell>(shell: &S, filter: &Filter) -> Reply {
    match tokio::time::timeout(
        std::time::Duration::from_secs(BRIDGE_TIMEOUT_SECS),
        capture(shell, filter),
    )
    .await
    {
        Ok(reply) => reply,
        Err(_) => Reply::error(BridgeError::Timeout, shell.now_epoch()),
    }
}
/// Bound on reading the published universe before falling back to the box.
const PUBLISHED_READ_TIMEOUT_MILLIS: u64 = 2_000;

/// Plan PR MV (2026-11-06): the reply from the published movers universe, or
/// why it cannot be used. A chosen minute (`as_of`) always goes to the box.
/// Cold path: one bounded GetObject, O(bytes) parse, O(n log n) select.
pub(crate) async fn from_published<S: OpsShell>(
    shell: &S,
    filter: &Filter,
) -> Result<Reply, &'static str> {
    if filter.as_of.is_some() {
        return Err("as_of");
    }
    let bytes = tokio::time::timeout(
        std::time::Duration::from_millis(PUBLISHED_READ_TIMEOUT_MILLIS),
        shell.movers_universe_object(),
    )
    .await
    .map_err(|_| "timeout")?
    .map_err(|_| "unavailable")?;
    if bytes.len() > MAX_UNIVERSE_BYTES {
        return Err("too_large");
    }
    let universe: MoversUniverse = serde_json::from_slice(&bytes).map_err(|_| "corrupt")?;
    let now = shell.now_epoch();
    universe
        .usable_at(now)
        .map_err(|refusal| refusal.as_str())?;
    let reply = select(&universe, filter);
    if !reply.valid_at(now, filter) {
        return Err("reply_invalid");
    }
    Ok(reply)
}

/// Bound on writing the viewer heartbeat.
const HEARTBEAT_TIMEOUT_MILLIS: u64 = 1_500;

/// Claims this container's heartbeat slot: `true` (and the slot) when the
/// last write was at least [`VIEWER_HEARTBEAT_INTERVAL_SECS`] ago or stamped
/// in the future (a clock step). One CAS, so two concurrent calls write once.
fn claim_heartbeat(last: &AtomicI64, now: i64) -> bool {
    let prev = last.load(Ordering::Relaxed);
    let due = now.saturating_sub(prev) >= VIEWER_HEARTBEAT_INTERVAL_SECS
        || prev > now.saturating_add(UNIVERSE_MAX_FUTURE_SKEW_SECS);
    due && last
        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Plan PR MV review: tells the box someone is viewing live movers, so it
/// publishes the universe (every 5 s for the next 120 s) instead of always.
/// At most once per 30 s per container; never for a chosen minute (`as_of`
/// is always answered by the box). A failed write frees the slot so the next
/// call retries; it never affects the reply.
async fn note_viewer<S: OpsShell>(shell: &S, filter: &Filter) {
    if filter.as_of.is_some() {
        return;
    }
    let now = shell.now_epoch();
    let cache = shell.stock_movers_cache();
    if let Some(cache) = cache
        && !claim_heartbeat(&cache.heartbeat_at, now)
    {
        return;
    }
    let Ok(body) = serde_json::to_vec(&ViewerHeartbeat::written_at(now)) else {
        return;
    };
    let written = tokio::time::timeout(
        std::time::Duration::from_millis(HEARTBEAT_TIMEOUT_MILLIS),
        shell.put_viewer_heartbeat(body),
    )
    .await;
    let reason = match written {
        Ok(Ok(())) => return,
        Ok(Err(reason)) => reason,
        Err(_) => "timed out".to_string(),
    };
    if let Some(cache) = cache {
        // Free the slot only if no later call has claimed it since.
        let freed = cache
            .heartbeat_at
            .compare_exchange(now, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
        tracing::debug!(freed, "movers viewer heartbeat slot");
    }
    tracing::info!(reason, "movers viewer heartbeat not written");
}

/// Called only after existing operator authentication and filter validation.
/// Answers from the published universe when it is usable (about 0.1-0.3 s),
/// else through the box exactly as before. One log line per call names the
/// source and the elapsed time.
pub async fn read<S: OpsShell>(shell: &S, filter: &Filter) -> Reply {
    if !filter.valid() {
        return Reply::error(BridgeError::InvalidResponse, shell.now_epoch());
    }
    let started = std::time::Instant::now();
    let (published, ()) = tokio::join!(from_published(shell, filter), note_viewer(shell, filter));
    let fallback = match published {
        Ok(reply) => {
            tracing::info!(
                source = "s3",
                elapsed_ms = started.elapsed().as_millis() as u64,
                "stock movers answered"
            );
            return reply;
        }
        Err(reason) => reason,
    };
    let reply = read_from_box(shell, filter).await;
    tracing::info!(
        source = "ssm",
        fallback,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "stock movers answered"
    );
    reply
}

/// The SSM path, unchanged from before plan PR MV.
async fn read_from_box<S: OpsShell>(shell: &S, filter: &Filter) -> Reply {
    let Some(cache) = shell.stock_movers_cache() else {
        return bounded_capture(shell, filter).await;
    };
    let Ok(mut slot) = cache.value.try_lock() else {
        return Reply::error(BridgeError::Busy, shell.now_epoch());
    };
    if let Some((stored, held, reply)) = slot.as_ref()
        && held == filter
        && stored.elapsed().as_secs() < CACHE_SECONDS
        && reply.valid_at(shell.now_epoch(), filter)
    {
        return reply.clone();
    }
    let reply = bounded_capture(shell, filter).await;
    *slot = Some((tokio::time::Instant::now(), filter.clone(), reply.clone()));
    reply
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_claim_heartbeat_once_per_interval_and_after_a_clock_step() {
        let last = AtomicI64::new(0);
        let now = 1_800_000_000;
        assert!(claim_heartbeat(&last, now));
        assert_eq!(last.load(Ordering::Relaxed), now);
        assert!(!claim_heartbeat(&last, now));
        assert!(!claim_heartbeat(
            &last,
            now + VIEWER_HEARTBEAT_INTERVAL_SECS - 1
        ));
        assert!(claim_heartbeat(&last, now + VIEWER_HEARTBEAT_INTERVAL_SECS));
        // The clock stepped back past the skew bound: write again.
        let back = now - UNIVERSE_MAX_FUTURE_SKEW_SECS - 1;
        assert!(claim_heartbeat(&last, back));
        assert!(!claim_heartbeat(&last, back + 1));
    }

    #[test]
    fn filtered_decode_never_relays_legacy_or_unbounded_output() {
        let now = 1_800_000_000;
        let f = Filter::default();
        let expected = Reply::error(BridgeError::InvalidResponse, now);
        for output in ["{", "server stderr", "{\"version\":1}"] {
            assert_eq!(decode(output, now, &f), expected);
        }
        assert_eq!(
            decode("", now, &f),
            Reply::error(BridgeError::BridgeUnavailable, now)
        );
        assert_eq!(
            decode(&" ".repeat(MAX_WIRE_BYTES + 1), now, &f),
            Reply::error(BridgeError::ResponseTooLarge, now)
        );
        let wire = serde_json::to_string(&Reply::error(BridgeError::Timeout, now)).unwrap();
        assert_eq!(
            decode(&wire, now, &f),
            Reply::error(BridgeError::Timeout, now)
        );
        assert_eq!(
            decode(&wire, now + 61, &f),
            Reply::error(BridgeError::InvalidResponse, now + 61)
        );
    }
}
