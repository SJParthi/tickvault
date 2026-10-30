//! Test-only loopback AWS endpoint (compiled under `#[cfg(test)]` only).
//!
//! The SDK-backed adapters in this crate (`SsmSecretStore`, `SdkEc2`,
//! `SdkSsm`, `SdkSns`, ...) are thin, but they are the code that turns a
//! vendor response into the value a Lambda acts on. This module lets a test
//! drive them through the REAL AWS SDK request path — signing, serialisation,
//! HTTP, response parsing — against a scripted server on 127.0.0.1, with no
//! AWS account and no new dependency (tokio's `TcpListener`, the same idiom
//! the minter's loopback Dhan server already uses).
//!
//! Each connection is answered by `responder(&request)` and recorded, so a
//! test can assert both what the adapter did with a response and exactly what
//! it sent (for example, that a `PutParameter` carried the token and nothing
//! else). Every response closes its connection, so concurrent requests (the
//! minter reads three parameters with `try_join!`) each get their own.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One HTTP request as the loopback server saw it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoopbackRequest {
    /// The `X-Amz-Target` header (`AmazonSSM.GetParameter`), empty for the
    /// query protocols (EC2, SNS), which name the action in the body.
    pub target: String,
    /// The request body, decoded as UTF-8 (lossy).
    pub body: String,
}

impl LoopbackRequest {
    /// The `Action=` of a query-protocol (EC2 / SNS) request, if any.
    pub fn query_action(&self) -> Option<String> {
        form_urlencoded::parse(self.body.as_bytes())
            .find(|(k, _)| k == "Action")
            .map(|(_, v)| v.into_owned())
    }

    /// The value of one form field of a query-protocol request.
    pub fn form_value(&self, key: &str) -> Option<String> {
        form_urlencoded::parse(self.body.as_bytes())
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }
}

/// A scripted response: HTTP status, `Content-Type`, body.
pub(crate) struct LoopbackResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl LoopbackResponse {
    /// An `awsJson1_1` (SSM) reply.
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/x-amz-json-1.1",
            body: body.into(),
        }
    }

    /// A query-protocol (EC2 / SNS) XML reply.
    pub fn xml(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/xml",
            body: body.into(),
        }
    }
}

type Responder = Arc<dyn Fn(&LoopbackRequest) -> LoopbackResponse + Send + Sync>;

/// Reads one HTTP/1.1 request (headers, then `Content-Length` bytes of body).
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<LoopbackRequest> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut content_length = 0usize;
    let mut target = String::new();
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            if name == "content-length" {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name == "x-amz-target" {
                target = value.trim().to_string();
            }
        }
    }
    while buf.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    let end = buf.len().min(header_end + content_length);
    Some(LoopbackRequest {
        target,
        body: String::from_utf8_lossy(&buf[header_end..end]).to_string(),
    })
}

/// Starts the loopback endpoint. Returns `(endpoint_url, recorded_requests)`.
/// The server lives until the test's runtime shuts down.
pub(crate) async fn start_loopback_aws<F>(
    responder: F,
) -> (String, Arc<Mutex<Vec<LoopbackRequest>>>)
where
    F: Fn(&LoopbackRequest) -> LoopbackResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind must succeed");
    let port = listener
        .local_addr()
        .expect("local_addr must be readable")
        .port();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&recorded);
    let responder: Responder = Arc::new(responder);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let sink = Arc::clone(&sink);
            let responder = Arc::clone(&responder);
            tokio::spawn(async move {
                let Some(request) = read_request(&mut stream).await else {
                    return;
                };
                let reply = responder(&request);
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request);
                let wire = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nx-amzn-requestid: loopback\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.content_type,
                    reply.body.len(),
                    reply.body
                );
                let _ = stream.write_all(wire.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), recorded)
}

/// An SDK config aimed at the loopback endpoint: fixed region and static
/// test credentials (so nothing probes IMDS or a profile), retries off (so an
/// error reply is seen once, not three times after back-off).
pub(crate) async fn loopback_sdk_config(endpoint: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("ap-south-1"))
        .credentials_provider(aws_sdk_ssm::config::Credentials::new(
            "AKIDLOOPBACK",
            "loopback-secret",
            None,
            None,
            "loopback",
        ))
        .endpoint_url(endpoint)
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .load()
        .await
}

/// A snapshot of what the server recorded so far.
pub(crate) fn recorded(requests: &Arc<Mutex<Vec<LoopbackRequest>>>) -> Vec<LoopbackRequest> {
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The awsJson error shape the SDK maps to a modelled (non-retryable) error.
pub(crate) fn json_error(type_name: &str, message: &str) -> LoopbackResponse {
    LoopbackResponse::json(
        400,
        format!(r#"{{"__type":"{type_name}","message":"{message}"}}"#),
    )
}

/// The EC2 query-protocol error envelope.
pub(crate) fn ec2_error(code: &str, message: &str) -> LoopbackResponse {
    LoopbackResponse::xml(
        400,
        format!(
            "<Response><Errors><Error><Code>{code}</Code><Message>{message}</Message></Error></Errors><RequestID>loopback</RequestID></Response>"
        ),
    )
}

/// The SNS query-protocol error envelope.
pub(crate) fn sns_error(code: &str, message: &str) -> LoopbackResponse {
    LoopbackResponse::xml(
        400,
        format!(
            "<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code><Message>{message}</Message></Error><RequestId>loopback</RequestId></ErrorResponse>"
        ),
    )
}

/// ONE lock for every test in this crate that touches process environment
/// variables. Two modules setting `AWS_ENDPOINT_URL` (or `INSTANCE_ID`)
/// under two different locks would race each other; one shared lock cannot.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Takes [`ENV_LOCK`], recovering from a poisoned lock (a failed test must
/// not fail every later one).
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sets (or removes) one environment variable and restores the previous
/// value on drop. Only use while holding [`ENV_LOCK`].
pub(crate) struct EnvVarGuard {
    key: String,
    previous: Option<String>,
}

impl EnvVarGuard {
    pub fn set(key: &str, value: Option<&str>) -> Self {
        let previous = std::env::var(key).ok();
        match value {
            // SAFETY: every caller holds ENV_LOCK; restored on drop.
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            // SAFETY: the guard is dropped while ENV_LOCK is still held.
            Some(v) => unsafe { std::env::set_var(&self.key, v) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

/// Points the DEFAULT AWS loader (`crate::clients::sdk_config`, which every
/// `handle` uses) at the loopback endpoint through the environment, the way
/// a Lambda's execution environment supplies region and credentials: static
/// keys, a fixed region, one attempt, no IMDS, no profile files.
pub(crate) fn aws_env_guards(endpoint: &str) -> Vec<EnvVarGuard> {
    vec![
        EnvVarGuard::set("AWS_ENDPOINT_URL", Some(endpoint)),
        EnvVarGuard::set("AWS_ACCESS_KEY_ID", Some("AKIDLOOPBACK")),
        EnvVarGuard::set("AWS_SECRET_ACCESS_KEY", Some("loopback-secret")),
        EnvVarGuard::set("AWS_SESSION_TOKEN", None),
        EnvVarGuard::set("AWS_PROFILE", None),
        EnvVarGuard::set("AWS_REGION", Some("ap-south-1")),
        EnvVarGuard::set("AWS_MAX_ATTEMPTS", Some("1")),
        EnvVarGuard::set("AWS_EC2_METADATA_DISABLED", Some("true")),
        EnvVarGuard::set("AWS_CONFIG_FILE", Some("/nonexistent/tv-loopback-config")),
        EnvVarGuard::set(
            "AWS_SHARED_CREDENTIALS_FILE",
            Some("/nonexistent/tv-loopback-credentials"),
        ),
    ]
}

/// A scripted AWS account for the multi-service `handle` tests: one EC2
/// instance, a set of SSM parameters, one budget, SNS and EventBridge that
/// accept everything. Operations named in `fail` answer with that service's
/// error envelope instead.
#[derive(Debug, Clone)]
pub(crate) struct Fleet {
    /// EC2 instance state name (`running`, `stopped`, ...); `None` answers
    /// `DescribeInstances` with no reservations.
    pub instance_state: Option<&'static str>,
    pub launch_time: &'static str,
    pub public_ip: Option<&'static str>,
    /// SSM parameter name → value; a name not listed is `ParameterNotFound`.
    pub ssm: std::collections::HashMap<String, String>,
    /// Month-to-date spend for `DescribeBudget`, as the API's decimal string.
    pub spend_usd: &'static str,
    /// Operation names (`StopInstances`, `Publish`, `AmazonSSM.PutParameter`,
    /// ...) that fail.
    pub fail: Vec<&'static str>,
}

impl Default for Fleet {
    fn default() -> Self {
        Self {
            instance_state: Some("running"),
            launch_time: "2026-10-30T03:00:00.000Z",
            public_ip: Some("203.0.113.7"),
            ssm: std::collections::HashMap::new(),
            spend_usd: "12.34",
            fail: Vec::new(),
        }
    }
}

const EC2_XMLNS: &str = "http://ec2.amazonaws.com/doc/2016-11-15/";

fn ec2_instances_reply(action: &str, previous: &str, current: &str) -> LoopbackResponse {
    LoopbackResponse::xml(
        200,
        format!(
            "<{action}Response xmlns=\"{EC2_XMLNS}\"><requestId>loopback</requestId><instancesSet><item><instanceId>i-0loopback</instanceId><currentState><code>0</code><name>{current}</name></currentState><previousState><code>16</code><name>{previous}</name></previousState></item></instancesSet></{action}Response>"
        ),
    )
}

impl Fleet {
    /// Answers one request as this account would.
    pub fn respond(&self, req: &LoopbackRequest) -> LoopbackResponse {
        let op = if req.target.is_empty() {
            req.query_action().unwrap_or_default()
        } else {
            req.target.clone()
        };
        if self.fail.iter().any(|f| *f == op) {
            return if req.target.is_empty() {
                if op == "Publish" {
                    sns_error("AuthorizationError", "loopback refusal")
                } else {
                    ec2_error("UnauthorizedOperation", "loopback refusal")
                }
            } else {
                json_error("AccessDeniedException", "loopback refusal")
            };
        }
        match op.as_str() {
            "AmazonSSM.GetParameter" => {
                let name = serde_json::from_str::<serde_json::Value>(&req.body)
                    .ok()
                    .and_then(|v| v.get("Name").and_then(|n| n.as_str()).map(str::to_string))
                    .unwrap_or_default();
                match self.ssm.get(&name) {
                    Some(value) => LoopbackResponse::json(
                        200,
                        serde_json::json!({"Parameter": {"Name": name, "Type": "String", "Value": value, "Version": 1}})
                            .to_string(),
                    ),
                    None => json_error("ParameterNotFound", "no such parameter"),
                }
            }
            "AmazonSSM.PutParameter" => {
                LoopbackResponse::json(200, r#"{"Version":2,"Tier":"Standard"}"#)
            }
            "AWSBudgetServiceGateway.DescribeBudget" => LoopbackResponse::json(
                200,
                format!(
                    r#"{{"Budget":{{"BudgetName":"tv","BudgetType":"COST","TimeUnit":"MONTHLY","CalculatedSpend":{{"ActualSpend":{{"Amount":"{}","Unit":"USD"}}}}}}}}"#,
                    self.spend_usd
                ),
            ),
            "DescribeInstances" => {
                let instances = match self.instance_state {
                    None => String::new(),
                    Some(state) => format!(
                        "<item><reservationId>r-0loopback</reservationId><instancesSet><item><instanceId>i-0loopback</instanceId><instanceState><code>16</code><name>{state}</name></instanceState><launchTime>{}</launchTime>{}</item></instancesSet></item>",
                        self.launch_time,
                        self.public_ip
                            .map(|ip| format!("<ipAddress>{ip}</ipAddress>"))
                            .unwrap_or_default()
                    ),
                };
                LoopbackResponse::xml(
                    200,
                    format!(
                        "<DescribeInstancesResponse xmlns=\"{EC2_XMLNS}\"><requestId>loopback</requestId><reservationSet>{instances}</reservationSet></DescribeInstancesResponse>"
                    ),
                )
            }
            "StopInstances" => ec2_instances_reply("StopInstances", "running", "stopping"),
            "StartInstances" => ec2_instances_reply("StartInstances", "stopped", "pending"),
            "Publish" => LoopbackResponse::xml(
                200,
                "<PublishResponse xmlns=\"http://sns.amazonaws.com/doc/2010-03-31/\"><PublishResult><MessageId>loopback-message</MessageId></PublishResult><ResponseMetadata><RequestId>loopback</RequestId></ResponseMetadata></PublishResponse>",
            ),
            // EventBridge (AWSEvents.*) and anything else JSON: an empty
            // success body.
            _ if !req.target.is_empty() => LoopbackResponse::json(200, "{}"),
            _ => ec2_error("InvalidAction", "the loopback fleet does not model this"),
        }
    }
}

/// Starts a loopback endpoint backed by `fleet`.
pub(crate) async fn start_fleet(fleet: Fleet) -> (String, Arc<Mutex<Vec<LoopbackRequest>>>) {
    start_loopback_aws(move |req| fleet.respond(req)).await
}

/// The operation names of every recorded request, in arrival order.
pub(crate) fn operations(requests: &Arc<Mutex<Vec<LoopbackRequest>>>) -> Vec<String> {
    recorded(requests)
        .iter()
        .map(|r| {
            if r.target.is_empty() {
                r.query_action().unwrap_or_default()
            } else {
                r.target.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_action_and_form_value_read_the_form_body() {
        let request = LoopbackRequest {
            target: String::new(),
            body: "Action=DescribeInstances&Version=2016-11-15&InstanceId.1=i-0abc".to_string(),
        };
        assert_eq!(request.query_action().as_deref(), Some("DescribeInstances"));
        assert_eq!(
            request.form_value("InstanceId.1").as_deref(),
            Some("i-0abc")
        );
        assert_eq!(request.form_value("Missing"), None);
        assert_eq!(LoopbackRequest::default().query_action(), None);
    }

    #[tokio::test]
    async fn test_loopback_server_answers_and_records_a_raw_request() {
        let (endpoint, requests) = start_loopback_aws(|req| {
            LoopbackResponse::json(200, format!(r#"{{"echo":"{}"}}"#, req.target))
        })
        .await;
        let reply = reqwest::Client::new()
            .post(&endpoint)
            .header("x-amz-target", "Svc.Op")
            .body("payload")
            .send()
            .await
            .expect("loopback must answer");
        assert_eq!(reply.status().as_u16(), 200);
        assert_eq!(
            reply.text().await.unwrap_or_default(),
            r#"{"echo":"Svc.Op"}"#
        );
        let seen = recorded(&requests);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target, "Svc.Op");
        assert_eq!(seen[0].body, "payload");
    }
}
