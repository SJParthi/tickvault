use super::*;
use crate::test_support::{
    LoopbackResponse, aws_env_guards, env_lock, json_error, recorded, start_loopback_aws,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn relay_credential_reads_request_decryption_and_propagate_missing_or_denied_values() {
    let _lock = env_lock();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (endpoint, requests) = start_loopback_aws(|request| {
                let body: Value = serde_json::from_str(&request.body).unwrap();
                match body["Name"].as_str() {
                    Some("/fixture/relay/value") => {
                        LoopbackResponse::json(200, r#"{"Parameter":{"Value":"test-only"}}"#)
                    }
                    Some("/fixture/relay/no-value") => {
                        LoopbackResponse::json(200, r#"{"Parameter":{}}"#)
                    }
                    _ => json_error("AccessDeniedException", "fixture refusal"),
                }
            })
            .await;
            let _env = aws_env_guards(&endpoint);
            assert!(fetch_ssm_secret("/fixture/relay/value").await.unwrap() == "test-only");
            let empty = fetch_ssm_secret("/fixture/relay/no-value")
                .await
                .unwrap_err();
            assert_eq!(empty.to_string(), "SSM parameter has no value");
            assert!(fetch_ssm_secret("/fixture/relay/denied").await.is_err());
            let reads = recorded(&requests);
            assert_eq!(reads.len(), 3);
            for request in reads {
                assert_eq!(request.target, "AmazonSSM.GetParameter");
                let body: Value = serde_json::from_str(&request.body).unwrap();
                assert_eq!(body["WithDecryption"], true);
            }
        });
}

#[tokio::test]
async fn a_partial_http_failure_preserves_the_private_entry_and_both_malformed_notifications() {
    let (endpoint, requests) = start_loopback_aws(|request| {
        if request.form_value("reply_markup").is_some() {
            LoopbackResponse::json(200, r#"{"ok":true}"#)
        } else {
            LoopbackResponse::json(429, "é".repeat(201))
        }
    })
    .await;
    let records = [
        json!({"Sns": {
            "Subject": "Operator portal ready",
            "Message": "Stocks & Status",
            "MessageAttributes": {"TickvaultEntry":{"Type":"String","Value":"operator-portal"}}
        }}),
        json!({"Sns": []}),
        json!("unusable record"),
    ];
    let deliveries = fold_for_delivery(&records, 1.0, &mut AlertCache::new());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{endpoint}/botfixture/sendMessage");
    let portal = "https://portal.example.org/stocks";

    let result = send_texts(&deliveries, records.len(), |delivery| {
        post_telegram_body(&client, &url, delivery.body("123", Some(portal)))
    })
    .await;

    let failure = format!("http 429: {}", "é".repeat(200));
    assert_eq!(
        result,
        json!({"sent":1, "records":3, "failures":[failure, failure]})
    );
    assert!(!nothing_was_delivered(&result));
    let posted = recorded(&requests);
    assert_eq!(posted.len(), 3);
    for request in &posted {
        assert_eq!(request.path, "/botfixture/sendMessage");
        assert_eq!(request.form_value("chat_id").as_deref(), Some("123"));
    }
    let markup: Value =
        serde_json::from_str(&posted[0].form_value("reply_markup").unwrap()).unwrap();
    assert_eq!(markup["inline_keyboard"][0][0]["web_app"]["url"], portal);
    assert!(
        posted[0]
            .form_value("text")
            .unwrap()
            .contains("Stocks & Status")
    );
    for fallback in &posted[1..] {
        assert_eq!(
            fallback.form_value("text").as_deref(),
            Some(GENERIC_SAFE_LINE)
        );
        assert!(fallback.form_value("reply_markup").is_none());
    }
}

#[tokio::test]
async fn transport_errors_hide_the_bot_token_before_and_after_response_headers() {
    const TOKEN: &str = "synthetic-private-path-marker";
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for truncate_body in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            if truncate_body {
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\nConnection: close\r\n\r\nshort",
                    )
                    .await
                    .unwrap();
            }
            socket.shutdown().await.unwrap();
        });
        let url = format!("http://{address}/bot{TOKEN}/sendMessage");
        let err = post_telegram_body(&client, &url, "text=ping".into())
            .await
            .unwrap_err();
        peer.await.unwrap();
        assert!(!err.is_empty());
        assert!(!err.contains(TOKEN));
    }
}
