//! Authenticated fixed-command bridge. Cache and in-flight exclusion are per Lambda container.
use crate::operator_control::OpsShell;
use crate::operator_movers_wire::{BridgeError, MAX_WIRE_BYTES, Reply};

const CACHE_SECONDS: u64 = 20;
const BRIDGE_TIMEOUT_SECS: u64 = 22;

#[derive(Default)]
pub struct MoversCache {
    value: tokio::sync::Mutex<Option<(tokio::time::Instant, Reply)>>,
}

fn decode(output: &str, now: i64) -> Reply {
    if output.is_empty() {
        return Reply::error(BridgeError::BridgeUnavailable, now);
    }
    if output.len() > MAX_WIRE_BYTES {
        return Reply::error(BridgeError::ResponseTooLarge, now);
    }
    match serde_json::from_str::<Reply>(output) {
        Ok(reply) if reply.valid_at(now) => reply,
        _ => Reply::error(BridgeError::InvalidResponse, now),
    }
}

async fn capture<S: OpsShell>(shell: &S) -> Reply {
    match shell.instance_state().await {
        Ok(state) if state == "running" => (),
        Ok(_) => return Reply::error(BridgeError::InstanceStopped, shell.now_epoch()),
        Err(_) => return Reply::error(BridgeError::InstanceUnavailable, shell.now_epoch()),
    }
    // This standalone executable is safe even before deployment: absent means unavailable.
    // Never invoke an unknown subcommand on an older tickvault app (that could start services).
    let output = shell.movers_payload().await;
    decode(&output, shell.now_epoch())
}

async fn bounded_capture<S: OpsShell>(shell: &S) -> Reply {
    match tokio::time::timeout(
        std::time::Duration::from_secs(BRIDGE_TIMEOUT_SECS),
        capture(shell),
    )
    .await
    {
        Ok(reply) => reply,
        Err(_) => Reply::error(BridgeError::Timeout, shell.now_epoch()),
    }
}

/// Called only after the existing operator authentication gate.
pub async fn read<S: OpsShell>(shell: &S) -> Reply {
    let Some(cache) = shell.movers_cache() else {
        return bounded_capture(shell).await;
    };
    let Ok(mut slot) = cache.value.try_lock() else {
        return Reply::error(BridgeError::Busy, shell.now_epoch());
    };
    if let Some((fetched, reply)) = slot.as_ref()
        && fetched.elapsed().as_secs() < CACHE_SECONDS
        && reply.valid_at(shell.now_epoch())
    {
        return reply.clone();
    }
    let reply = bounded_capture(shell).await;
    *slot = Some((tokio::time::Instant::now(), reply.clone()));
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator_movers_wire::Outcome;

    #[test]
    fn bounded_decode_never_relays_untyped_or_truncated_output() {
        let now = 1_800_000_000;
        for source in [
            "{\"version\":1",
            "server stderr: unauthorized",
            "{\"version\":2}",
        ] {
            assert_eq!(
                decode(source, now).outcome,
                Outcome::Error {
                    code: BridgeError::InvalidResponse
                }
            );
        }
        assert_eq!(
            decode("", now).outcome,
            Outcome::Error {
                code: BridgeError::BridgeUnavailable
            }
        );
        assert_eq!(
            decode(&" ".repeat(MAX_WIRE_BYTES + 1), now).outcome,
            Outcome::Error {
                code: BridgeError::ResponseTooLarge
            }
        );
        let valid = Reply::error(BridgeError::AppUnauthorized, now);
        let wire = serde_json::to_string(&valid).unwrap();
        assert_eq!(decode(&wire, now), valid);
        assert_eq!(
            decode(&wire, now + 61).outcome,
            Outcome::Error {
                code: BridgeError::InvalidResponse
            }
        );
    }
}
