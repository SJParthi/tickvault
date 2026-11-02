//! Deploy watchdog Lambda — AWS-native safety-net for the post-merge
//! auto-deploy (rust-only phase 2b-2 wave 1 port of
//! `deploy/aws/lambda/deploy-watchdog/handler.py`, DELETED in the same PR).
//!
//! The post-merge auto-deploy runs from `deploy-aws-after-close.yml`, a
//! GitHub Actions cron (08:00 + 08:45 + 15:46 IST Mon-Fri). GitHub-hosted
//! cron is best-effort: under platform load it can be delayed 10-30+
//! minutes or skipped entirely, which would leave the box running STALE
//! code with no signal. This Lambda is the belt-and-suspenders: it runs IN
//! AWS (EventBridge -> Lambda), so it covers GitHub-cron misses even when
//! GitHub Actions is degraded. Three EventBridge triggers fire it: the
//! INSTANT instance-start event pattern (the PRIMARY morning deploy
//! trigger) plus the 08:50 / 15:51 IST cron backups.
//!
//! Every fire records the deployed binary and an observed target SHA. GitHub
//! main is preferred; the SSM fallback is the newest DEPLOY-RELEVANT commit,
//! so a newer binary may legitimately differ from that mirror.
//!
//! Only a bounded GitHub comparison of two exact commit IDs can establish
//! that the recorded binary is an ancestor of the target and authorize a
//! deployment request. Equality needs no network comparison. A newer binary,
//! divergent history, missing credentials, or unavailable comparison never
//! dispatches and never produces an "old build" notification. Unknown and
//! divergent outcomes remain explicit in structured logs and result JSON.
//!
//! The existing `tv_binary_main_sha_mismatch` metric still records mismatch,
//! independently of ancestry: mismatch is evidence, not proof of old code.
//! A successful workflow run remains diagnostic only, since an off-hours
//! no-op run can be green without replacing the binary.
//!
//! **Holiday-stop day (2026-10-20).** A stale verdict does NOT dispatch while
//! `/tickvault/<env>/holiday-stop-date` (the marker the app's holiday gate
//! writes before it stops the box on a weekday NSE holiday) is today's IST
//! date: the dispatched `deploy-aws-after-close.yml` runs `deploy-aws.yml` by
//! workflow_dispatch, which self-starts a stopped box, so a redeploy would
//! undo the holiday stop or meet it mid-swap. The read reuses
//! `start_watchdog::holiday_stop_is_today` (same parameter, same IST date,
//! same fail-open: an unreadable marker dispatches as before). One `info!`
//! line, result `"skipped": "holiday_stop"`, no page. The 24h mismatch
//! metric is still published. `deploy-aws.yml` checks the same marker
//! before it starts a stopped box, which covers the other dispatchers.
//! **Honest limit:** on the instance-start event the watchdog can fire
//! before the gate has written the marker in that same boot; that one
//! firing still dispatches, and the deploy it starts skips at the marker
//! once the box is stopped, or meets the stop during its verify.
//!
//! Behavior-parity ledger (deliberate deviations from the legacy runtime):
//! * env vars are read PER INVOCATION (the legacy runtime read them at module import;
//!   Lambda cold-starts per config change so both see current values).
//! * `tracing` structured logs replace the legacy `logging` calls 1:1
//!   (same levels, same decision points).
//! * GitHub REST uses `reqwest` (10s timeout, fixed api.github.com host)
//!   instead of urllib — same status/body tuple semantics: transport
//!   failure -> (0, {"error": "github request failed"}), unparsable body
//!   on an HTTP response -> {"error": "github http error"}.
//! * positive SSM token reads are cached for at most five minutes; empty
//!   reads and errors are not cached. Binary and target SHAs are never cached.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tracing::{error, info, warn};

mod ancestry;
use ancestry::{
    BuildComparison, BuildRelation, classify_comparison, comparison_path, validated_full_sha,
};

/// Env defaults — legacy parity (`os.environ.get(...)` fallbacks).
const DEFAULT_GH_REPO: &str = "SJParthi/tickvault";
const DEFAULT_GH_DESIRED_REF: &str = "main";
const DEFAULT_GH_DEPLOY_WORKFLOW: &str = "deploy-aws.yml";
const DEFAULT_GH_DISPATCH_WORKFLOW: &str = "deploy-aws-after-close.yml";
const DEFAULT_BINARY_SHA_PARAM: &str = "/tickvault/prod/deploy/binary-git-sha";
/// Newest deploy-relevant commit, written every 30 minutes by
/// postmerge-catchup.yml -- NOT by deploy-aws.yml.
/// Its path filter intentionally excludes unrelated workflow-only commits.
/// The mirror may therefore be an ancestor of a correctly deployed binary;
/// it must never authorize a deploy from inequality alone.
const DEFAULT_DESIRED_SHA_PARAM: &str = "/tickvault/prod/deploy/desired-git-sha";

/// B9 deploy provenance metric identity — legacy parity
/// (`_MISMATCH_METRIC_*`). PutMetricData is called DIRECTLY (bypasses the
/// Prometheus→EMF allowlist in user-data.sh.tftpl by design — this metric
/// originates in the Lambda, not the app).
const MISMATCH_METRIC_NAMESPACE: &str = "Tickvault/Prod";
const MISMATCH_METRIC_NAME: &str = "tv_binary_main_sha_mismatch";
const MISMATCH_METRIC_HOST: &str = "tickvault-prod";

const GITHUB_API_BASE: &str = "https://api.github.com"; // APPROVED: infrastructure constant — the GitHub REST API base (Lambda-side, legacy watchdog parity)
const GITHUB_TIMEOUT_SECONDS: u64 = 10;
const GITHUB_RESPONSE_BYTES_MAX: usize = 256 * 1024;
const TOKEN_CACHE_TTL_SECONDS: u64 = 300;
const TOKEN_CACHE_TTL: Duration = Duration::from_secs(TOKEN_CACHE_TTL_SECONDS);
const TOKEN_CACHE_ENTRIES_MAX: usize = 4;

// ------------------------------------------------------------------------
// Pure decision logic (no IO — unit-tested below, legacy tests ported 1:1)
// ------------------------------------------------------------------------

/// Legacy workflow-history mismatch diagnostic. This does not establish
/// ancestry and must not authorize a deployment request.
pub fn is_stale(desired_sha: Option<&str>, deployed_sha: Option<&str>) -> bool {
    let (Some(desired), Some(deployed)) = (desired_sha, deployed_sha) else {
        return false;
    };
    if desired.is_empty() || deployed.is_empty() {
        return false;
    }
    desired.trim() != deployed.trim()
}

/// B9 deploy provenance — value for the tv_binary_main_sha_mismatch metric.
///
/// Returns:
/// * `Some(1.0)` — both shas are KNOWN and differ (running binary != main HEAD)
/// * `Some(0.0)` — both shas are KNOWN and equal (healthy)
/// * `None` — either sha is unknown/empty/"unknown" → SKIP publishing (no
///   false signal on uncertainty; the alarm treats missing data as
///   notBreaching, so a skipped sample never pages)
///
/// Comparison is normalized to the lowercase first-7 characters so a full
/// 40-hex sha and a conventional short sha compare correctly.
pub fn binary_mismatch_value(binary_sha: Option<&str>, desired_sha: Option<&str>) -> Option<f64> {
    fn norm(sha: Option<&str>) -> String {
        let s = sha.unwrap_or("").trim().to_lowercase();
        if s.is_empty() || s == "unknown" {
            return String::new();
        }
        s.chars().take(7).collect()
    }
    let (b, d) = (norm(binary_sha), norm(desired_sha));
    if b.is_empty() || d.is_empty() {
        return None;
    }
    Some(if b == d { 0.0 } else { 1.0 })
}

/// Pick main HEAD from the GitHub answer, falling back to the SSM mirror.
///
/// Why a fallback exists at all: `desired_sha` needs
/// `/tickvault/<env>/operator/github-token`, and on 2026-09-02 that parameter
/// did not exist -- so the deploy-provenance comparison had been dead for its
/// whole life while `tv-<env>-binary-sha-stale` read OK, because
/// treat_missing_data = notBreaching turns a producer that declines to produce
/// into a healthy-looking alarm. The mirror removes the credential from the
/// critical path without removing the check.
///
/// The GitHub answer stays PREFERRED, so placing a token later upgrades this
/// automatically with no further change. "unknown" is rejected as a value
/// because that is the literal the portal writes for an unresolved sha, and
/// comparing it would manufacture a mismatch out of an absence.
///
/// Returns the sha and a label naming which leg answered, so the fire log
/// records the source instead of leaving it to be inferred.
#[must_use]
pub fn resolve_desired_sha<'a>(
    from_github: Option<&'a str>,
    from_ssm_mirror: Option<&'a str>,
) -> (Option<&'a str>, &'static str) {
    fn usable(sha: Option<&str>) -> bool {
        matches!(sha.map(str::trim), Some(t) if !t.is_empty() && !t.eq_ignore_ascii_case("unknown"))
    }
    if usable(from_github) {
        return (from_github, "github");
    }
    if usable(from_ssm_mirror) {
        return (from_ssm_mirror, "ssm-mirror");
    }
    (None, "none")
}

/// Legacy binary mismatch diagnostic retained for callers. Despite its
/// historical name, inequality does not prove that the binary is older.
/// Deployment authorization uses exact-SHA ancestry instead.
pub fn binary_is_stale(binary_sha: Option<&str>, desired_sha: Option<&str>) -> bool {
    binary_mismatch_value(binary_sha, desired_sha) == Some(1.0)
}

// ------------------------------------------------------------------------
// IO helpers (thin wrappers — exercised in the live deploy, legacy parity)
// ------------------------------------------------------------------------

/// Only positive token values are cached, with bounded lifetime and entries.
/// Do not derive Debug: cached values are secrets.
struct CachedParam {
    value: String,
    cached_at: Instant,
}

impl CachedParam {
    fn usable_at(&self, now: Instant) -> Option<&str> {
        let age = now.checked_duration_since(self.cached_at)?;
        (age < TOKEN_CACHE_TTL && !self.value.is_empty()).then_some(self.value.as_str())
    }
}

static PARAM_CACHE: Mutex<Option<HashMap<String, CachedParam>>> = Mutex::new(None);

/// Failure or an empty result is retried on the next invocation. Successful
/// values expire so token rotation does not require replacing warm containers.
async fn cached_param(ssm: &aws_sdk_ssm::Client, name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    {
        let mut guard = PARAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = guard.as_mut() {
            if let Some(value) = cache.get(name).and_then(|v| v.usable_at(Instant::now())) {
                return value.to_string();
            }
            cache.remove(name);
        }
    }
    match ssm
        .get_parameter()
        .name(name)
        .with_decryption(true)
        .send()
        .await
    {
        Ok(resp) => {
            let value = resp
                .parameter()
                .and_then(|p| p.value())
                .unwrap_or("")
                .to_string();
            if !value.is_empty() {
                let mut guard = PARAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                let cache = guard.get_or_insert_with(HashMap::new);
                if cache.len() >= TOKEN_CACHE_ENTRIES_MAX && !cache.contains_key(name) {
                    cache.clear();
                }
                cache.insert(
                    name.to_string(),
                    CachedParam {
                        value: value.clone(),
                        cached_at: Instant::now(),
                    },
                );
            }
            value
        }
        Err(e) => {
            // The param NAME is safe to log; the value never is.
            error!(code = "LAMBDA-AWS-01", param = name, error = %e, "ssm get_parameter failed");
            String::new()
        }
    }
}

fn append_github_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> bool {
    if chunk.len() > GITHUB_RESPONSE_BYTES_MAX.saturating_sub(body.len()) {
        return false;
    }
    body.extend_from_slice(chunk);
    true
}

/// Bound accumulated response bytes as well as the existing request timeout.
/// Page 2 of comparisons avoids GitHub's first-page file/patch payload.
async fn read_github_response(mut response: reqwest::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|n| n > GITHUB_RESPONSE_BYTES_MAX as u64)
    {
        return (0, json!({"error": "github response exceeds byte limit"}));
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if append_github_chunk(&mut body, &chunk) => {}
            Ok(Some(_)) => return (0, json!({"error": "github response exceeds byte limit"})),
            Ok(None) => break,
            Err(_) => return (0, json!({"error": "github response read failed"})),
        }
    }
    let parsed = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({"error": "github http error"}))
    };
    (status, parsed)
}

/// Minimal GitHub REST call. Returns (status, json) — legacy `_gh` parity:
/// no token -> (0, {"error": "github token not configured"}); transport
/// failure -> (0, {"error": "github request failed"}); unparsable body on
/// an HTTP response -> {"error": "github http error"}.
async fn gh(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    if token.is_empty() {
        return (0, json!({"error": "github token not configured"}));
    }
    let url = format!("{GITHUB_API_BASE}{path}");
    let mut req = if method == "POST" {
        client.post(&url)
    } else {
        client.get(&url)
    };
    req = req
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .header("user-agent", "tickvault-deploy-watchdog");
    if let Some(b) = body {
        req = req.header("content-type", "application/json").json(b);
    }
    match req.send().await {
        Ok(resp) => read_github_response(resp).await,
        // Token-redaction hardening: never log the raw reqwest error here
        // (its Display can embed the URL); the fixed literal is enough.
        Err(_) => (0, json!({"error": "github request failed"})),
    }
}

/// Current HEAD sha of the desired ref (main). Legacy `_desired_sha`.
async fn desired_sha(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    desired_ref: &str,
) -> Option<String> {
    let (status, body) = gh(
        client,
        token,
        "GET",
        &format!("/repos/{repo}/commits/{desired_ref}"),
        None,
    )
    .await;
    if status == 200 {
        return validated_full_sha(body.get("sha").and_then(Value::as_str)).map(str::to_string);
    }
    warn!(status, "could not fetch desired sha");
    None
}

/// head_sha of the most recent SUCCESSFUL deploy workflow run on the ref.
/// Legacy `_deployed_sha`.
async fn deployed_sha(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    deploy_workflow: &str,
    desired_ref: &str,
) -> Option<String> {
    let path = format!(
        "/repos/{repo}/actions/workflows/{deploy_workflow}/runs?branch={desired_ref}&status=success&per_page=1"
    );
    let (status, body) = gh(client, token, "GET", &path, None).await;
    if status == 200 {
        if let Some(first) = body
            .get("workflow_runs")
            .and_then(Value::as_array)
            .and_then(|runs| runs.first())
        {
            return first
                .get("head_sha")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        info!(
            workflow = deploy_workflow,
            r#ref = desired_ref,
            "no successful deploy run found yet"
        );
        return None;
    }
    warn!(status, "could not fetch deployed sha");
    None
}

/// Trigger the (idempotent, market-hours-guarded) auto-deploy workflow.
/// Legacy `_dispatch_deploy` — ok iff GitHub answers 201/204.
async fn dispatch_deploy(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    dispatch_workflow: &str,
    desired_ref: &str,
) -> bool {
    let (status, _) = gh(
        client,
        token,
        "POST",
        &format!("/repos/{repo}/actions/workflows/{dispatch_workflow}/dispatches"),
        Some(&json!({"ref": desired_ref})),
    )
    .await;
    let ok = status == 201 || status == 204;
    if !ok {
        error!(
            code = "LAMBDA-AWS-02",
            workflow = dispatch_workflow,
            status,
            "workflow_dispatch failed"
        );
    }
    ok
}

/// Deployed-binary git SHA from SSM (B9 deploy provenance). Read fresh on
/// every invocation (NOT via the token cache — the value changes on every
/// deploy and Lambda containers persist across invocations). Legacy
/// `_binary_sha`.
async fn read_sha_param(ssm: &aws_sdk_ssm::Client, param: &str) -> Option<String> {
    if param.is_empty() {
        return None;
    }
    match ssm.get_parameter().name(param).send().await {
        Ok(resp) => {
            let val = resp
                .parameter()
                .and_then(|p| p.value())
                .unwrap_or("")
                .trim()
                .to_string();
            validated_full_sha(Some(&val)).map(str::to_string)
        }
        Err(e) => {
            warn!(param, error = %e, "could not read sha param");
            None
        }
    }
}

/// Compare immutable commits; a missing token or comparison failure is
/// uncertainty, including when the target came from an SSM mirror.
async fn verify_build_comparison(
    client: &reqwest::Client,
    token: &str,
    repo: &str,
    binary: Option<&str>,
    desired: Option<&str>,
) -> BuildComparison {
    let (Some(binary), Some(desired)) = (validated_full_sha(binary), validated_full_sha(desired))
    else {
        return BuildComparison::unknown("invalid_or_missing_sha", None);
    };
    if binary == desired {
        return BuildComparison::current();
    }
    let Some(path) = comparison_path(repo, binary, desired) else {
        return BuildComparison::unknown("invalid_repository", None);
    };
    let (status, body) = gh(client, token, "GET", &path, None).await;
    classify_comparison(binary, desired, status, &body)
}

/// B9 deploy provenance: publish tv_binary_main_sha_mismatch so the
/// tv-<env>-binary-sha-stale alarm (Minimum >= 1 over 24h) can detect a
/// persistent mismatch. Mismatch alone does not establish ancestry. Wrapped so the
/// watchdog's core stale-deploy dispatch job NEVER breaks on metric IO
/// (legacy parity: the blanket `except` + error log).
async fn publish_binary_mismatch_metric(
    cw: &aws_sdk_cloudwatch::Client,
    binary: Option<&str>,
    desired: Option<&str>,
) {
    let Some(value) = binary_mismatch_value(binary, desired) else {
        // 2026-09-02: this was `info!`, and that is how a deploy-provenance
        // check went blind for its whole life without anyone noticing. Live
        // evidence: FOUR consecutive fires logged this line and published
        // nothing, `tv_binary_main_sha_mismatch` had ZERO datapoints in three
        // days, and `tv-<env>-binary-sha-stale` (treat_missing_data =
        // notBreaching) therefore read OK the entire time. A producer that
        // silently declines to produce is indistinguishable from a healthy
        // one when the alarm treats no-data as health.
        //
        // Root cause on that day: `/tickvault/<env>/operator/github-token`
        // does not exist, so `desired_sha` cannot ask GitHub for main HEAD.
        // The binary sha itself was fine — written by deploy-aws.yml the day
        // before — so only the COMPARISON was dead.
        //
        // ERROR, not INFO, because "I cannot do the one job I exist for" is
        // an error even when the invocation returns Ok. The message TEXT is
        // deliberately unchanged: `tv-<env>-deploy-provenance-blind`
        // (deploy-watchdog-lambda.tf) is a term filter over this log group
        // that matches on it, so rewording the string silences the alarm.
        error!(
            code = "LAMBDA-PROV-01",
            binary_sha_known = binary.is_some(),
            desired_sha_known = desired.is_some(),
            "binary/main mismatch metric skipped — a sha is unknown"
        );
        return;
    };
    let datum = aws_sdk_cloudwatch::types::MetricDatum::builder()
        .metric_name(MISMATCH_METRIC_NAME)
        .dimensions(
            aws_sdk_cloudwatch::types::Dimension::builder()
                .name("host")
                .value(MISMATCH_METRIC_HOST)
                .build(),
        )
        .value(value)
        .unit(aws_sdk_cloudwatch::types::StandardUnit::None)
        .build();
    match cw
        .put_metric_data()
        .namespace(MISMATCH_METRIC_NAMESPACE)
        .metric_data(datum)
        .send()
        .await
    {
        Ok(_) => info!(metric = MISMATCH_METRIC_NAME, value, "published"),
        Err(e) => {
            error!(code = "LAMBDA-AWS-02", metric = MISMATCH_METRIC_NAME, error = %e, "put_metric_data failed")
        }
    }
}

/// SNS publish — never fatal (legacy `_publish` parity: unset topic warns,
/// a publish failure is error-logged and swallowed).
async fn publish(sns: &aws_sdk_sns::Client, topic_arn: &str, subject: &str, message: &str) {
    if topic_arn.is_empty() {
        warn!(subject, "ALERTS_TOPIC_ARN unset — cannot publish");
        return;
    }
    if let Err(e) = sns
        .publish()
        .topic_arn(topic_arn)
        .subject(subject)
        .message(message)
        .send()
        .await
    {
        error!(code = "LAMBDA-NOTIFY-01", error = %e, "sns publish failed");
    }
}

// ------------------------------------------------------------------------
// Entry point
// ------------------------------------------------------------------------

/// First 8 chars of an optional sha — legacy parity `(sha or "")[:8]`.
fn short8(sha: Option<&str>) -> String {
    sha.unwrap_or("").chars().take(8).collect()
}

/// This notification is only sent for a verified ancestor relationship.
/// A dispatch receipt proves acceptance, not deployment completion.
fn verified_behind_alert(
    binary: Option<&str>,
    desired: Option<&str>,
    desired_source: &str,
    workflow: &str,
    dispatched: bool,
) -> (String, String) {
    let outcome = if dispatched {
        "accepted"
    } else {
        "not confirmed"
    };
    let subject = format!("Deploy watchdog: verified build lag — redeploy request {outcome}");
    let receipt = if dispatched {
        "was accepted"
    } else {
        "was not confirmed"
    };
    let message = format!(
        "The recorded binary {} is an ancestor of observed target {} (source: {}). \
         The redeploy request to {} {}. Deployment completion is not verified; \
         the existing market-hours and holiday safeguards still apply.",
        short8(binary),
        short8(desired),
        desired_source,
        workflow,
        receipt,
    );
    (subject, message)
}

/// Entry point. Idempotent — safe to run on every 5-minutes-after
/// schedule. Result JSON preserves legacy fields and adds explicit ancestry;
/// `stale`/`binary_stale` are null when the comparison is unresolved.
pub async fn handle(event: Value) -> Result<Value, lambda_runtime::Error> {
    let window = event
        .get("window")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let gh_repo = std::env::var("GH_REPO").unwrap_or_else(|_| DEFAULT_GH_REPO.to_string());
    let gh_desired_ref =
        std::env::var("GH_DESIRED_REF").unwrap_or_else(|_| DEFAULT_GH_DESIRED_REF.to_string());
    let gh_deploy_workflow = std::env::var("GH_DEPLOY_WORKFLOW")
        .unwrap_or_else(|_| DEFAULT_GH_DEPLOY_WORKFLOW.to_string());
    let gh_dispatch_workflow = std::env::var("GH_DISPATCH_WORKFLOW")
        .unwrap_or_else(|_| DEFAULT_GH_DISPATCH_WORKFLOW.to_string());
    let token_param = std::env::var("OPERATOR_GITHUB_TOKEN_PARAM").unwrap_or_default();
    let alerts_topic_arn = std::env::var("ALERTS_TOPIC_ARN").unwrap_or_default();
    let binary_sha_param =
        std::env::var("BINARY_SHA_PARAM").unwrap_or_else(|_| DEFAULT_BINARY_SHA_PARAM.to_string());
    let desired_sha_param = std::env::var("DESIRED_SHA_PARAM")
        .unwrap_or_else(|_| DEFAULT_DESIRED_SHA_PARAM.to_string());
    let holiday_stop_param = std::env::var("HOLIDAY_STOP_PARAM")
        .unwrap_or_else(|_| crate::start_watchdog::DEFAULT_HOLIDAY_STOP_PARAM.to_string());

    let config = crate::clients::sdk_config().await;
    let ssm = crate::clients::ssm(&config);
    let cw = crate::clients::cloudwatch(&config);
    let sns = crate::clients::sns(&config);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(GITHUB_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let token = cached_param(&ssm, &token_param).await;
    let desired_from_github = desired_sha(&http, &token, &gh_repo, &gh_desired_ref).await;
    // Only reach for the mirror when GitHub could not answer -- an SSM read
    // per fire is cheap, but a read nobody needs is still a read, and keeping
    // GitHub on the fast path means the behaviour is unchanged the day a
    // token is placed.
    let desired_from_mirror = if desired_from_github.is_none() {
        read_sha_param(&ssm, &desired_sha_param).await
    } else {
        None
    };
    let (resolved, desired_source) = resolve_desired_sha(
        desired_from_github.as_deref(),
        desired_from_mirror.as_deref(),
    );
    let desired = resolved.map(str::to_string);
    info!(
        desired_source,
        desired = %short8(desired.as_deref()),
        "resolved observed target for the provenance comparison"
    );
    // Read the on-box binary provenance sha ONCE per fire — used BOTH for
    // the 24h mismatch metric and the ground-truth box-binary staleness
    // check below.
    let binary = read_sha_param(&ssm, &binary_sha_param).await;
    // B9 deploy provenance: sample binary-vs-main drift on every fire (the
    // 24h alarm needs samples; a None value is silently skipped).
    publish_binary_mismatch_metric(&cw, binary.as_deref(), desired.as_deref()).await;
    let deployed = deployed_sha(
        &http,
        &token,
        &gh_repo,
        &gh_deploy_workflow,
        &gh_desired_ref,
    )
    .await;

    // Workflow history and the mismatch metric remain diagnostics. Neither
    // proves direction: only exact-SHA ancestry can authorize a dispatch.
    let github_stale = is_stale(desired.as_deref(), deployed.as_deref());
    let comparison = verify_build_comparison(
        &http,
        &token,
        &gh_repo,
        binary.as_deref(),
        desired.as_deref(),
    )
    .await;
    let binary_stale = comparison.relation.permits_dispatch();
    let stale = binary_stale;
    info!(
        window,
        desired = %short8(desired.as_deref()),
        deployed = %short8(deployed.as_deref()),
        binary = %short8(binary.as_deref()),
        desired_source,
        relation = comparison.relation.as_str(),
        verification_reason = comparison.reason,
        comparison_http_status = ?comparison.http_status,
        github_stale,
        dispatch_permitted = binary_stale,
        "deploy-watchdog verdict"
    );

    if !stale {
        if matches!(
            comparison.relation,
            BuildRelation::Unknown | BuildRelation::Diverged
        ) {
            warn!(
                relation = comparison.relation.as_str(),
                reason = comparison.reason,
                "deploy comparison unresolved: automatic dispatch held"
            );
        }
        return Ok(json!({
            "window": window,
            "stale": comparison.relation.stale_value(),
            "github_stale": github_stale,
            "binary_stale": comparison.relation.stale_value(),
            "relation": comparison.relation.as_str(),
            "verification_reason": comparison.reason,
            "comparison_http_status": comparison.http_status,
            "desired_source": desired_source,
            "dispatched": false,
            "skipped": comparison.relation.as_str(),
        }));
    }

    // 2026-10-20: the holiday gate stopped the box on purpose today (weekday
    // NSE holiday). A redeploy now would self-start it (deploy-aws.yml by
    // workflow_dispatch) or meet the stop mid-swap. Same marker read as the
    // start-watchdog, fail-open: an unreadable marker dispatches as before.
    let holiday_marker = crate::start_watchdog::SdkSsm {
        client: ssm.clone(),
    };
    if crate::start_watchdog::holiday_stop_is_today(
        &holiday_marker,
        &holiday_stop_param,
        chrono::Utc::now(),
    )
    .await
    {
        info!(
            window,
            github_stale,
            binary_stale,
            "stale deploy, but the holiday-stop marker is today: the holiday gate stopped the box on purpose — not dispatching a redeploy"
        );
        return Ok(json!({
            "window": window,
            "stale": true,
            "github_stale": github_stale,
            "binary_stale": binary_stale,
            "relation": comparison.relation.as_str(),
            "verification_reason": comparison.reason,
            "comparison_http_status": comparison.http_status,
            "desired_source": desired_source,
            "dispatched": false,
            "skipped": "holiday_stop",
        }));
    }

    let dispatched = dispatch_deploy(
        &http,
        &token,
        &gh_repo,
        &gh_dispatch_workflow,
        &gh_desired_ref,
    )
    .await;
    let (subject, message) = verified_behind_alert(
        binary.as_deref(),
        desired.as_deref(),
        desired_source,
        &gh_dispatch_workflow,
        dispatched,
    );
    publish(&sns, &alerts_topic_arn, &subject, &message).await;
    warn!(
        window,
        github_stale, binary_stale, dispatched, "stale deploy detected"
    );
    Ok(json!({
        "window": window,
        "stale": true,
        "github_stale": github_stale,
        "binary_stale": binary_stale,
        "relation": comparison.relation.as_str(),
        "verification_reason": comparison.reason,
        "comparison_http_status": comparison.http_status,
        "desired_source": desired_source,
        "dispatched": dispatched,
    }))
}

// ------------------------------------------------------------------------
// Tests — the legacy test_handler.py suite ported 1:1 (same names). The
// Legacy diagnostics stay covered. Dispatch authorization additionally needs
// verified ancestry; mismatch or successful workflow history is insufficient.
// ------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_stale_true_when_shas_differ() {
        assert!(is_stale(Some("aaaaaaa"), Some("bbbbbbb")));
    }

    #[test]
    fn test_is_stale_false_when_shas_equal() {
        assert!(!is_stale(Some("deadbeef"), Some("deadbeef")));
    }

    #[test]
    fn test_is_stale_false_when_desired_unknown() {
        // GitHub API blip fetching main HEAD — must NOT dispatch on
        // uncertainty.
        assert!(!is_stale(None, Some("deadbeef")));
        assert!(!is_stale(Some(""), Some("deadbeef")));
    }

    #[test]
    fn test_is_stale_false_when_deployed_unknown() {
        // No successful deploy run yet (fresh repo) — must NOT dispatch
        // (would loop).
        assert!(!is_stale(Some("deadbeef"), None));
        assert!(!is_stale(Some("deadbeef"), Some("")));
    }

    #[test]
    fn test_is_stale_false_when_both_unknown() {
        assert!(!is_stale(None, None));
    }

    #[test]
    fn test_is_stale_ignores_surrounding_whitespace() {
        assert!(!is_stale(Some(" deadbeef "), Some("deadbeef")));
        assert!(!is_stale(Some("deadbeef\n"), Some("deadbeef")));
    }

    #[test]
    fn test_is_stale_is_case_sensitive_on_real_shas() {
        // Git shas are lowercase hex; a case flip is a genuine difference,
        // not noise.
        assert!(is_stale(Some("ABCDEF0"), Some("abcdef0")));
    }

    // -------------------------------------------------------------------
    // B9 deploy provenance — binary_mismatch_value (pure)
    // -------------------------------------------------------------------

    fn full_sha(prefix: &str) -> String {
        format!("{prefix}{}", "0".repeat(40 - prefix.len()))
    }

    #[test]
    fn test_binary_mismatch_one_when_known_and_different() {
        assert_eq!(
            binary_mismatch_value(Some(&full_sha("aaaaaaa")), Some(&full_sha("bbbbbbb"))),
            Some(1.0)
        );
    }

    #[test]
    fn test_binary_mismatch_zero_when_known_and_equal() {
        let sha = "deadbeefcafe0123456789abcdef012345678901";
        assert_eq!(binary_mismatch_value(Some(sha), Some(sha)), Some(0.0));
    }

    #[test]
    fn test_binary_mismatch_compares_short7_prefix_and_case_insensitive() {
        // A 40-hex full sha vs its short-7 form must compare EQUAL (0.0),
        // and case is normalized (SSM/API sources could disagree on case).
        let full = "deadbeefcafe0123456789abcdef012345678901";
        assert_eq!(
            binary_mismatch_value(Some(full), Some("deadbee")),
            Some(0.0)
        );
        assert_eq!(
            binary_mismatch_value(Some(&full.to_uppercase()), Some(full)),
            Some(0.0)
        );
    }

    #[test]
    fn test_binary_mismatch_none_when_binary_unknown() {
        // Never publish a false signal on uncertainty — skip (None).
        assert_eq!(binary_mismatch_value(None, Some("deadbee")), None);
        assert_eq!(binary_mismatch_value(Some(""), Some("deadbee")), None);
        assert_eq!(
            binary_mismatch_value(Some("unknown"), Some("deadbee")),
            None
        );
    }

    #[test]
    fn resolve_desired_sha_precedence_table() {
        // The whole contract in one place: GitHub first, mirror second, and a
        // placeholder is never a sha on either leg. The named cases below
        // explain WHY each row matters; this one pins that they are the only
        // rows, so a future edit cannot quietly add a fourth behaviour.
        const CASES: &[(Option<&str>, Option<&str>, Option<&str>, &str)] = &[
            (
                Some("aaaa1111"),
                Some("bbbb2222"),
                Some("aaaa1111"),
                "github",
            ),
            (Some("aaaa1111"), None, Some("aaaa1111"), "github"),
            (None, Some("bbbb2222"), Some("bbbb2222"), "ssm-mirror"),
            (
                Some("unknown"),
                Some("bbbb2222"),
                Some("bbbb2222"),
                "ssm-mirror",
            ),
            (None, None, None, "none"),
            (Some(" "), Some("unknown"), None, "none"),
        ];
        for (gh, mirror, want_sha, want_src) in CASES {
            let (sha, src) = resolve_desired_sha(*gh, *mirror);
            assert_eq!(sha, *want_sha, "github={gh:?} mirror={mirror:?}");
            assert_eq!(src, *want_src, "github={gh:?} mirror={mirror:?}");
        }
    }

    #[test]
    fn github_wins_when_it_answers_so_a_token_upgrades_this_automatically() {
        let (sha, src) = resolve_desired_sha(Some("aaaa1111"), Some("bbbb2222"));
        assert_eq!(sha, Some("aaaa1111"));
        assert_eq!(src, "github", "the mirror must never shadow a live answer");
    }

    #[test]
    fn the_mirror_answers_when_github_cannot() {
        // The 2026-09-02 shape: /tickvault/prod/operator/github-token does not
        // exist, so desired_sha() returns None and the comparison was dead.
        let (sha, src) = resolve_desired_sha(None, Some("bbbb2222"));
        assert_eq!(sha, Some("bbbb2222"));
        assert_eq!(src, "ssm-mirror");
    }

    #[test]
    fn neither_source_answering_is_reported_as_none_not_as_a_sha() {
        let (sha, src) = resolve_desired_sha(None, None);
        assert!(sha.is_none());
        assert_eq!(src, "none");
        // And the downstream comparison must still decline rather than guess.
        assert!(binary_mismatch_value(Some("aaaa1111"), sha).is_none());
    }

    #[test]
    fn placeholder_and_blank_values_are_not_treated_as_a_sha() {
        // "unknown" is the literal the portal writes for an unresolved sha.
        // Accepting it would compare a real binary against an absence and
        // publish a fabricated mismatch -- worse than publishing nothing.
        for junk in ["unknown", "UNKNOWN", "  ", ""] {
            let (sha, src) = resolve_desired_sha(None, Some(junk));
            assert!(sha.is_none(), "{junk:?} must not be accepted as a sha");
            assert_eq!(src, "none");
        }
        // Same rule on the GitHub leg, which then falls through to the mirror.
        let (sha, src) = resolve_desired_sha(Some("unknown"), Some("bbbb2222"));
        assert_eq!(sha, Some("bbbb2222"));
        assert_eq!(src, "ssm-mirror");
    }

    #[test]
    fn the_mirror_is_not_written_by_the_deploy_workflow() {
        // The trap this design exists to avoid: deploy-aws.yml writes
        // GITHUB_SHA, which IS main HEAD at deploy time. A mirror written
        // there would always equal binary-git-sha, so binary_is_stale would be
        // permanently false -- a signal that can never go red. The mirror must
        // come from a source that advances when MAIN advances.
        let src = include_str!("deploy_watchdog.rs");
        assert!(
            src.contains("postmerge-catchup.yml -- NOT by deploy-aws.yml"),
            "the const doc must keep naming the writer, or the next reader \
             will move the write into the deploy workflow and silently kill \
             the signal"
        );
        assert_ne!(
            DEFAULT_DESIRED_SHA_PARAM, DEFAULT_BINARY_SHA_PARAM,
            "the mirror and the on-box provenance must be different params"
        );
    }

    /// 2026-10-20: on a holiday-stop day a stale verdict must not dispatch
    /// (a dispatched deploy self-starts the box the holiday gate stopped).
    /// Source scan of `handle`: the marker read sits after the stale verdict
    /// and before the dispatch, and the skip returns without dispatching.
    #[test]
    fn handle_reads_the_holiday_stop_marker_before_it_dispatches() {
        let src = include_str!("deploy_watchdog.rs");
        let handle = src.find("pub async fn handle(").expect("handle is defined");
        let body = &src[handle..];
        let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
        let stale = body.find("if !stale {").expect("stale verdict branch");
        let marker = body
            .find("crate::start_watchdog::holiday_stop_is_today(")
            .expect("handle must read the holiday-stop marker");
        let skip = body
            .find("\"skipped\": \"holiday_stop\"")
            .expect("handle must report the holiday-stop skip");
        let dispatch = body
            .find("dispatch_deploy(")
            .expect("handle dispatches the redeploy");
        assert!(
            stale < marker && marker < skip && skip < dispatch,
            "the holiday-stop marker must be read and honoured before the dispatch"
        );
        assert!(body.contains("std::env::var(\"HOLIDAY_STOP_PARAM\")"));
    }

    #[test]
    fn test_binary_mismatch_none_when_desired_unknown() {
        assert_eq!(binary_mismatch_value(Some("deadbee"), None), None);
        assert_eq!(binary_mismatch_value(Some("deadbee"), Some("")), None);
        assert_eq!(
            binary_mismatch_value(Some("deadbee"), Some("unknown")),
            None
        );
    }

    // -------------------------------------------------------------------
    // binary_is_stale (pure) — retained legacy mismatch diagnostic
    // -------------------------------------------------------------------

    #[test]
    fn test_binary_is_stale_true_when_box_binary_differs_from_main() {
        // The exact 2026-07-09 case: box booted on the previous binary.
        assert!(binary_is_stale(
            Some(&full_sha("aaaaaaa")),
            Some(&full_sha("bbbbbbb"))
        ));
    }

    #[test]
    fn test_binary_is_stale_false_when_in_sync_short_vs_full() {
        let full = "deadbeefcafe0123456789abcdef012345678901";
        assert!(!binary_is_stale(Some(full), Some("deadbee")));
        assert!(!binary_is_stale(Some(full), Some(full)));
    }

    #[test]
    fn test_binary_is_stale_false_on_uncertainty_no_spurious_deploy() {
        // Never dispatch on uncertainty — an SSM/GitHub blip must not
        // force a redeploy.
        assert!(!binary_is_stale(None, Some("deadbee")));
        assert!(!binary_is_stale(Some(""), Some("deadbee")));
        assert!(!binary_is_stale(Some("unknown"), Some("deadbee")));
        assert!(!binary_is_stale(Some("deadbee"), None));
        assert!(!binary_is_stale(Some("deadbee"), Some("")));
        assert!(!binary_is_stale(Some("deadbee"), Some("unknown")));
    }

    #[test]
    fn token_cache_expires_at_ttl_and_never_reuses_empty_or_future_values() {
        let now = Instant::now();
        let cached = CachedParam {
            value: "synthetic-test-token".into(),
            cached_at: now,
        };
        assert_eq!(cached.usable_at(now), Some("synthetic-test-token"));
        assert!(
            cached
                .usable_at(now + TOKEN_CACHE_TTL - Duration::from_nanos(1))
                .is_some()
        );
        assert!(cached.usable_at(now + TOKEN_CACHE_TTL).is_none());
        assert!(
            cached
                .usable_at(now + TOKEN_CACHE_TTL + Duration::from_secs(1))
                .is_none()
        );
        let empty = CachedParam {
            value: String::new(),
            cached_at: now,
        };
        assert!(empty.usable_at(now).is_none());
        let future = CachedParam {
            value: "synthetic-test-token".into(),
            cached_at: now + Duration::from_secs(1),
        };
        assert!(future.usable_at(now).is_none());
    }

    #[test]
    fn github_body_limit_accepts_exact_cap_and_refuses_next_byte_without_growth() {
        let mut body = vec![b' '; GITHUB_RESPONSE_BYTES_MAX - 1];
        assert!(append_github_chunk(&mut body, b" "));
        assert_eq!(body.len(), GITHUB_RESPONSE_BYTES_MAX);
        assert!(!append_github_chunk(&mut body, b"x"));
        assert_eq!(body.len(), GITHUB_RESPONSE_BYTES_MAX);
        let mut empty = Vec::new();
        assert!(!append_github_chunk(
            &mut empty,
            &vec![b'x'; GITHUB_RESPONSE_BYTES_MAX + 1]
        ));
        assert!(empty.is_empty());
    }

    #[test]
    fn unconfirmed_dispatch_alert_never_claims_redeploy_was_triggered_or_complete() {
        let (subject, body) = verified_behind_alert(
            Some(&full_sha("aaaaaaa")),
            Some(&full_sha("bbbbbbb")),
            "ssm-mirror",
            "deploy-aws-after-close.yml",
            false,
        );
        assert!(subject.ends_with("request not confirmed"));
        assert!(body.contains("source: ssm-mirror"));
        assert!(body.contains(".yml was not confirmed."));
        assert!(body.contains("completion is not verified"));
        for misleading in ["old build", "main is at", "TRIGGERED", "No action needed"] {
            assert!(!body.contains(misleading), "{misleading}");
        }
    }

    #[test]
    fn accepted_dispatch_alert_reports_receipt_not_deployment_completion() {
        let (subject, body) = verified_behind_alert(
            Some(&full_sha("aaaaaaa")),
            Some(&full_sha("bbbbbbb")),
            "github",
            "deploy-aws-after-close.yml",
            true,
        );
        assert!(subject.ends_with("request accepted"));
        assert!(body.contains("was accepted"));
        assert!(body.contains("completion is not verified"));
    }

    #[tokio::test]
    async fn missing_token_cannot_dispatch_from_a_mirror_mismatch() {
        // If this short-circuit regresses, an explicit loopback-only proxy
        // prevents the fixture from reaching GitHub or ambient proxy services.
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let binary = full_sha("aaaaaaa");
        let desired = full_sha("bbbbbbb");
        let compared = verify_build_comparison(
            &http,
            "",
            "SJParthi/tickvault",
            Some(&binary),
            Some(&desired),
        )
        .await;
        assert_eq!(compared.relation, BuildRelation::Unknown);
        assert_eq!(compared.reason, "github_unavailable");
        assert_eq!(compared.http_status, Some(0));
        assert!(!compared.relation.permits_dispatch());
        let equal = verify_build_comparison(
            &http,
            "",
            "SJParthi/tickvault",
            Some(&binary),
            Some(&binary),
        )
        .await;
        assert_eq!(equal, BuildComparison::current());
    }

    #[test]
    fn dispatch_uses_ancestry_after_metric_and_before_existing_holiday_guard() {
        let source = include_str!("deploy_watchdog.rs");
        let handle = &source[source.find("pub async fn handle(").unwrap()..];
        let handle = &handle[..handle.find("\n}\n").unwrap()];
        let metric = handle.find("publish_binary_mismatch_metric(").unwrap();
        let compare = handle
            .find("let comparison = verify_build_comparison(")
            .unwrap();
        let authorization = handle
            .find("let binary_stale = comparison.relation.permits_dispatch()")
            .unwrap();
        let held = handle.find("if !stale {").unwrap();
        let holiday = handle
            .find("crate::start_watchdog::holiday_stop_is_today(")
            .unwrap();
        let dispatch = handle.find("let dispatched = dispatch_deploy(").unwrap();
        let notification = handle.find("verified_behind_alert(").unwrap();
        assert!(metric < compare && compare < authorization && authorization < held);
        assert!(held < holiday && holiday < dispatch && dispatch < notification);
        assert!(!handle.contains("github_stale ||"));
        assert!(!handle.contains("binary_is_stale("));
    }

    /// Local socket only, explicit no_proxy: fixtures never consume provider
    /// credentials or contact GitHub. The response is bounded by each caller.
    async fn local_github_response(
        raw: Vec<u8>,
    ) -> (reqwest::Response, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let read = socket.read(&mut request).await.unwrap();
            assert!(read > 0);
            // Oversize responses may be rejected before this write completes.
            let _ = socket.write_all(&raw).await;
            let _ = socket.shutdown().await;
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{address}/owned-test-fixture"))
            .send()
            .await
            .unwrap();
        (response, server)
    }

    #[tokio::test]
    async fn bounded_http_reader_accepts_json_and_empty_dispatch_receipt() {
        let payload = br#"{"status":"ahead","ahead_by":1}"#;
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        raw.extend_from_slice(payload);
        let (response, server) = local_github_response(raw).await;
        let (status, body) = read_github_response(response).await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({"status":"ahead","ahead_by":1}));
        server.await.unwrap();
        let (response, server) =
            local_github_response(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec())
                .await;
        assert_eq!(read_github_response(response).await, (204, json!({})));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bounded_http_reader_rejects_declared_and_chunked_oversize_responses() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            GITHUB_RESPONSE_BYTES_MAX + 1
        )
        .into_bytes();
        let (response, server) = local_github_response(raw).await;
        let (status, body) = read_github_response(response).await;
        assert_eq!(status, 0);
        assert_eq!(body["error"], "github response exceeds byte limit");
        server.await.unwrap();
        let mut raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        raw.extend_from_slice(format!("{:x}\r\n", GITHUB_RESPONSE_BYTES_MAX).as_bytes());
        raw.resize(raw.len() + GITHUB_RESPONSE_BYTES_MAX, b' ');
        raw.extend_from_slice(b"\r\n1\r\nx\r\n0\r\n\r\n");
        let (response, server) = local_github_response(raw).await;
        let (status, body) = read_github_response(response).await;
        assert_eq!(status, 0);
        assert_eq!(body["error"], "github response exceeds byte limit");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_json_or_truncated_http_body_cannot_authorize_dispatch() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{".to_vec();
        let (response, server) = local_github_response(raw).await;
        let (status, body) = read_github_response(response).await;
        let binary = full_sha("aaaaaaa");
        let desired = full_sha("bbbbbbb");
        let comparison = classify_comparison(&binary, &desired, status, &body);
        assert_eq!(comparison.relation, BuildRelation::Unknown);
        assert!(!comparison.relation.permits_dispatch());
        server.await.unwrap();
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{".to_vec();
        let (response, server) = local_github_response(raw).await;
        let (status, body) = read_github_response(response).await;
        assert_eq!(status, 0);
        assert_eq!(body["error"], "github response read failed");
        server.await.unwrap();
    }
}
