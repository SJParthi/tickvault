"""Telegram webhook Lambda — Z+ L1 DETECT layer.

Receives SNS messages from `tv-alerts` (CloudWatch alarms OR direct
`aws sns publish` from the deploy-aws workflow) and forwards them to
the operator's Telegram chat via the bot API.

Charter authority: operator-charter-forever.md §C row "100% alerting"
+ §F mandates Severity::Critical → Telegram. Without this Lambda the
entire L1 DETECT layer is email-only, which means the operator
cannot react in real time during market hours.

No pip deps — urllib3 + boto3 are pre-installed in the AWS Lambda
Python 3.12 runtime. Keeps the zip artifact tiny (< 5 KB).

Invocation: SNS -> Lambda (this) -> Telegram bot API.

Environment variables (set by Terraform):
  TELEGRAM_BOT_TOKEN_SSM_PARAM  — SSM path holding the bot token
  TELEGRAM_CHAT_ID_SSM_PARAM    — SSM path holding the chat ID
  LOG_LEVEL                     — INFO (default) / DEBUG / WARNING
"""

from __future__ import annotations

import json
import logging
import os
import time
import urllib.parse
import urllib.request
from typing import Any

LOG_LEVEL = os.environ.get("LOG_LEVEL", "INFO")
logger = logging.getLogger()
logger.setLevel(LOG_LEVEL)

TELEGRAM_BOT_TOKEN_SSM_PARAM = os.environ.get(
    "TELEGRAM_BOT_TOKEN_SSM_PARAM", "/tickvault/prod/telegram/bot-token"
)
TELEGRAM_CHAT_ID_SSM_PARAM = os.environ.get(
    "TELEGRAM_CHAT_ID_SSM_PARAM", "/tickvault/prod/telegram/chat-id"
)
TELEGRAM_API_BASE = "https://api.telegram.org"
TELEGRAM_TIMEOUT_SECONDS = 8

# Cached SSM reads — Lambda containers stay warm for ~15 min. Re-fetch
# only when the container is cold. Keeps the SSM bill at ~1 read per
# container init instead of per alarm.
_CACHED_TOKEN: str | None = None
_CACHED_CHAT_ID: str | None = None
_SSM_CLIENT = None  # Lazy-init on first SSM call so pure-function tests don't need boto3.


def _ssm_client() -> Any:
    """Return a cached boto3 SSM client (lazy import for testability)."""
    global _SSM_CLIENT
    if _SSM_CLIENT is None:
        import boto3  # noqa: PLC0415  — intentional lazy import

        _SSM_CLIENT = boto3.client("ssm")
    return _SSM_CLIENT


def _fetch_ssm_secret(parameter_name: str) -> str:
    """Fetch a SecureString parameter and return the decrypted value."""
    response = _ssm_client().get_parameter(Name=parameter_name, WithDecryption=True)
    return response["Parameter"]["Value"]


def _get_credentials() -> tuple[str, str]:
    """Return (bot_token, chat_id), caching across warm invocations."""
    global _CACHED_TOKEN, _CACHED_CHAT_ID
    if _CACHED_TOKEN is None:
        _CACHED_TOKEN = _fetch_ssm_secret(TELEGRAM_BOT_TOKEN_SSM_PARAM)
    if _CACHED_CHAT_ID is None:
        _CACHED_CHAT_ID = _fetch_ssm_secret(TELEGRAM_CHAT_ID_SSM_PARAM)
    return _CACHED_TOKEN, _CACHED_CHAT_ID


def _severity_emoji(subject: str, alarm_state: str | None) -> str:
    """Map alarm severity / subject to a leading emoji per charter §D rule 5+10."""
    subject_lower = (subject or "").lower()
    state = (alarm_state or "").upper()
    if "fail" in subject_lower or "critical" in subject_lower or state == "ALARM":
        return "🆘"
    if state == "INSUFFICIENT_DATA":
        return "⚠️"
    if state == "OK":
        return "✅"
    if "ok" in subject_lower or "deploy ok" in subject_lower:
        return "✅"
    return "🔔"


def _format_cloudwatch_alarm(alarm: dict[str, Any]) -> str:
    """Format a parsed CloudWatch alarm dict into a plain-English message."""
    name = alarm.get("AlarmName", "unknown-alarm")
    state = alarm.get("NewStateValue", "ALARM")
    reason = alarm.get("NewStateReason", "")
    region = alarm.get("Region", "")
    emoji = _severity_emoji(name, state)
    # Trim very long reasons so Telegram's 4096-char body cap is safe.
    if len(reason) > 2000:
        reason = reason[:2000] + " …(truncated)"
    return (
        f"{emoji} *{name}*\n"
        f"State: `{state}`\n"
        f"Region: `{region}`\n"
        f"Reason: {reason}"
    )


def _format_plain_sns(subject: str | None, message: str) -> str:
    """Format a non-CloudWatch SNS publish (e.g., from deploy-aws workflow)."""
    emoji = _severity_emoji(subject or "", None)
    if subject:
        return f"{emoji} *{subject}*\n{message}"
    return f"{emoji} {message}"


def _build_message(sns_record: dict[str, Any]) -> str:
    """Convert one SNS record's Message into a Telegram-ready string."""
    subject = sns_record.get("Subject")
    message = sns_record.get("Message", "")
    # CloudWatch alarms ship Message as a JSON string; everything else
    # ships it as plain text (e.g., aws sns publish --message "hi").
    try:
        parsed = json.loads(message)
        if isinstance(parsed, dict) and "AlarmName" in parsed:
            return _format_cloudwatch_alarm(parsed)
    except (TypeError, json.JSONDecodeError):
        pass
    return _format_plain_sns(subject, message)


def _post_to_telegram(token: str, chat_id: str, text: str) -> tuple[int, str]:
    """POST to the Telegram bot API. Returns (status_code, body_text)."""
    url = f"{TELEGRAM_API_BASE}/bot{token}/sendMessage"
    payload = urllib.parse.urlencode(
        {
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown",
            "disable_web_page_preview": "true",
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=payload,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=TELEGRAM_TIMEOUT_SECONDS) as resp:
        return resp.status, resp.read().decode("utf-8", errors="replace")


def lambda_handler(event: dict[str, Any], _context: Any) -> dict[str, Any]:
    """SNS-triggered entry point."""
    records = event.get("Records") or []
    if not records:
        logger.warning("No SNS Records in event — skipping")
        return {"sent": 0, "skipped": 1}

    try:
        token, chat_id = _get_credentials()
    except Exception:  # noqa: BLE001
        logger.exception("Failed to fetch Telegram credentials from SSM")
        # Re-raise so SNS marks the delivery failed and retries per its
        # default policy. Without retry the alert is lost forever.
        raise

    sent = 0
    failures: list[str] = []
    for record in records:
        sns = record.get("Sns") or {}
        try:
            text = _build_message(sns)
            status, body = _post_to_telegram(token, chat_id, text)
            if status >= 400:
                failures.append(f"http {status}: {body[:200]}")
                logger.error("Telegram POST returned %s: %s", status, body[:200])
            else:
                sent += 1
        except Exception as exc:  # noqa: BLE001
            failures.append(str(exc))
            logger.exception("Failed to relay one SNS record to Telegram")
        # Cheap rate-limit cushion. Telegram allows ~30 msg/sec per bot;
        # if 5+ alarms fire in the same SNS batch we don't want to flirt
        # with their throttle.
        time.sleep(0.05)

    return {"sent": sent, "failures": failures, "records": len(records)}
