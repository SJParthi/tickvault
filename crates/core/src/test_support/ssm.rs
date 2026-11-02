//! Test-only SDK failure seam: serialize/sign synthetic requests, then refuse
//! before transmission. No socket, shared profile, metadata endpoint or real key.

use std::future::Future;
use std::sync::{Arc, Mutex};

use aws_sdk_ssm::Client;
use aws_sdk_ssm::config::interceptors::BeforeTransmitInterceptorContextRef;
use aws_sdk_ssm::config::{ConfigBag, Intercept, RuntimeComponents};

tokio::task_local! {
    static FIXTURE: FailureFixture;
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) path: String,
    pub(crate) with_decryption: bool,
}

#[derive(Clone, Debug)]
struct RejectBeforeTransmit(Arc<Mutex<Vec<Request>>>);

impl Intercept for RejectBeforeTransmit {
    fn name(&self) -> &'static str {
        "SyntheticSsmFailure"
    }

    fn read_before_transmit(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _components: &RuntimeComponents,
        _config: &mut ConfigBag,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let body = context
            .request()
            .body()
            .bytes()
            .expect("buffered SSM request");
        let json: serde_json::Value = serde_json::from_slice(body).expect("SSM JSON request");
        let request = Request {
            path: json["Name"]
                .as_str()
                .expect("GetParameter Name")
                .to_string(),
            with_decryption: json["WithDecryption"]
                .as_bool()
                .expect("GetParameter WithDecryption"),
        };
        self.0.lock().expect("request recorder lock").push(request);
        Err(std::io::Error::other("synthetic SSM failure before transmission").into())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FailureFixture {
    client: Client,
    requests: Arc<Mutex<Vec<Request>>>,
    public_ip_urls: Arc<Mutex<Vec<String>>>,
}

impl FailureFixture {
    pub(crate) fn new() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let config = aws_sdk_ssm::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_ssm::config::Region::new("ap-south-1"))
            .endpoint_url("http://127.0.0.1:9")
            .retry_config(aws_sdk_ssm::config::retry::RetryConfig::disabled())
            .credentials_provider(aws_sdk_ssm::config::Credentials::new(
                "synthetic-access-key",
                "synthetic-secret-key",
                None,
                None,
                "unit-test",
            ))
            .interceptor(RejectBeforeTransmit(Arc::clone(&requests)))
            .build();
        Self {
            client: Client::from_conf(config),
            requests,
            public_ip_urls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        FIXTURE.scope(self.clone(), future).await
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("request recorder lock").clone()
    }

    pub(crate) fn public_ip_attempts(&self) -> usize {
        self.public_ip_urls().len()
    }

    pub(crate) fn public_ip_urls(&self) -> Vec<String> {
        self.public_ip_urls
            .lock()
            .expect("IP recorder lock")
            .clone()
    }

    pub(crate) fn assert_requests(&self, minimum: usize, maximum: usize, suffixes: &[&str]) {
        let requests = self.requests();
        assert!(
            (minimum..=maximum).contains(&requests.len()),
            "unexpected SSM attempt count: {requests:?}"
        );
        if minimum == maximum && minimum == suffixes.len() {
            for suffix in suffixes {
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.path.ends_with(suffix))
                        .count(),
                    1,
                    "required parameter must be attempted once: {suffix}"
                );
            }
        }
        for request in requests {
            assert!(request.path.starts_with("/tickvault/unit-test/"));
            assert!(
                suffixes.iter().any(|suffix| request.path.ends_with(suffix)),
                "unexpected path: {}",
                request.path
            );
            assert!(
                request.with_decryption,
                "secret reads must request decryption"
            );
        }
    }
}

pub(crate) fn scoped_client() -> Client {
    FIXTURE
        .try_with(|fixture| fixture.client.clone())
        .expect("unit tests must install a synthetic SSM fixture before client construction")
}

pub(crate) fn environment_override() -> Option<String> {
    FIXTURE.try_with(|_| "unit-test".to_string()).ok()
}

pub(crate) fn record_public_ip_attempt(url: &str) {
    FIXTURE
        .try_with(|fixture| {
            fixture
                .public_ip_urls
                .lock()
                .expect("IP recorder lock")
                .push(url.to_string())
        })
        .expect("unit tests must install a public-IP fixture before transport");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[should_panic(expected = "unit tests must install a synthetic SSM fixture")]
    async fn unscoped_factory_fails_before_provider_configuration() {
        let _ = crate::auth::secret_manager::create_ssm_client().await;
    }

    #[tokio::test]
    async fn fixtures_are_task_local_and_do_not_leak_after_scope() {
        let first = FailureFixture::new();
        let second = FailureFixture::new();
        let read = || async {
            let client = crate::auth::secret_manager::create_ssm_client().await;
            crate::auth::secret_manager::fetch_secret(&client, "/tickvault/unit-test/probe").await
        };
        let (a, b) = tokio::join!(first.scope(read()), second.scope(read()));
        assert!(a.is_err() && b.is_err());
        first.assert_requests(1, 1, &["/probe"]);
        second.assert_requests(1, 1, &["/probe"]);
        assert!(
            environment_override().is_none(),
            "scope must be removed after completion"
        );
    }

    #[test]
    fn test_assert_requests_rejects_incomplete_misdirected_or_unencrypted_records() {
        let request = |path: &str, with_decryption| Request {
            path: path.into(),
            with_decryption,
        };
        let left = request("/tickvault/unit-test/left", true);
        let right = request("/tickvault/unit-test/right", true);
        let suffixes: &[&str] = &["/left", "/right"];
        for (case, records, minimum, maximum, accepted) in [
            (
                "exact complete",
                vec![left.clone(), right.clone()],
                2,
                2,
                true,
            ),
            ("ranged subset", vec![left.clone()], 1, 2, true),
            ("too few", vec![left.clone()], 2, 2, false),
            ("too many", vec![left.clone(), right.clone()], 1, 1, false),
            (
                "wrong prefix",
                vec![request("/tickvault/prod/left", true)],
                1,
                2,
                false,
            ),
            (
                "wrong suffix",
                vec![request("/tickvault/unit-test/other", true)],
                1,
                2,
                false,
            ),
            (
                "not decrypted",
                vec![request("/tickvault/unit-test/left", false)],
                1,
                2,
                false,
            ),
            (
                "duplicate left missing right",
                vec![left.clone(), left],
                2,
                2,
                false,
            ),
        ] {
            // Build only the existing synthetic client. No request is sent;
            // malformed recorded inputs must not pass the assertion helper.
            let fixture = FailureFixture::new();
            *fixture.requests.lock().expect("fixture records") = records;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fixture.assert_requests(minimum, maximum, suffixes);
            }));
            assert_eq!(result.is_ok(), accepted, "{case}");
        }
    }
}
