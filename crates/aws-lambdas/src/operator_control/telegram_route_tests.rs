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
