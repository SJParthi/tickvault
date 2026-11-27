use super::*;
use crate::test_support::{
    Captured, LoopbackResponse, json_error, loopback_sdk_config, recorded, start_loopback_aws,
};
use reqwest::{Method, StatusCode, header::AUTHORIZATION};

const BINARY: &str = "37328206793e099b0781c4b449a0859e17387a73";
const TARGET: &str = "49dd25239bde21411008b11bd76c51d5e0f8284f";

/// Use the production request builder and response reader. Only the test
/// translates the reviewed HTTPS origin to a one-request loopback endpoint.
async fn comparison_fixture(token: &str, status: StatusCode, body: Value) -> BuildComparison {
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let path = comparison_path(DEFAULT_GH_REPO, BINARY, TARGET).unwrap();
    let mut request = github_request(&http, token, &path, None)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.method(), Method::GET);
    assert_eq!(request.url().as_str(), format!("{GITHUB_API_BASE}{path}"));

    let expected = (!token.is_empty()).then(|| format!("Bearer {token}"));
    assert_eq!(
        request
            .headers()
            .get(AUTHORIZATION)
            .map(|h| h.to_str().unwrap()),
        expected.as_deref()
    );
    let (endpoint, requests) = start_loopback_aws(move |_| LoopbackResponse {
        status: status.as_u16(),
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    })
    .await;
    *request.url_mut() = format!("{endpoint}{path}").parse().unwrap();
    let response = http.execute(request).await.unwrap();
    let (code, parsed) = read_github_response(response).await;
    let received = recorded(&requests);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].path, path);
    classify_comparison(BINARY, TARGET, code, &parsed)
}

#[tokio::test]
async fn public_comparison_verifies_an_ancestor_without_authorizing_a_dispatch() {
    let result = comparison_fixture(
        "",
        StatusCode::OK,
        json!({
            "status": "ahead",
            "base_commit": { "sha": BINARY },
            "merge_base_commit": { "sha": BINARY },
            "ahead_by": 2, "behind_by": 0
        }),
    )
    .await;
    assert_eq!(result.relation, BuildRelation::BinaryBehind);
    assert_eq!(result.reason, "verified_ancestry");
    assert_eq!(result.http_status, Some(200));
    assert_eq!(
        dispatch_hold_reason(false, ""),
        Some("missing_dispatch_credential")
    );
}

#[tokio::test]
async fn denied_public_or_authenticated_reads_keep_the_actual_status_and_unknown_ancestry() {
    for (token, status) in [
        ("", StatusCode::NOT_FOUND),
        ("", StatusCode::FORBIDDEN),
        ("", StatusCode::TOO_MANY_REQUESTS),
        ("fixture-invalid", StatusCode::UNAUTHORIZED),
    ] {
        let result = comparison_fixture(token, status, json!({"message": "unavailable"})).await;
        assert_eq!(result.relation, BuildRelation::Unknown);
        assert_eq!(result.reason, "github_http_error");
        assert_eq!(result.http_status, Some(status.as_u16()));
    }
}

#[test]
fn dispatch_requires_a_credential_and_remains_subject_to_the_holiday_hold() {
    let http = reqwest::Client::new();
    let path = "/repos/SJParthi/tickvault/actions/workflows/deploy-aws-after-close.yml/dispatches";
    let body = json!({"ref": "main", "inputs": {"automatic_retry": true}});
    assert!(github_request(&http, "", path, Some(&body)).is_none());
    let request = github_request(&http, "fixture-token", path, Some(&body))
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.headers()[AUTHORIZATION], "Bearer fixture-token");
    assert_eq!(dispatch_hold_reason(false, "fixture-token"), None);
    assert_eq!(
        dispatch_hold_reason(true, "fixture-token"),
        Some("holiday_stop")
    );
    assert_eq!(dispatch_hold_reason(true, ""), Some("holiday_stop"));
}

#[tokio::test]
async fn optional_ssm_credential_absence_is_distinct_from_a_denied_read() {
    for (kind, is_error) in [
        ("ParameterNotFound", false),
        ("AccessDeniedException", true),
    ] {
        let (endpoint, requests) = start_loopback_aws(move |_| json_error(kind, "fixture")).await;
        let ssm = aws_sdk_ssm::Client::new(&loopback_sdk_config(&endpoint).await);
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_env_filter("tickvault_aws_lambdas::deploy_watchdog=info")
            .with_writer(move || writer.clone())
            .finish();
        let _scope = tracing::subscriber::set_default(subscriber);
        assert!(
            cached_param(&ssm, "/fixture/optional-public-watchdog-token")
                .await
                .is_empty()
        );
        assert_eq!(recorded(&requests).len(), 1);
        let log = captured.text();
        assert_eq!(log.contains("LAMBDA-AWS-01"), is_error);
        assert_eq!(
            log.contains("optional GitHub dispatch credential is absent"),
            !is_error
        );
    }
}
