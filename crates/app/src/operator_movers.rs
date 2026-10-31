//! One read-only local API request for the public operator portal.
//! Invoked by a standalone binary: an older app never sees an unknown subcommand.
//! No broker calls, service startup, database writes, or token output.

use secrecy::ExposeSecret;
use serde::Deserialize;
use tickvault_common::operator_movers_wire::{
    BridgeError, MAX_SOURCE_BYTES, MAX_SOURCE_ROWS, MAX_WIRE_BYTES, Outcome, PROTOCOL_VERSION,
    Reply, SELECTION_LIMIT, Snapshot, SourceStatus, Stock,
};

const LOCAL_API_TIMEOUT_SECS: u64 = 5;
const CAPTURE_TIMEOUT_SECS: u64 = 12;

#[derive(Deserialize)]
struct Source {
    status: SourceStatus,
    market_open: bool,
    as_of_ist: Option<String>,
    data_age_secs: Option<i64>,
    fno_stocks_total: usize,
    stocks_priced: usize,
    window_secs: i64,
    value_basis: String,
    stocks: Vec<Stock>,
}

/// Preserve the canonical API's prices, status and membership; select only its rankings.
fn select_source(body: &[u8], now: i64) -> Result<Reply, BridgeError> {
    if body.len() > MAX_SOURCE_BYTES {
        return Err(BridgeError::ResponseTooLarge);
    }
    let source: Source = serde_json::from_slice(body).map_err(|_| BridgeError::InvalidResponse)?;
    if source.stocks.len() > MAX_SOURCE_ROWS {
        return Err(BridgeError::ResponseTooLarge);
    }
    if source.stocks.len() != source.stocks_priced || !source.stocks.iter().all(Stock::valid) {
        return Err(BridgeError::InvalidResponse);
    }
    let mut ids = std::collections::HashSet::new();
    let mut previous = f64::INFINITY;
    let mut null_seen = false;
    for row in &source.stocks {
        if !ids.insert(row.security_id) {
            return Err(BridgeError::InvalidResponse);
        }
        if let Some(pct) = row.change_pct {
            if null_seen || pct > previous {
                return Err(BridgeError::InvalidResponse);
            }
            previous = pct;
        } else {
            null_seen = true;
        }
    }
    let positive = |s: &&Stock| s.change_pct.is_some_and(|p| p > 0.0);
    let negative = |s: &&Stock| s.change_pct.is_some_and(|p| p < 0.0);
    let snapshot = Snapshot {
        status: source.status,
        market_open: source.market_open,
        as_of_ist: source.as_of_ist,
        data_age_secs: source.data_age_secs,
        fno_stocks_total: source.fno_stocks_total,
        stocks_priced: source.stocks_priced,
        window_secs: source.window_secs,
        value_basis: source.value_basis,
        selection_limit: SELECTION_LIMIT,
        gainers_total: source.stocks.iter().filter(positive).count(),
        losers_total: source.stocks.iter().filter(negative).count(),
        gainers: source
            .stocks
            .iter()
            .filter(positive)
            .take(SELECTION_LIMIT)
            .cloned()
            .collect(),
        losers: source
            .stocks
            .iter()
            .rev()
            .filter(negative)
            .take(SELECTION_LIMIT)
            .cloned()
            .collect(),
    };
    if !snapshot.valid() {
        return Err(BridgeError::InvalidResponse);
    }
    let reply = Reply {
        version: PROTOCOL_VERSION,
        captured_at_epoch_secs: now,
        outcome: Outcome::Snapshot { snapshot },
    };
    // SSM returns at most 24,000 characters; stay below that with a byte budget.
    if serde_json::to_vec(&reply)
        .map_err(|_| BridgeError::InvalidResponse)?
        .len()
        > MAX_WIRE_BYTES
    {
        return Err(BridgeError::ResponseTooLarge);
    }
    Ok(reply)
}

async fn read_local() -> Result<Reply, BridgeError> {
    // Same secret resolver as application startup; the value stays in this process.
    let token = tickvault_core::auth::secret_manager::fetch_api_bearer_token()
        .await
        .map_err(|_| BridgeError::CredentialsUnavailable)?;
    if token.expose_secret().is_empty() {
        return Err(BridgeError::CredentialsUnavailable);
    }
    let client = local_client()?;
    let response = client
        .get("http://127.0.0.1:3001/api/fno-stocks") // APPROVED: private local canonical API, never user-controlled.
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .map_err(|_| BridgeError::AppUnavailable)?;
    read_response(response).await
}

fn local_client() -> Result<reqwest::Client, BridgeError> {
    reqwest::Client::builder()
        .no_proxy() // APPROVED: only the fixed loopback destination below; no cloud proxy receives the token.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(LOCAL_API_TIMEOUT_SECS))
        .build()
        .map_err(|_| BridgeError::AppUnavailable)
}

async fn read_response(mut response: reqwest::Response) -> Result<Reply, BridgeError> {
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(BridgeError::AppUnauthorized);
    }
    if response.status().as_u16() != 200 {
        return Err(BridgeError::AppUnavailable);
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_SOURCE_BYTES as u64)
    {
        return Err(BridgeError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BridgeError::AppUnavailable)?
    {
        if chunk.len() > MAX_SOURCE_BYTES.saturating_sub(body.len()) {
            return Err(BridgeError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    select_source(&body, chrono::Utc::now().timestamp())
}

/// A complete bounded response, including failure states; errors never contain secret-bearing diagnostics.
async fn capture() -> Reply {
    match tokio::time::timeout(
        std::time::Duration::from_secs(CAPTURE_TIMEOUT_SECS),
        read_local(),
    )
    .await
    {
        Ok(Ok(reply)) => reply,
        Ok(Err(code)) => Reply::error(code, chrono::Utc::now().timestamp()),
        Err(_) => Reply::error(BridgeError::Timeout, chrono::Utc::now().timestamp()),
    }
}

/// Standalone CLI output. No tracing subscriber: SSM receives only the JSON envelope.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _provider = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let reply = capture().await;
    let out = std::io::stdout();
    let mut lock = out.lock();
    serde_json::to_writer(&mut lock, &reply)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn source(rows: usize) -> Value {
        // Serialize the real app API types so field drift cannot be hidden by a parallel fixture schema.
        let stocks = (0..rows)
            .map(|i| {
                let change = (rows / 2) as f64 - i as f64;
                tickvault_api::handlers::fno_stocks::FnoStockRow {
                    security_id: i as u64 + 1,
                    segment: "NSE_EQ",
                    name: format!("STOCK{i}"),
                    ltp: 100.0,
                    prev_close: Some(99.0),
                    change: Some(change),
                    change_pct: Some(change),
                    volume: Some(1000),
                    value_cr: Some(1.0),
                    last_trade_ist: "10:15:00".into(),
                }
            })
            .collect();
        serde_json::to_value(tickvault_api::handlers::fno_stocks::FnoStocksResponse {
            status: if rows == 0 { "closed" } else { "live" },
            market_open: rows != 0,
            as_of_ist: (rows != 0).then(|| "10:15:00".into()),
            data_age_secs: (rows != 0).then_some(2),
            fno_stocks_total: rows.max(236),
            stocks_priced: rows,
            window_secs: 900,
            value_basis: "day volume x average traded price / 1 crore",
            stocks,
        })
        .unwrap()
    }

    fn selected(value: &Value) -> Result<Reply, BridgeError> {
        select_source(&serde_json::to_vec(value).unwrap(), 1_800_000_000)
    }

    #[test]
    fn canonical_large_universe_selects_complete_top_twenty_each_way() {
        let input = source(1000);
        assert!(serde_json::to_vec(&input).unwrap().len() > 24_000);
        let reply = selected(&input).unwrap();
        assert!(serde_json::to_vec(&reply).unwrap().len() < MAX_WIRE_BYTES);
        let Outcome::Snapshot { snapshot } = reply.outcome else {
            panic!("expected snapshot")
        };
        assert_eq!((snapshot.gainers_total, snapshot.losers_total), (500, 499));
        assert_eq!((snapshot.gainers.len(), snapshot.losers.len()), (20, 20));
        assert_eq!(snapshot.gainers[0].security_id, 1);
        assert_eq!(snapshot.losers[0].security_id, 1000);
        assert_eq!(snapshot.losers[19].security_id, 981);
        assert_eq!(
            snapshot.gainers[0].change_pct,
            input["stocks"][0]["change_pct"].as_f64()
        );
    }

    #[test]
    fn closed_is_preserved_with_no_invented_prices() {
        let reply = selected(&source(0)).unwrap();
        let Outcome::Snapshot { snapshot } = reply.outcome else {
            panic!("expected snapshot")
        };
        assert_eq!(snapshot.status, SourceStatus::Closed);
        assert_eq!(snapshot.fno_stocks_total, 236);
        assert_eq!(snapshot.stocks_priced, 0);
        assert!(snapshot.gainers.is_empty() && snapshot.losers.is_empty());
    }

    #[test]
    fn stale_and_missing_previous_close_remain_distinct() {
        let mut input = source(5);
        input["status"] = json!("stale");
        input["data_age_secs"] = json!(90);
        input["stocks"][4]["change_pct"] = Value::Null;
        let reply = selected(&input).unwrap();
        let Outcome::Snapshot { snapshot } = reply.outcome else {
            panic!("expected snapshot")
        };
        assert_eq!(snapshot.status, SourceStatus::Stale);
        assert_eq!(snapshot.stocks_priced, 5);
        assert_eq!((snapshot.gainers_total, snapshot.losers_total), (2, 1));
    }

    #[test]
    fn malformed_or_inconsistent_source_is_refused_as_a_whole() {
        let base = source(5);
        for (pointer, replacement) in [
            ("/stocks_priced", json!(4)),
            ("/status", json!("invented")),
            ("/market_open", json!(false)),
            ("/data_age_secs", json!(-1)),
            ("/data_age_secs", json!(61)),
            ("/as_of_ist", Value::Null),
            ("/stocks/1/security_id", json!(1)),
            ("/stocks/1/change_pct", json!(2000.0)),
            ("/stocks/0/segment", json!("BSE_EQ")),
            ("/stocks/0/ltp", json!(0.0)),
            ("/stocks/0/name", json!("x".repeat(129))),
            ("/stocks/0/name", json!("a\nb")),
            ("/window_secs", json!(0)),
            ("/fno_stocks_total", json!(1)),
        ] {
            let mut altered = base.clone();
            *altered.pointer_mut(pointer).unwrap() = replacement;
            assert_eq!(
                selected(&altered),
                Err(BridgeError::InvalidResponse),
                "{pointer}"
            );
        }
        let mut null_first = base;
        null_first["stocks"][0]["change_pct"] = Value::Null;
        assert_eq!(selected(&null_first), Err(BridgeError::InvalidResponse));
    }

    #[test]
    fn bounded_transport_refuses_truncation_extra_rows_and_oversized_strings() {
        assert_eq!(
            select_source(&vec![b' '; MAX_SOURCE_BYTES + 1], 0),
            Err(BridgeError::ResponseTooLarge)
        );
        assert_eq!(selected(&source(1001)), Err(BridgeError::ResponseTooLarge));
        let bytes = serde_json::to_vec(&source(10)).unwrap();
        assert_eq!(
            select_source(&bytes[..bytes.len() - 1], 0),
            Err(BridgeError::InvalidResponse)
        );
        let mut escaped = source(100);
        for row in escaped["stocks"].as_array_mut().unwrap() {
            row["name"] = json!("\\".repeat(128));
        }
        // Even maximally escaped, individually valid symbols cannot exceed the SSM envelope cap silently.
        match selected(&escaped) {
            Ok(reply) => assert!(serde_json::to_vec(&reply).unwrap().len() <= MAX_WIRE_BYTES),
            Err(error) => assert_eq!(error, BridgeError::ResponseTooLarge),
        }
    }

    #[test]
    fn wire_refuses_unknown_protocol_and_bad_capture_clock() {
        let mut reply = selected(&source(10)).unwrap();
        assert!(reply.valid_at(1_800_000_060));
        assert!(!reply.valid_at(1_800_000_061));
        assert!(!reply.valid_at(1_799_999_994));
        reply.version += 1;
        assert!(!reply.valid_at(1_800_000_000));
    }

    struct HttpFixture {
        url: String,
        task: tokio::task::JoinHandle<()>,
        finish: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl HttpFixture {
        async fn start(redirect: bool) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let _provider = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (finish, finished) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let (mut stream, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut part = [0u8; 1024];
                    let n = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        stream.read(&mut part),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert!(n > 0 && request.len() + n <= 4096);
                    request.extend_from_slice(&part[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                if redirect {
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{address}/must-not-follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                } else {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    let mut payload = vec![b' '; MAX_SOURCE_BYTES + 1];
                    let chunk_header = format!("{:x}\r\n", payload.len());
                    payload.extend_from_slice(b"\r\n");
                    stream.write_all(chunk_header.as_bytes()).await.unwrap();
                    // Reader may close immediately on reaching its cap. A cancelled write is expected then.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        stream.write_all(&payload),
                    )
                    .await
                    {
                        Ok(Ok(())) => (),
                        Ok(Err(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                            ) =>
                        {
                            return;
                        }
                        other => panic!("unexpected fixture write outcome: {other:?}"),
                    }
                    // Deliberately omit the final chunk: a bounded reader must refuse before EOF.
                    let _finished = finished.await;
                }
            });
            Self {
                url: format!("http://{address}/api/fno-stocks"),
                task,
                finish: Some(finish),
            }
        }

        async fn close(mut self) {
            if let Some(finish) = self.finish.take() {
                let _sent = finish.send(());
            }
            tokio::time::timeout(std::time::Duration::from_secs(3), &mut self.task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn private_http_client_refuses_redirects() {
        let fixture = HttpFixture::start(true).await;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            local_client().unwrap().get(&fixture.url).send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status().as_u16(), 302);
        assert_eq!(
            read_response(response).await,
            Err(BridgeError::AppUnavailable)
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn stream_limit_refuses_oversized_body_before_peer_finishes_response() {
        let fixture = HttpFixture::start(false).await;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            local_client().unwrap().get(&fixture.url).send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.content_length(), None);
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), read_response(response)).await;
        fixture.close().await;
        assert_eq!(result.unwrap(), Err(BridgeError::ResponseTooLarge));
    }
}
