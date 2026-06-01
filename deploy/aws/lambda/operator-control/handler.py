"""Operator-control Lambda — the action backend for the single-page operator
console (start / stop / restart-app / restart-questdb / status).

WHY THIS EXISTS
---------------
The operator wants ONE place (their own console URL + Telegram) to both VIEW
and CONTROL the trading box, without the AWS console or the GitHub UI. Grafana
covers VIEW; this Lambda covers CONTROL. A button on the console page sends an
authenticated POST to this Lambda's Function URL; the Lambda performs the
scoped action against the ONE EC2 instance.

SECURITY MODEL (deliberately strict — this can stop a live trading box)
-----------------------------------------------------------------------
* Function URL auth is NONE at the AWS layer, but EVERY request must carry
  `Authorization: Bearer <secret>` matching OPERATOR_CONTROL_SECRET (injected
  from SSM SecureString at deploy). Constant-time compare. Missing/wrong → 401.
* The Lambda's IAM role is scoped to EXACTLY this instance: ec2 Start/Stop/
  Reboot on the one instance ARN, ssm:SendCommand on the one instance +
  AWS-RunShellScript only. It can do NOTHING else.
* Destructive actions (stop / reboot / restart-app / stop-app) are blocked
  during market hours (09:15–15:30 IST Mon–Fri) unless the body sets
  {"force": true} — mirrors aws-control.yml's guard.
* Every invocation is CloudWatch-logged (audit) and the action is echoed back.

This Lambda intentionally does NOT implement `deploy` — deploy needs GitHub
`workflow_dispatch` (a PAT we don't store). Deploy stays on the auto-on-merge
path; the console links to it rather than holding a GitHub credential.
"""

from __future__ import annotations

import datetime
import hmac
import json
import os
import time

import boto3

REGION = os.environ["AWS_REGION"]
INSTANCE_ID = os.environ["TV_INSTANCE_ID"]
# Name of the SSM SecureString holding the bearer secret. We read the secret
# at cold start via ssm:GetParameter (decrypted) — it is NEVER placed in the
# Lambda env or Terraform state, so it cannot leak via GetFunctionConfiguration.
_SECRET_PARAM = os.environ.get("OPERATOR_CONTROL_SECRET_PARAM", "")

_ec2 = boto3.client("ec2", region_name=REGION)
_ssm = boto3.client("ssm", region_name=REGION)


def _load_control_secret() -> str:
    """Read the bearer secret from SSM SecureString (decrypted)."""
    if not _SECRET_PARAM:
        return ""
    try:
        return _ssm.get_parameter(Name=_SECRET_PARAM, WithDecryption=True)["Parameter"]["Value"]
    except Exception:  # noqa: BLE001 — no secret => deny all (fail closed)
        return ""


# 60s TTL cache: avoids an SSM read on every call, but picks up a ROTATED
# secret within 60s (review finding 5) — not stuck until the next cold start.
_SECRET_TTL_SECS = 60.0
_secret_cache = {"value": "", "ts": 0.0}


def _control_secret() -> str:
    now = time.monotonic()
    if not _secret_cache["value"] or (now - _secret_cache["ts"]) > _SECRET_TTL_SECS:
        _secret_cache["value"] = _load_control_secret()
        _secret_cache["ts"] = now
    return _secret_cache["value"]

# Actions that are blocked during market hours unless force=true.
_DESTRUCTIVE = {"stop", "reboot", "restart-app", "stop-app"}

# IST market window (seconds since IST midnight): 09:15:00 .. 15:30:00.
_MKT_OPEN_SECS = 9 * 3600 + 15 * 60
_MKT_CLOSE_SECS = 15 * 3600 + 30 * 60
_IST_OFFSET_SECS = 19800  # +05:30


def _is_market_hours(now_utc: datetime.datetime) -> bool:
    """True during NSE market hours (Mon–Fri 09:15–15:30 IST)."""
    ist = now_utc + datetime.timedelta(seconds=_IST_OFFSET_SECS)
    if ist.weekday() >= 5:  # Sat/Sun
        return False
    sod = ist.hour * 3600 + ist.minute * 60 + ist.second
    return _MKT_OPEN_SECS <= sod < _MKT_CLOSE_SECS


def _authorized(headers: dict) -> bool:
    """Constant-time bearer check. Header keys are lowercased by Function URL.
    Required format is exactly `Authorization: Bearer <secret>` (one space,
    case-sensitive scheme) — document this on the console page to avoid
    emergency lockout confusion (review finding 4)."""
    secret = _control_secret()
    if not secret:
        return False  # fail closed: no secret configured => deny all
    auth = (headers or {}).get("authorization", "")
    if not auth.startswith("Bearer "):
        return False
    presented = auth[len("Bearer ") :]
    return hmac.compare_digest(presented, secret)


def _ssm_shell(commands: list[str]) -> str:
    """Run a shell command on the instance via SSM, return the command id."""
    resp = _ssm.send_command(
        InstanceIds=[INSTANCE_ID],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": commands},
    )
    return resp["Command"]["CommandId"]


def _resp(status: int, body: dict) -> dict:
    return {
        "statusCode": status,
        "headers": {"content-type": "application/json"},
        "body": json.dumps(body),
    }


def lambda_handler(event, _context):
    headers = event.get("headers") or {}
    if not _authorized(headers):
        return _resp(401, {"error": "unauthorized"})

    # Body may be a JSON string (Function URL) or already-parsed.
    raw = event.get("body") or "{}"
    try:
        payload = raw if isinstance(raw, dict) else json.loads(raw)
    except json.JSONDecodeError:
        return _resp(400, {"error": "invalid JSON body"})

    action = str(payload.get("action", "")).strip()
    force = bool(payload.get("force", False))

    if action in _DESTRUCTIVE and _is_market_hours(datetime.datetime.utcnow()) and not force:
        return _resp(
            409,
            {
                "error": "blocked during market hours (09:15-15:30 IST). "
                'Re-send with {"force": true} to override.',
                "action": action,
            },
        )

    try:
        if action == "start":
            _ec2.start_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action, "instance": INSTANCE_ID})
        if action == "stop":
            _ec2.stop_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action, "instance": INSTANCE_ID})
        if action == "reboot":
            _ec2.reboot_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action, "instance": INSTANCE_ID})
        if action == "restart-app":
            cid = _ssm_shell(["systemctl restart tickvault"])
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "stop-app":
            cid = _ssm_shell(
                ["systemctl stop tickvault || true", "systemctl disable tickvault || true"]
            )
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "restart-questdb":
            cid = _ssm_shell(
                [
                    "cd /opt/tickvault/repo/deploy/docker && docker compose restart questdb",
                ]
            )
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "status":
            state = _ec2.describe_instances(InstanceIds=[INSTANCE_ID])
            inst = state["Reservations"][0]["Instances"][0]
            cid = _ssm_shell(
                [
                    "systemctl is-active tickvault || true",
                    "curl -fsS 'http://127.0.0.1:9000/exec?query=SELECT%20count(*)%20FROM%20ticks' || true",
                ]
            )
            return _resp(
                200,
                {
                    "ok": True,
                    "action": "status",
                    "instance_state": inst["State"]["Name"],
                    "status_command_id": cid,
                },
            )
        return _resp(400, {"error": f"unknown action: {action!r}"})
    except Exception as exc:  # noqa: BLE001
        # Review finding 1: do NOT echo the raw AWS exception to the caller —
        # it can leak ARNs / account id / topology. Log it for the operator;
        # return a generic message.
        print(f"operator-control action={action!r} failed: {exc!r}")  # noqa: T201 — CloudWatch
        return _resp(500, {"error": "action failed", "action": action})
