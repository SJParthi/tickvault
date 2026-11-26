// Known-answer fixtures generated independently of the verifier. The HMAC
// covers the sorted decoded fields, including the optional signature.
const TG_TIME: i64 = 1_700_000_000;
const TG_LAUNCH: &str = "query_id=abc%2B%2F%3D&user=%7B%22id%22%3A1234%2C%22first_name%22%3A%22A+%26+B%22%7D&auth_date=1700000000&signature=example_-&hash=3d8a3cf0093da70a058c893250d4bbe925a496fd547793ed7974bd9da68e2892";
const TG_OTHER: &str = "query_id=abc%2B%2F%3D&user=%7B%22id%22%3A9999%2C%22first_name%22%3A%22A+%26+B%22%7D&auth_date=1700000000&signature=example_-&hash=162da7591b7ee9f28b4eb0d3e03d5d6f42b7b97fd17cadfb1c4fa936ae938e78";
const TG_GROUP: &str = "query_id=abc%2B%2F%3D&user=%7B%22id%22%3A1234%2C%22first_name%22%3A%22A+%26+B%22%7D&auth_date=1700000000&signature=example_-&chat_type=group&hash=7eb5b31a192c10ff4ad83244bf0635d236389ee9f04ec539adbbf6f73e52e3bb";

fn telegram_mock() -> MockShell {
    let mut shell = MockShell {
        now: TG_TIME,
        instance_state: Ok("stopped".into()),
        forbid_ssm: Some("unexpected shell command"),
        ..MockShell::default()
    };
    shell
        .env
        .insert("test-bot".into(), "12345:example_test_token".into());
    shell.env.insert("test-chat".into(), "1234".into());
    shell
}
fn exchange_event(raw: &str) -> Value {
    post_event(&json!({"action":"telegram-session","init_data":raw}), None)
}

#[tokio::test]
async fn telegram_session_routes_only_fixed_market_reads() {
    let shell = telegram_mock();
    let response = route(&exchange_event(TG_LAUNCH), &shell).await;
    assert_eq!(status_of(&response), 200);
    assert_eq!(response["headers"]["cache-control"], "no-store");
    let value = body_of(&response);
    assert_eq!(value["expires_at"], TG_TIME + 43_200);
    let token = value["session_token"].as_str().unwrap();
    assert!(!token.contains(AUTH_SECRET));
    for action in ["view", "stock-movers"] {
        assert_eq!(
            status_of(&route(&post_event(&json!({"action":action}), Some(token)), &shell).await),
            200
        );
    }
    for credential in [token, AUTH_SECRET] {
        let leaked = post_event(
            &json!({"action":"view", "init_data":TG_LAUNCH}),
            Some(credential),
        );
        assert_eq!(status_of(&route(&leaked, &shell).await), 400);
    }
    for action in [
        "status",
        "start",
        "stop",
        "reboot",
        "force_stop",
        "sql",
        "QDB",
        "qdb_console_url",
        "logs",
        "feed-toggle",
        "feeds-view",
        "wipe-questdb",
        "aws_status",
        "telegram-session",
        "fno-stocks",
        "",
        "unknown",
    ] {
        let ev = post_event(
            &json!({"action":action,"force":true,"query":"SELECT 1","init_data":TG_LAUNCH}),
            Some(token),
        );
        assert_eq!(status_of(&route(&ev, &shell).await), 403, "{action}");
    }
    assert_eq!(
        status_of(
            &route(
                &post_event(&json!({"action":"view","init_data":TG_LAUNCH}), None),
                &shell
            )
            .await
        ),
        401
    );
    assert_eq!(
        status_of(
            &route(
                &post_event(&json!({"action":"stop"}), Some(TG_LAUNCH)),
                &shell
            )
            .await
        ),
        401
    );
    assert_eq!(
        status_of(&post(&shell, json!({"action":"status"})).await),
        200
    );
    assert!(shell.captured_joined().is_empty());
}

#[tokio::test]
async fn telegram_exchange_invalid_data_and_identity_fail_closed() {
    for bad in [
        TG_OTHER.to_owned(),
        TG_GROUP.to_owned(),
        TG_LAUNCH.replace("1234", "4321"),
        TG_LAUNCH.replace("signature=example_-", "signature=other"),
        TG_LAUNCH.replace("query_id", "query%0aid"),
        TG_LAUNCH.replace("example_-", "example%FF"),
        TG_LAUNCH.replace("example_-", "example%0D"),
        TG_LAUNCH.replace("example_-", "example%2"),
        TG_LAUNCH.replace("hash=", "hash=g"),
        format!("{TG_LAUNCH}&%75ser=duplicate"),
        format!("{TG_LAUNCH}&hash=ignored"),
        TG_LAUNCH.replace("%3A1234", "%3A%221234%22"),
        TG_LAUNCH.replace("%3A1234", "%3A-1234"),
        TG_LAUNCH.replace("%3A1234", "%3A0"),
        TG_LAUNCH.replace("%3A1234", "%3A1234%2C%22id%22%3A1234"),
        TG_LAUNCH.replace("%3A1234", "%3A1234%2C%22is_bot%22%3Atrue"),
        TG_LAUNCH.replace("%3A1234", "%3A1234.0"),
        String::new(),
        "x".repeat(9000),
    ] {
        assert_eq!(
            status_of(&route(&exchange_event(&bad), &telegram_mock()).await),
            401
        );
    }
    for (time, expect) in [
        (TG_TIME - 1, 401),
        (TG_TIME + 300, 200),
        (TG_TIME + 301, 401),
    ] {
        let mut shell = telegram_mock();
        shell.now = time;
        assert_eq!(
            status_of(&route(&exchange_event(TG_LAUNCH), &shell).await),
            expect
        );
    }
    for chat in ["", "-1234", "01234", "0", "not-an-id", "4503599627370496"] {
        let mut shell = telegram_mock();
        shell.env.insert("test-chat".into(), chat.into());
        assert_eq!(
            status_of(&route(&exchange_event(TG_LAUNCH), &shell).await),
            503
        );
    }
    let mut shell = telegram_mock();
    shell.env.remove("test-bot");
    assert_eq!(
        status_of(&route(&exchange_event(TG_LAUNCH), &shell).await),
        503
    );
    shell.secret.clear();
    assert_eq!(
        status_of(&route(&exchange_event(TG_LAUNCH), &shell).await),
        503
    );
}

#[tokio::test]
async fn telegram_expiry_rotation_domain_separation_and_failure_budget() {
    let mut shell = telegram_mock();
    let proof = exchange_event(TG_LAUNCH);
    let session = body_of(&route(&proof, &shell).await)["session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let action = |t: &str| post_event(&json!({"action":"view"}), Some(t));
    shell.now = TG_TIME + 43_199;
    assert_eq!(status_of(&route(&action(&session), &shell).await), 200);
    shell.now = TG_TIME + 43_200;
    for _ in 0..AUTH_FAILURE_LIMIT + 1 {
        assert_eq!(status_of(&route(&action(&session), &shell).await), 401);
    }
    assert_eq!(status_of(&route(&action(AUTH_SECRET), &shell).await), 200);
    shell.now = TG_TIME - 1;
    assert_eq!(status_of(&route(&action(&session), &shell).await), 401);
    shell.now = TG_TIME;
    assert_eq!(
        status_of(
            &route(
                &action(&mint_qdb_link_token_default_ttl(AUTH_SECRET, TG_TIME)),
                &shell
            )
            .await
        ),
        401
    );
    assert_eq!(
        status_of(&route(&action(&session.replace("read-v1", "admin-v1")), &shell).await),
        401
    );
    shell.secret = "rotated-test-key".into();
    assert_eq!(status_of(&route(&action(&session), &shell).await), 401);
    assert_eq!(status_of(&route(&action(AUTH_SECRET), &shell).await), 401);
    assert_eq!(
        status_of(&route(&action("rotated-test-key"), &shell).await),
        200
    );
    for _ in 0..AUTH_FAILURE_LIMIT {
        assert_eq!(
            status_of(&route(&exchange_event(TG_OTHER), &shell).await),
            401
        );
    }
    assert_eq!(status_of(&route(&proof, &shell).await), 429);
}

#[tokio::test]
async fn telegram_exchange_binds_explicit_post_envelope_and_cannot_upgrade_a_session() {
    let shell = telegram_mock();
    let form = json!({"action":"telegram-session","init_data":TG_LAUNCH});
    let mut object_event = post_event(&form, None);
    object_event["body"] = form.clone();
    let authenticated = route(&object_event, &shell).await;
    assert_eq!(status_of(&authenticated), 200);
    let token = body_of(&authenticated)["session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    object_event["requestContext"]["http"]["method"] = json!("PUT");
    assert_eq!(status_of(&route(&object_event, &shell).await), 401);
    object_event["body"] = json!({"action":"view"});
    object_event["headers"] = json!({"authorization":format!("Bearer {token}")});
    assert_eq!(status_of(&route(&object_event, &shell).await), 403);
    for mut event in [
        post_event(&form, Some("incorrect")),
        post_event(
            &json!({"action":"telegram-session","init_data":TG_LAUNCH,"force":true}),
            None,
        ),
        json!({"body":form}),
        json!({"httpMethod":"POST","isBase64Encoded":true,"body":form.to_string()}),
    ] {
        assert_eq!(status_of(&route(&event, &telegram_mock()).await), 401);
        event["body"] = json!({"action":"stop","init_data":TG_LAUNCH});
        assert_eq!(status_of(&route(&event, &telegram_mock()).await), 401);
    }
    let mut unavailable = telegram_mock();
    unavailable.env.remove("test-chat");
    for _ in 0..AUTH_FAILURE_LIMIT + 1 {
        assert_eq!(
            status_of(&route(&exchange_event(TG_LAUNCH), &unavailable).await),
            503
        );
    }
    assert_eq!(
        status_of(&post(&unavailable, json!({"action":"status"})).await),
        200
    );
    let shell = telegram_mock();
    // Alternating successful lower-privilege reads and invalid guesses cannot
    // clear the full operator-key failure guard.
    for _ in 1..AUTH_FAILURE_LIMIT {
        assert_eq!(
            status_of(
                &route(
                    &post_event(&json!({"action":"view"}), Some("wrong")),
                    &shell
                )
                .await
            ),
            401
        );
        assert_eq!(
            status_of(&route(&post_event(&json!({"action":"view"}), Some(&token)), &shell).await),
            200
        );
    }
    assert_eq!(
        status_of(
            &route(
                &post_event(&json!({"action":"view"}), Some("wrong")),
                &shell
            )
            .await
        ),
        401
    );
    assert_eq!(
        status_of(&post(&shell, json!({"action":"status"})).await),
        429
    );
}

#[tokio::test]
async fn telegram_exchange_requires_signed_private_chat_to_match_the_signed_user() {
    // Independent WebAppData signatures for each chat. A rejection therefore
    // tests chat authorization even when the signature itself is valid.
    let cases = [
        (
            r#"{"id":1234,"type":"private"}"#,
            "a2738d8ab5699b535a6fc15b6e139aeb24534e730bc68fdb7572595f59caeb5e",
            200,
        ),
        (
            r#"{"id":4321,"type":"private"}"#,
            "9f45c4fe0c127cc4fcdda4eb05feb23a76dba1a236fbdde999e1a57dc7af8e42",
            401,
        ),
        (
            r#"{"id":1234,"type":"group"}"#,
            "e535fa1a33aeae139f7453f6da846853c3473b6426042213b790be98b8853d1d",
            401,
        ),
        (
            "{}",
            "5615d583486a21109c78d44d7f7f2e88e1c3fa46942448cb5d081a47bd35758e",
            401,
        ),
    ];
    for (chat, signature, expected) in cases {
        let raw = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("chat", chat),
                ("user", r#"{"id":1234}"#),
                ("hash", signature),
                ("chat_type", "private"),
                ("auth_date", "1700000000"),
            ])
            .finish();
        let reply = route(&exchange_event(&raw), &telegram_mock()).await;
        assert_eq!(status_of(&reply), expected);
        assert_eq!(
            body_of(&reply)["session_token"].is_string(),
            expected == 200
        );
    }
}

#[tokio::test]
async fn telegram_exchange_rejects_malformed_bodies_and_respects_the_wire_limit() {
    let mut event = exchange_event(TG_LAUNCH);
    let wire = event["body"].as_str().unwrap();
    let padded = format!("{wire:16384}");
    event["body"] = json!(padded);
    assert_eq!(status_of(&route(&event, &telegram_mock()).await), 200);
    for body in [
        json!(format!("{padded} ")),
        json!(false),
        json!([]),
        json!("{}["),
        Value::Null,
    ] {
        event["body"] = body;
        let reply = route(&event, &telegram_mock()).await;
        assert_eq!(status_of(&reply), 401);
        assert!(body_of(&reply)["session_token"].is_null());
    }
    // The exchange itself rejects an invalid raw size before asking for the
    // credentials; only testing the route would exercise its outer body guard.
    let mut unavailable = telegram_mock();
    unavailable.env.remove("test-bot");
    for raw in ["", &"x".repeat(8193)] {
        assert!(
            telegram_session::exchange(raw, AUTH_SECRET, TG_TIME, &unavailable)
                .await
                .is_none()
        );
    }
}

#[test]
fn telegram_noncanonical_or_nonpositive_signed_expirations_are_invalid_credentials() {
    // Each MAC is valid for the exact expiry text and the fixture control key.
    for (expiry, signature) in [
        ("0", "cTEu5eUpn5NufbegEZEyyjJ77p2FlFm6kx_Yp9TxP7A"),
        ("-1", "23j3BZaOHybbFgGbTjvyyVPRrcdEf-hulDdWjFVgsUI"),
        ("01700043200", "qp0G2M17xsvWDv90fxHdLuJNVhlOLvL-kGDicsKRrlg"),
    ] {
        let headers =
            json!({"authorization": format!("Bearer tv-tg-read-v1.{expiry}.{signature}")});
        assert!(telegram_session::signed_expiry(&headers, AUTH_SECRET).is_none());
    }
}

#[test]
fn telegram_real_shell_reads_both_secure_parameters_and_fails_closed_if_either_is_missing() {
    use crate::test_support::{
        EnvVarGuard, Fleet, aws_env_guards, env_lock, recorded, start_fleet,
    };
    let _lock = env_lock();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut fleet = Fleet::default();
            fleet
                .ssm
                .insert("/local/bot".into(), "test-bot-value".into());
            fleet.ssm.insert("/local/chat".into(), "1234".into());
            let (endpoint, requests) = start_fleet(fleet).await;
            let _aws = aws_env_guards(&endpoint);
            let _bot = EnvVarGuard::set("TELEGRAM_BOT_TOKEN_SSM_PARAM", Some("/local/bot"));
            let _chat = EnvVarGuard::set("TELEGRAM_CHAT_ID_SSM_PARAM", Some("/local/chat"));
            let shell = AwsShell::new().await;
            {
                let _absent = EnvVarGuard::set("TELEGRAM_CHAT_ID_SSM_PARAM", None);
                assert_eq!(shell.telegram_identity_parameters().await, None);
                assert!(recorded(&requests).is_empty());
            }
            let expected = Some(("test-bot-value".to_owned(), "1234".to_owned()));
            assert_eq!(shell.telegram_identity_parameters().await, expected);
            assert_eq!(shell.telegram_identity_parameters().await, expected);
            let mut names = Vec::new();
            for request in recorded(&requests) {
                assert_eq!(request.target, "AmazonSSM.GetParameter");
                let body: Value = serde_json::from_str(&request.body).unwrap();
                assert_eq!(body["WithDecryption"], true);
                names.push(body["Name"].as_str().unwrap().to_owned());
            }
            names.sort();
            assert_eq!(names, ["/local/bot", "/local/chat"]);
            for key in ["TELEGRAM_BOT_TOKEN_SSM_PARAM", "TELEGRAM_CHAT_ID_SSM_PARAM"] {
                let _missing = EnvVarGuard::set(key, Some("/local/missing"));
                assert_eq!(shell.telegram_identity_parameters().await, None);
            }
        });
}
