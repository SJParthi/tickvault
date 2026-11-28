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
        delivery.post(&client, &url, "123", Some(portal))
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

#[test]
fn telegram_accepted_message_requires_a_confirmed_positive_message_id() {
    let confirmed = r#"{"ok":true,"result":{"message_id":17}}"#;
    assert!(telegram_accepted_message(200, confirmed));
    for refused_status in [199, 300, 302, 400, 429, 500] {
        assert!(!telegram_accepted_message(refused_status, confirmed));
    }
    for incomplete in [
        "invalid",
        r#"{"ok":false,"result":{"message_id":17}}"#,
        r#"{"ok":"true","result":{"message_id":17}}"#,
        r#"{"ok":true}"#,
        r#"{"ok":true,"result":{"message_id":0}}"#,
        r#"{"ok":true,"result":{"message_id":-2}}"#,
        r#"{"ok":true,"result":{"message_id":"17"}}"#,
        r#"{"ok":false,"ok":true,"result":{"message_id":17}}"#,
    ] {
        assert!(!telegram_accepted_message(200, incomplete));
    }
}

#[tokio::test]
async fn private_morning_receipt_follows_its_own_send_in_a_mixed_batch() {
    use crate::test_support::Captured;
    const ACCEPTED: &str = "f1e2d3c4-b5a6-47c8-9012-000000000001";
    const OTHER: &str = "f1e2d3c4-b5a6-47c8-9012-000000000002";
    const PRIVATE_CONTENT: &str = "private-notice-fixture";
    let notice = |subject: &str, id: &str, message: &str| {
        json!({"Sns":{
            "Subject":subject,"MessageId":id,"Message":message,
            "MessageAttributes":{"TickvaultEntry":{"Type":"String","Value":"operator-portal"}}
        }})
    };
    let mut markerless = notice(START_NOTICE_SUBJECT, OTHER, PRIVATE_CONTENT);
    markerless["Sns"]["MessageAttributes"] = json!({});
    let mut markerless_refusal = markerless.clone();
    markerless_refusal["Sns"]["Message"] = json!("simulate-refusal without marker");
    let mut invalid_marker_refusal = notice(
        START_NOTICE_SUBJECT,
        OTHER,
        "simulate-refusal invalid marker",
    );
    invalid_marker_refusal["Sns"]["MessageAttributes"]["TickvaultEntry"]["Type"] = json!("Binary");
    let mut idless = notice(START_NOTICE_SUBJECT, OTHER, PRIVATE_CONTENT);
    idless["Sns"].as_object_mut().unwrap().remove("MessageId");
    let records = [
        notice("Operator portal ready", OTHER, PRIVATE_CONTENT),
        markerless,
        markerless_refusal,
        invalid_marker_refusal,
        idless,
        notice(START_NOTICE_SUBJECT, "invalid-id-fixture", PRIVATE_CONTENT),
        notice(START_NOTICE_SUBJECT, OTHER, "simulate-refusal"),
        notice(START_NOTICE_SUBJECT, OTHER, "simulate-incomplete"),
        notice(START_NOTICE_SUBJECT, ACCEPTED, PRIVATE_CONTENT),
    ];
    let portal = "https://private-fixture.invalid/stocks";
    let private_chat = "754001932";
    let replies = fold_for_delivery(&records, 1.0, &mut AlertCache::new());
    let accepted = replies.last().unwrap();
    for (chat, url) in [("-123", Some(portal)), (private_chat, None)] {
        let (body, button_attached) = accepted.prepare(chat, url);
        assert!(!body.contains("reply_markup"));
        assert!(!button_attached);
    }
    let (endpoint, requests) = start_loopback_aws(|request| {
        let text = request.form_value("text").unwrap();
        if text.contains("simulate-refusal") {
            LoopbackResponse::json(429, format!("{PRIVATE_CONTENT}: {text}"))
        } else if text.contains("simulate-incomplete") {
            LoopbackResponse::json(200, r#"{"ok":true}"#)
        } else {
            LoopbackResponse::json(200, json!({"ok":true,"result":{
                "message_id":17,"date":1795748400,"chat":{"id":754001932,"type":"private"},
                "from":{"is_bot":true,"first_name":"fixture bot"},
                "text":PRIVATE_CONTENT.repeat(50),
                "reply_markup":{"inline_keyboard":[[{"text":"Open","web_app":{"url":"https://private-fixture.invalid/stocks"}}]]}
            }}).to_string())
        }
    })
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{endpoint}/botfixture/sendMessage");
    let capture = Captured::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let result = send_texts(&replies, records.len(), |delivery| {
        delivery.post(&client, &url, private_chat, Some(portal))
    })
    .await;
    accepted
        .clone()
        .post(&client, &url, "-123", Some(portal))
        .await
        .unwrap();
    assert!(
        accepted
            .clone()
            .post(
                &client,
                "invalid-fixture-endpoint",
                private_chat,
                Some(portal)
            )
            .await
            .is_err()
    );
    let posted = recorded(&requests);
    assert_eq!(posted.len(), records.len() + 1);
    for withheld in &posted[1..=3] {
        assert!(withheld.form_value("reply_markup").is_none());
    }
    assert_eq!(result["sent"], records.len() - 3);
    assert!(!result.to_string().contains(PRIVATE_CONTENT));
    let log = capture.text();
    let receipts: Vec<Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| entry["fields"].clone())
        .filter(|fields| fields["relay_outcome"].is_string())
        .collect();
    assert_eq!(
        receipts
            .iter()
            .map(|r| (
                r["relay_outcome"].as_str().unwrap(),
                r["sns_message_id"].as_str(),
                r["http_status"].as_i64()
            ))
            .collect::<Vec<_>>(),
        [
            ("private_button_omitted", Some(OTHER), Some(200)),
            ("private_button_omitted", Some(OTHER), Some(429)),
            ("private_button_omitted", Some(OTHER), Some(429)),
            ("sns_message_id_unavailable", None, Some(200)),
            ("sns_message_id_unavailable", None, Some(200)),
            ("bot_response_unconfirmed", Some(OTHER), Some(429)),
            ("bot_response_unconfirmed", Some(OTHER), Some(200)),
            ("telegram_accepted", Some(ACCEPTED), None),
            ("private_button_omitted", Some(ACCEPTED), Some(200)),
            ("transport_error", Some(ACCEPTED), None),
        ]
    );
    assert!(!receipts[8]["button_attached"].as_bool().unwrap());
    assert_eq!(receipts[7].as_object().unwrap().len(), 3);
    for sensitive in [
        PRIVATE_CONTENT,
        private_chat,
        portal,
        "botfixture",
        "invalid-id-fixture",
        "invalid-fixture-endpoint",
        "relative URL without a base",
    ] {
        assert!(!log.contains(sensitive));
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
