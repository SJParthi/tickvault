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
//! On each fire it answers ONE question, using GitHub as the source of
//! truth (the same idempotency signal `deploy-aws-after-close.yml` itself
//! uses): is `main` HEAD already deployed?
//!
//! * desired == deployed  -> healthy, stay SILENT (no spam).
//! * desired != deployed  -> the cron missed (or is late) -> trigger
//!   `deploy-aws-after-close.yml` via workflow_dispatch (itself idempotent
//!   + market-hours-guarded, so a double-fire with the cron is a safe
//!   no-op) AND publish a Severity::High Telegram.
//! * either sha unknown (GitHub API blip / no deploy history) -> stay
//!   SILENT and log WARNING. The watchdog NEVER dispatches on uncertainty
//!   — a spurious deploy is worse than waiting for the next
//!   5-minutes-later run. No false-OK: we only ever claim "deployed" by
//!   positive sha match, never by absence.
//!
//! B9 deploy provenance rides every fire: the on-box binary sha (SSM
//! `/tickvault/<env>/deploy/binary-git-sha`, written by deploy-aws.yml
//! post-swap) is compared against main HEAD and published as the
//! `tv_binary_main_sha_mismatch` CloudWatch metric (the
//! `tv-<env>-binary-sha-stale` alarm's input), and `binary_is_stale` is
//! the GROUND-TRUTH box-start staleness signal (a green off-hours NO-OP
//! skip run can fool the GitHub signal but never the SSM param — the
//! 2026-07-09 outage class).
//!
//! Behavior-parity ledger (deliberate deviations from the python):
//! * env vars are read PER INVOCATION (python read them at module import;
//!   Lambda cold-starts per config change so both see current values).
//! * `tracing` structured logs replace the python `logging` calls 1:1
//!   (same levels, same decision points).
//! * GitHub REST uses `reqwest` (10s timeout, fixed api.github.com host)
//!   instead of urllib — same status/body tuple semantics: transport
//!   failure -> (0, {"error": "github request failed"}), unparsable body
//!   on an HTTP response -> {"error": "github http error"}.
//! * the SSM token cache is a process-`static` Mutex map (the python
//!   module-global `_param_cache`) — warm containers skip the re-read;
//!   the BINARY sha param is deliberately NOT cached (python parity: it
//!   changes on every deploy).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use tracing::{error, info, warn};

/// Env defaults — python parity (`os.environ.get(...)` fallbacks).
const DEFAULT_GH_REPO: &str = "SJParthi/tickvault";
const DEFAULT_GH_DESIRED_REF: &str = "main";
const DEFAULT_GH_DEPLOY_WORKFLOW: &str = "deploy-aws.yml";
const DEFAULT_GH_DISPATCH_WORKFLOW: &str = "deploy-aws-after-close.yml";
const DEFAULT_BINARY_SHA_PARAM: &str = "/tickvault/prod/deploy/binary-git-sha";

/// B9 deploy provenance metric identity — python parity
/// (`_MISMATCH_METRIC_*`). PutMetricData is called DIRECTLY (bypasses the
/// Prometheus→EMF allowlist in user-data.sh.tftpl by design — this metric
/// originates in the Lambda, not the app).
const MISMATCH_METRIC_NAMESPACE: &str = "Tickvault/Prod";
const MISMATCH_METRIC_NAME: &str = "tv_binary_main_sha_mismatch";
const MISMATCH_METRIC_HOST: &str = "tickvault-prod";

const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_TIMEOUT_SECONDS: u64 = 10;

// ------------------------------------------------------------------------
// Pure decision logic (no IO — unit-tested below, python tests ported 1:1)
// ------------------------------------------------------------------------

/// Return true iff a redeploy is needed.
///
/// Stale ONLY when BOTH shas are known AND they differ. If either is
/// unknown (empty/None — GitHub API blip, or no successful deploy run
/// yet) we return false: the watchdog must never dispatch on uncertainty
/// (a spurious deploy is worse than waiting for the next 5-minutes-later
/// run). This is the no-false-OK contract inverted — we never claim
/// "deployed" by absence, and we never "fix" what we cannot positively
/// confirm is broken. Python parity: `if not desired_sha or not
/// deployed_sha: return False`.
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

/// Return true iff the ON-BOX running binary is provably older than main
/// HEAD.
///
/// This is the GROUND-TRUTH staleness signal for the box-start
/// (instance-start) firing: `binary_sha` is the SSM
/// /tickvault/<env>/deploy/binary-git-sha param, which deploy-aws.yml
/// writes ONLY after a verified-healthy binary swap. Unlike the GitHub
/// "last successful deploy-aws run" head_sha (see `is_stale`), a green
/// off-hours NO-OP skip run or a market-hours-blocked run can NEVER
/// advance this param — so a box that auto-started on the previous binary
/// is caught here even when a misleading skip run makes the GitHub signal
/// look "deployed" (the exact 2026-07-09 boot-critical-fix outage class).
///
/// No-false-OK: reuses `binary_mismatch_value`, which returns None when
/// EITHER sha is unknown/empty/"unknown" — so an SSM/GitHub blip is
/// treated as "not stale" and never triggers a spurious redeploy.
pub fn binary_is_stale(binary_sha: Option<&str>, desired_sha: Option<&str>) -> bool {
    binary_mismatch_value(binary_sha, desired_sha) == Some(1.0)
}

// ------------------------------------------------------------------------
// IO helpers (thin wrappers — exercised in the live deploy, python parity)
// ------------------------------------------------------------------------

/// Warm-container SSM param cache — python parity (`_param_cache`
/// module-global). Only the GitHub TOKEN goes through this; the binary
/// sha param is read fresh every fire.
static PARAM_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// SSM SecureString read with warm-container caching. Any failure returns
/// "" (the watchdog must never crash on the token read — python parity:
/// the blanket `except` in `_cached_param`).
async fn cached_param(ssm: &aws_sdk_ssm::Client, name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    {
        let guard = PARAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref().and_then(|c| c.get(name)) {
            return cached.clone();
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
            let val = resp
                .parameter()
                .and_then(|p| p.value())
                .unwrap_or("")
                .to_string();
            let mut guard = PARAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .get_or_insert_with(HashMap::new)
                .insert(name.to_string(), val.clone());
            val
        }
        Err(e) => {
            // The param NAME is safe to log; the value never is.
            error!(param = name, error = %e, "ssm get_parameter failed");
            String::new()
        }
    }
}

/// Minimal GitHub REST call. Returns (status, json) — python `_gh` parity:
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
        Ok(resp) => {
            let status = resp.status().as_u16();
            let txt = resp.text().await.unwrap_or_default();
            let parsed = if txt.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&txt).unwrap_or_else(|_| json!({"error": "github http error"}))
            };
            (status, parsed)
        }
        // Token-redaction hardening: never log the raw reqwest error here
        // (its Display can embed the URL); the fixed literal is enough.
        Err(_) => (0, json!({"error": "github request failed"})),
    }
}

/// Current HEAD sha of the desired ref (main). Python `_desired_sha`.
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
        return body.get("sha").and_then(Value::as_str).map(str::to_string);
    }
    warn!(status, "could not fetch desired sha");
    None
}

/// head_sha of the most recent SUCCESSFUL deploy workflow run on the ref.
/// Python `_deployed_sha`.
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
/// Python `_dispatch_deploy` — ok iff GitHub answers 201/204.
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
            workflow = dispatch_workflow,
            status, "workflow_dispatch failed"
        );
    }
    ok
}

/// Deployed-binary git SHA from SSM (B9 deploy provenance). Read fresh on
/// every invocation (NOT via the token cache — the value changes on every
/// deploy and Lambda containers persist across invocations). Python
/// `_binary_sha`.
async fn binary_sha(ssm: &aws_sdk_ssm::Client, param: &str) -> Option<String> {
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
            if val.is_empty() { None } else { Some(val) }
        }
        Err(e) => {
            warn!(param, error = %e, "could not read binary sha param");
            None
        }
    }
}

/// B9 deploy provenance: publish tv_binary_main_sha_mismatch so the
/// tv-<env>-binary-sha-stale alarm (Minimum >= 1 over 24h) can detect a
/// running binary that lags main HEAD for a full day. Wrapped so the
/// watchdog's core stale-deploy dispatch job NEVER breaks on metric IO
/// (python parity: the blanket `except` + error log).
async fn publish_binary_mismatch_metric(
    cw: &aws_sdk_cloudwatch::Client,
    binary: Option<&str>,
    desired: Option<&str>,
) {
    let Some(value) = binary_mismatch_value(binary, desired) else {
        info!("binary/main mismatch metric skipped — a sha is unknown");
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
        Err(e) => error!(metric = MISMATCH_METRIC_NAME, error = %e, "put_metric_data failed"),
    }
}

/// SNS publish — never fatal (python `_publish` parity: unset topic warns,
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
        error!(error = %e, "sns publish failed");
    }
}

// ------------------------------------------------------------------------
// Entry point
// ------------------------------------------------------------------------

/// First 8 chars of an optional sha — python parity `(sha or "")[:8]`.
fn short8(sha: Option<&str>) -> String {
    sha.unwrap_or("").chars().take(8).collect()
}

/// Entry point. Idempotent — safe to run on every 5-minutes-after
/// schedule. Python `lambda_handler` parity, same result JSON shape.
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

    let config = crate::clients::sdk_config().await;
    let ssm = crate::clients::ssm(&config);
    let cw = crate::clients::cloudwatch(&config);
    let sns = crate::clients::sns(&config);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(GITHUB_TIMEOUT_SECONDS))
        .build()?;

    let token = cached_param(&ssm, &token_param).await;
    let desired = desired_sha(&http, &token, &gh_repo, &gh_desired_ref).await;
    // Read the on-box binary provenance sha ONCE per fire — used BOTH for
    // the 24h mismatch metric and the ground-truth box-binary staleness
    // check below.
    let binary = binary_sha(&ssm, &binary_sha_param).await;
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

    // Two INDEPENDENT staleness signals, OR-ed so we never REDUCE
    // detection:
    // * github_stale — main HEAD != head_sha of the last SUCCESSFUL
    //   deploy-aws run. Best-effort: a green off-hours no-op SKIP run (box
    //   down) concludes `success` with head_sha = the merged commit, so
    //   this signal can be FOOLED into "deployed" while the box actually
    //   booted stale (postmerge-catchup.yml can create such skip runs).
    // * binary_stale — the ON-BOX running binary's provenance sha != main
    //   HEAD. GROUND TRUTH: the SSM param only advances on a verified
    //   swap, so a skip run can never fool it (2026-07-09 outage class).
    let github_stale = is_stale(desired.as_deref(), deployed.as_deref());
    let binary_stale = binary_is_stale(binary.as_deref(), desired.as_deref());
    let stale = github_stale || binary_stale;
    info!(
        window,
        desired = %short8(desired.as_deref()),
        deployed = %short8(deployed.as_deref()),
        binary = %short8(binary.as_deref()),
        github_stale,
        binary_stale,
        stale,
        "deploy-watchdog verdict"
    );

    if !stale {
        // Healthy OR uncertain — stay silent (no spam, no spurious deploy).
        return Ok(json!({
            "window": window,
            "stale": false,
            "github_stale": github_stale,
            "binary_stale": binary_stale,
            "dispatched": false,
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
    // Name the box-binary case explicitly so the operator knows the box is
    // RUNNING old code right now (vs. just a late deploy pipeline).
    let (subject, detail) = if binary_stale {
        (
            "Deploy watchdog: box booted on STALE code — redeploy triggered".to_string(),
            format!(
                "⚠️ The box is RUNNING an old build: its binary is {} but main is at {}",
                short8(binary.as_deref()),
                short8(desired.as_deref())
            ),
        )
    } else {
        (
            "Deploy watchdog: GitHub cron missed — redeploy triggered".to_string(),
            format!(
                "⚠️ The {window} auto-deploy did not land: main is at {} but the last successful deploy was {}",
                short8(desired.as_deref()),
                short8(deployed.as_deref())
            ),
        )
    };
    let message = format!(
        "{detail}. The AWS watchdog {} the redeploy ({gh_dispatch_workflow}). \
         It is idempotent + market-hours-guarded, so it is safe. {}",
        if dispatched {
            "TRIGGERED"
        } else {
            "FAILED to trigger"
        },
        if dispatched {
            "No action needed — confirm the deploy run goes green."
        } else {
            "ACTION: dispatch the deploy manually."
        }
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
        "dispatched": dispatched,
    }))
}

// ------------------------------------------------------------------------
// Tests — the python test_handler.py suite ported 1:1 (same names). The
// decision contract that must never regress: dispatch ONLY when both shas
// are known AND differ; NEVER dispatch on uncertainty; whitespace-
// insensitive equality.
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
    fn test_binary_mismatch_none_when_desired_unknown() {
        assert_eq!(binary_mismatch_value(Some("deadbee"), None), None);
        assert_eq!(binary_mismatch_value(Some("deadbee"), Some("")), None);
        assert_eq!(
            binary_mismatch_value(Some("deadbee"), Some("unknown")),
            None
        );
    }

    // -------------------------------------------------------------------
    // binary_is_stale (pure) — the ground-truth box-start dispatch signal
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
}
