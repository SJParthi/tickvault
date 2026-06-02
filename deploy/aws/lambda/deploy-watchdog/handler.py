"""Deploy watchdog Lambda — AWS-native safety-net for the post-merge auto-deploy.

The post-merge auto-deploy runs from `deploy-aws-after-close.yml`, a GitHub
Actions cron (08:45 + 15:31 IST Mon-Fri). GitHub-hosted cron is best-effort:
under platform load it can be delayed 10-30+ minutes or skipped entirely, which
would leave the box running STALE code with no signal. aws-autopilot mitigates
some of this, but it too runs on GitHub.

This Lambda is the belt-and-suspenders: it runs IN AWS (EventBridge -> Lambda),
so it covers GitHub-cron misses even when GitHub Actions is degraded. Two
EventBridge schedules fire it a few minutes AFTER the GitHub cron:

  * 08:50 IST (03:20 UTC Mon-Fri) — 5 min after the 08:45 pre-market cron.
  * 15:36 IST (10:06 UTC Mon-Fri) — 5 min after the 15:31 post-market cron.

On each fire it answers ONE question, using GitHub as the source of truth (the
same idempotency signal `deploy-aws-after-close.yml` itself uses):

    Is `main` HEAD already deployed?
      deployed = head_sha of the most recent SUCCESSFUL `deploy-aws.yml` run
      desired  = current `main` HEAD sha

  * desired == deployed  -> healthy, stay SILENT (no spam).
  * desired != deployed  -> the cron missed (or is late) -> trigger
    `deploy-aws-after-close.yml` via workflow_dispatch (itself idempotent +
    market-hours-guarded, so a double-fire with the cron is a safe no-op) AND
    publish a Severity::High Telegram so the operator learns the cron slipped.
  * either sha unknown (GitHub API blip / no deploy history) -> stay SILENT and
    log WARNING. The watchdog NEVER dispatches on uncertainty — a spurious
    deploy is worse than waiting for the next 5-minutes-later run. No false-OK:
    we only ever claim "deployed" by positive sha match, never by absence.

Environment variables (set by Terraform):
  GH_REPO                      — "SJParthi/tickvault"
  GH_DESIRED_REF               — branch to deploy from (default "main")
  GH_DEPLOY_WORKFLOW           — workflow file to read deployed sha from ("deploy-aws.yml")
  GH_DISPATCH_WORKFLOW         — workflow file to dispatch on staleness ("deploy-aws-after-close.yml")
  OPERATOR_GITHUB_TOKEN_PARAM  — SSM SecureString param holding a repo-scoped token
  ALERTS_TOPIC_ARN             — operator's tv_alerts SNS topic for Telegram
  LOG_LEVEL                    — INFO (default) / DEBUG / WARNING

No pip deps — boto3 (SSM + SNS) is pre-installed in the AWS Lambda Python 3.12
runtime; GitHub calls use urllib (same pattern as the operator-control Lambda).
"""

from __future__ import annotations

import json
import logging
import os
import urllib.error
import urllib.request
from typing import Any

# boto3 (SSM + SNS) is imported lazily inside the IO helpers so the pure
# decision logic (`is_stale`) is unit-testable without the AWS SDK installed.
# boto3 is always present in the AWS Lambda Python 3.12 runtime.

LOG_LEVEL = os.environ.get("LOG_LEVEL", "INFO")
logger = logging.getLogger()
logger.setLevel(LOG_LEVEL)

GH_REPO = os.environ.get("GH_REPO", "SJParthi/tickvault")
GH_DESIRED_REF = os.environ.get("GH_DESIRED_REF", "main")
GH_DEPLOY_WORKFLOW = os.environ.get("GH_DEPLOY_WORKFLOW", "deploy-aws.yml")
GH_DISPATCH_WORKFLOW = os.environ.get("GH_DISPATCH_WORKFLOW", "deploy-aws-after-close.yml")
_GH_TOKEN_PARAM = os.environ.get("OPERATOR_GITHUB_TOKEN_PARAM", "")
ALERTS_TOPIC_ARN = os.environ.get("ALERTS_TOPIC_ARN", "")

# --------------------------------------------------------------------------- #
# Pure decision logic (no IO — unit-tested in test_handler.py)
# --------------------------------------------------------------------------- #


def is_stale(desired_sha: str | None, deployed_sha: str | None) -> bool:
    """Return True iff a redeploy is needed.

    Stale ONLY when BOTH shas are known AND they differ. If either is unknown
    (empty/None — GitHub API blip, or no successful deploy run yet) we return
    False: the watchdog must never dispatch on uncertainty (a spurious deploy is
    worse than waiting for the next 5-minutes-later run). This is the no-false-OK
    contract inverted — we never claim "deployed" by absence, and we never
    "fix" what we cannot positively confirm is broken.
    """
    if not desired_sha or not deployed_sha:
        return False
    return desired_sha.strip() != deployed_sha.strip()


# --------------------------------------------------------------------------- #
# IO helpers
# --------------------------------------------------------------------------- #

_param_cache: dict[str, str] = {}


def _cached_param(name: str) -> str:
    if not name:
        return ""
    if name in _param_cache:
        return _param_cache[name]
    try:
        import boto3

        ssm = boto3.client("ssm")
        val = ssm.get_parameter(Name=name, WithDecryption=True)["Parameter"]["Value"]
        _param_cache[name] = val
        return val
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("ssm get_parameter failed for %s: %s", name, exc)
        return ""


def _gh(method: str, path: str, body: dict | None = None) -> tuple[int, object]:
    """Minimal GitHub REST call using urllib (no deps). Returns (status, json)."""
    token = _cached_param(_GH_TOKEN_PARAM)
    if not token:
        return 0, {"error": "github token not configured"}
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request("https://api.github.com" + path, data=data, method=method)
    req.add_header("authorization", "Bearer " + token)
    req.add_header("accept", "application/vnd.github+json")
    req.add_header("x-github-api-version", "2022-11-28")
    req.add_header("user-agent", "tickvault-deploy-watchdog")
    if data is not None:
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:  # noqa: S310 — fixed host
            txt = r.read().decode() or "{}"
            return r.status, (json.loads(txt) if txt.strip() else {})
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode() or "{}")
        except Exception:  # noqa: BLE001
            return e.code, {"error": "github http error"}
    except Exception:  # noqa: BLE001
        return 0, {"error": "github request failed"}


def _desired_sha() -> str | None:
    """Current HEAD sha of the desired ref (main)."""
    status, body = _gh("GET", f"/repos/{GH_REPO}/commits/{GH_DESIRED_REF}")
    if status == 200 and isinstance(body, dict):
        sha = body.get("sha")
        return sha if isinstance(sha, str) else None
    logger.warning("could not fetch desired sha (status=%s)", status)
    return None


def _deployed_sha() -> str | None:
    """head_sha of the most recent SUCCESSFUL deploy workflow run on the ref."""
    path = (
        f"/repos/{GH_REPO}/actions/workflows/{GH_DEPLOY_WORKFLOW}/runs"
        f"?branch={GH_DESIRED_REF}&status=success&per_page=1"
    )
    status, body = _gh("GET", path)
    if status == 200 and isinstance(body, dict):
        runs = body.get("workflow_runs", [])
        if isinstance(runs, list) and runs:
            sha = runs[0].get("head_sha")
            return sha if isinstance(sha, str) else None
        logger.info("no successful %s run found yet on %s", GH_DEPLOY_WORKFLOW, GH_DESIRED_REF)
        return None
    logger.warning("could not fetch deployed sha (status=%s)", status)
    return None


def _dispatch_deploy() -> bool:
    """Trigger the (idempotent, market-hours-guarded) auto-deploy workflow."""
    status, _ = _gh(
        "POST",
        f"/repos/{GH_REPO}/actions/workflows/{GH_DISPATCH_WORKFLOW}/dispatches",
        {"ref": GH_DESIRED_REF},
    )
    ok = status in (201, 204)
    if not ok:
        logger.error("workflow_dispatch of %s failed (status=%s)", GH_DISPATCH_WORKFLOW, status)
    return ok


def _publish(subject: str, message: str) -> None:
    if not ALERTS_TOPIC_ARN:
        logger.warning("ALERTS_TOPIC_ARN unset — cannot publish: %s", subject)
        return
    try:
        import boto3

        boto3.client("sns").publish(TopicArn=ALERTS_TOPIC_ARN, Subject=subject, Message=message)
    except Exception as exc:  # noqa: BLE001 — watchdog must never crash
        logger.error("sns publish failed: %s", exc)


# --------------------------------------------------------------------------- #
# Entry point
# --------------------------------------------------------------------------- #


def lambda_handler(event: dict[str, Any], _context: Any = None) -> dict[str, Any]:
    """Entry point. Idempotent — safe to run on every 5-minutes-after schedule."""
    window = (event or {}).get("window", "unknown")
    desired = _desired_sha()
    deployed = _deployed_sha()
    stale = is_stale(desired, deployed)
    logger.info(
        "deploy-watchdog window=%s desired=%s deployed=%s stale=%s",
        window,
        (desired or "")[:8],
        (deployed or "")[:8],
        stale,
    )

    if not stale:
        # Healthy OR uncertain — stay silent (no spam, no spurious deploy).
        return {"window": window, "stale": False, "dispatched": False}

    dispatched = _dispatch_deploy()
    _publish(
        "Deploy watchdog: GitHub cron missed — redeploy triggered",
        (
            f"⚠️ The {window} auto-deploy did not land: main is at {(desired or '')[:8]} but "
            f"the last successful deploy was {(deployed or '')[:8]}. The AWS watchdog "
            f"{'TRIGGERED' if dispatched else 'FAILED to trigger'} the redeploy "
            f"({GH_DISPATCH_WORKFLOW}). It is idempotent + market-hours-guarded, so it is safe. "
            f"{'No action needed — confirm the deploy run goes green.' if dispatched else 'ACTION: dispatch the deploy manually.'}"
        ),
    )
    logger.warning(
        "stale deploy detected (window=%s) — dispatched=%s", window, dispatched
    )
    return {"window": window, "stale": True, "dispatched": dispatched}
