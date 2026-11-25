//! Read-only filtered mode of the dedicated operator helper. All market logic
//! remains in the canonical authenticated API; no shell or broker access here.
use secrecy::ExposeSecret;
use tickvault_common::operator_movers_wire::{BridgeError, MAX_WIRE_BYTES};
use tickvault_common::operator_stock_movers_wire::{Filter, Reply};
const CAPTURE_TIMEOUT_SECS: u64 = 12;

fn parse_arguments(arguments: &[String]) -> Option<Filter> {
    match arguments {
        [flag, encoded] if flag == "--filter" => Filter::from_hex(encoded),
        _ => None,
    }
}
async fn read_local(filter: &Filter) -> Result<Reply, BridgeError> {
    let token = tickvault_core::auth::secret_manager::fetch_api_bearer_token()
        .await
        .map_err(|_| BridgeError::CredentialsUnavailable)?;
    if token.expose_secret().is_empty() {
        return Err(BridgeError::CredentialsUnavailable);
    }
    let response = crate::operator_movers::local_client()?
        .get("http://127.0.0.1:3001/api/stock-movers") // APPROVED: fixed private canonical API; no proxy/redirects.
        .bearer_auth(token.expose_secret())
        .query(filter)
        .send()
        .await
        .map_err(|_| BridgeError::AppUnavailable)?;
    read_response(response, filter).await
}
async fn read_response(
    mut response: reqwest::Response,
    filter: &Filter,
) -> Result<Reply, BridgeError> {
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(BridgeError::AppUnauthorized);
    }
    if response.status().as_u16() != 200 {
        return Err(BridgeError::AppUnavailable);
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_WIRE_BYTES as u64)
    {
        return Err(BridgeError::ResponseTooLarge);
    }
    let mut bytes = Vec::with_capacity(MAX_WIRE_BYTES);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BridgeError::AppUnavailable)?
    {
        if chunk.len() > MAX_WIRE_BYTES.saturating_sub(bytes.len()) {
            return Err(BridgeError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    decode(&bytes, chrono::Utc::now().timestamp(), filter)
}
fn decode(bytes: &[u8], now: i64, filter: &Filter) -> Result<Reply, BridgeError> {
    if bytes.len() > MAX_WIRE_BYTES {
        return Err(BridgeError::ResponseTooLarge);
    }
    let reply: Reply = serde_json::from_slice(bytes).map_err(|_| BridgeError::InvalidResponse)?;
    if !reply.valid_at(now, filter) {
        return Err(BridgeError::InvalidResponse);
    }
    Ok(reply)
}
/// Writes only a typed v2 envelope. Bad arguments cannot start the app.
pub async fn run_filtered(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let reply = if let Some(filter) = parse_arguments(arguments) {
        match tokio::time::timeout(
            std::time::Duration::from_secs(CAPTURE_TIMEOUT_SECS),
            read_local(&filter),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(code)) => Reply::error(code, chrono::Utc::now().timestamp()),
            Err(_) => Reply::error(BridgeError::Timeout, chrono::Utc::now().timestamp()),
        }
    } else {
        Reply::error(BridgeError::InvalidResponse, chrono::Utc::now().timestamp())
    };
    serde_json::to_writer(std::io::stdout().lock(), &reply)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filtered_cli_accepts_only_a_bounded_typed_argument() {
        let f = Filter::default();
        assert_eq!(
            parse_arguments(&["--filter".into(), f.to_hex().unwrap()]),
            Some(f)
        );
        for args in [
            vec![],
            vec!["--filter".into()],
            vec!["--start".into(), "7b7d".into()],
            vec!["--filter".into(), "7b7d".into(), "extra".into()],
        ] {
            assert!(parse_arguments(&args).is_none());
        }
    }
    #[test]
    fn filtered_decode_refuses_old_truncated_large_or_expired_envelopes() {
        let now = 1_800_000_000;
        let f = Filter::default();
        let wire = serde_json::to_vec(&Reply::error(BridgeError::AppUnavailable, now)).unwrap();
        assert!(decode(&wire, now, &f).is_ok());
        assert_eq!(
            decode(&wire, now + 61, &f),
            Err(BridgeError::InvalidResponse)
        );
        assert_eq!(
            decode(&wire[..wire.len() - 1], now, &f),
            Err(BridgeError::InvalidResponse)
        );
        assert_eq!(
            decode(&vec![b' '; MAX_WIRE_BYTES + 1], now, &f),
            Err(BridgeError::ResponseTooLarge)
        );
        let old = serde_json::to_vec(&tickvault_common::operator_movers_wire::Reply::error(
            BridgeError::Timeout,
            now,
        ))
        .unwrap();
        assert_eq!(decode(&old, now, &f), Err(BridgeError::InvalidResponse));
    }
}
