"""Telegram webhook Lambda — Z+ L1 DETECT layer.

Receives SNS messages from `tv-alerts` (CloudWatch alarms OR direct
`aws sns publish` from the deploy-aws workflow) and forwards them to
the operator's Telegram chat via the bot API.

Charter authority: operator-charter-forever.md §C row "100% alerting"
+ §D (the 10 Telegram commandments) + §F mandates Severity::Critical
→ Telegram. Without this Lambda the entire L1 DETECT layer is
email-only, which means the operator cannot react in real time during
market hours.

House style (2026-07-07 Telegram UX overhaul, judge final contract):
every CloudWatch alarm renders as `{emoji} {plain-English line}` +
an IST 12-hour timestamp. The raw CloudWatch `NewStateReason`
("Threshold Crossed: 1 datapoint ...") NEVER reaches Telegram — it is
print()-logged to CloudWatch Logs only, for forensics. ALARM+OK pairs
inside one SNS batch fold to a single recovered line; lone OK flips
fold into ONE recovered line; ALARM records are NEVER folded,
digested, or suppressed. Messages are sent as plain text (no
parse_mode) so an alarm name containing '*' or '_' can never trigger
a silent Markdown-parse 400 drop.

No pip deps — urllib3 + boto3 are pre-installed in the AWS Lambda
Python 3.12 runtime. Keeps the zip artifact tiny (< 10 KB).

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
import re
import time
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
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

# Warm-container duplicate-OK guard (judge contract, robustness graft):
# suppress a repeat SAME-state OK for the same alarm within this window.
# ALARM-state records are NEVER consulted against this cache — a dropped
# 🆘 is unacceptable; a duplicate ✅ after a cold start is accepted.
_LAST_SENT: dict[str, tuple[str, float]] = {}
OK_REPEAT_SUPPRESS_SECS = 300

_IST = timezone(timedelta(hours=5, minutes=30))

# Strips the `tv-<env>-` prefix off an alarm name so the phrase table is
# environment-agnostic (tv-prod-cpu-high-5min and tv-staging-cpu-high-5min
# map to the same plain-English line).
_ENV_PREFIX_RE = re.compile(r"^tv-[a-z0-9]+-")

# Known alarm → auto-driver plain English (charter §D: no library names,
# no file paths, no jargon). Keys are alarm names AFTER the tv-<env>-
# prefix strip. Unknown names fall back to a humanized form — never a
# KeyError, never raw JSON.
#
# Broker/host tag convention (operator directive 2026-07-14): a phrase
# scoped to exactly one broker leads with "🔷 DHAN:" (or "🟢 GROWW:");
# a phrase about the whole box/process leads with "🖥️ HOST:" where the
# subject would otherwise be ambiguous. The OK flip reuses the phrase
# verbatim, so recoveries carry the same tag automatically.
ALARM_PHRASES: dict[str, str] = {
    "aggregator-no-seals": "Candle building has stopped during market hours",
    "binary-sha-stale": "The running app is more than a day behind the latest approved code",
    "boot-heartbeat-missing": "The app did not start on time this morning",
    "budget-killswitch-errors": "The cost kill-switch helper is failing",
    "clock-skew-high": "The server clock has drifted too far",
    "cpu-high-5min": "Server CPU has been very high for 5 minutes",
    "disk-used-high": "Server disk is almost full",
    "disk-watcher-respawn": "The disk monitor keeps restarting",
    "dlq-ticks": "Some market data overflowed into the emergency backup",
    "ebs-write-latency-high": "Disk writes have become very slow",
    "eventbridge-dlq-depth": "Scheduled cloud tasks are failing and piling up",
    "instance-status-failed": "The cloud server is failing its health checks",
    "late-tick-after-boundary": "Market data is arriving too late to build candles",
    "logs-ingestion-runaway": "Log volume is growing abnormally fast",
    "market-hours-liveness-missing": "🖥️ HOST: the app has gone silent during market hours",
    "mem-used-high": "Server memory is almost full",
    "network-out-runaway": "Outbound network traffic is abnormally high",
    "operator-control-errors": "The operator control page is failing",
    "order-update-ws-inactive": "Order confirmations feed has gone quiet",
    "orders-rejected": "Orders are being rejected",
    "questdb-console-front-errors": "The database console page is failing",
    "questdb-disconnected": "The database has been unreachable for too long",
    "realtime-guarantee-critical": "Overall system health has dropped to critical",
    "spill-dropped": "Live market data overflowed the safety buffer",
    "system-status-failed": "The cloud hardware is failing its health checks",
    "telegram-webhook-errors": "The Telegram alert relay itself is failing",
    "tick-gap-instruments-silent": "🔷 DHAN: some instruments have stopped sending prices",
    "ticks-dropped": "Live market data is being lost",
    "token-remaining-low": "🔷 DHAN: access token expires soon — spot-1m + option-chain pulls will stop",
    "ws-failed-connections": "🔷 DHAN: the live market data connection keeps failing",
    "ws-frame-dropped-no-wal": "Live market data arrived but could not be saved",
    "ws-pool-all-dead": "🔷 DHAN: ALL live market data connections are down",
    "ws-reconnect-gap-high": "🔷 DHAN: the live market data feed is taking too long to reconnect",
}

_GENERIC_SAFE_LINE = "🔔 Alert received — details are in the server log"


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


def _ist_12h(state_change_time: str) -> str:
    """Render a CloudWatch StateChangeTime as an IST 12-hour clock string.

    Accepts the CloudWatch shapes ("2026-07-07T04:31:12.345+0000",
    "...Z", with/without fractional seconds). Malformed / missing input
    falls back to the invocation time — the timestamp line degrades,
    never crashes (fail-open per contract).
    """
    parsed: datetime | None = None
    raw = str(state_change_time or "").strip()
    if raw:
        try:
            parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        except ValueError:
            for fmt in ("%Y-%m-%dT%H:%M:%S.%f%z", "%Y-%m-%dT%H:%M:%S%z"):
                try:
                    parsed = datetime.strptime(raw, fmt)
                    break
                except ValueError:
                    continue
    if parsed is None:
        parsed = datetime.now(timezone.utc)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    try:
        ist = parsed.astimezone(_IST)
    except (OverflowError, OSError, ValueError):
        # Edge-dated inputs (year 0001 / 9999) parse fine but overflow on
        # the tz conversion — degrade to invocation time, never crash.
        ist = datetime.now(timezone.utc).astimezone(_IST)
    hour = ist.hour % 12 or 12
    ampm = "AM" if ist.hour < 12 else "PM"
    return f"{hour}:{ist.minute:02d} {ampm}"


def _alarm_phrase(alarm_name: str) -> str:
    """Map an alarm name to plain English; humanize unknown names (fail-open)."""
    key = _ENV_PREFIX_RE.sub("", str(alarm_name or "").strip())
    phrase = ALARM_PHRASES.get(key)
    if phrase:
        return phrase
    words = key.replace("-", " ").replace("_", " ").strip()
    if not words:
        return "A cloud alarm changed state"
    return words[0].upper() + words[1:]


def _house_line(alarm: dict[str, Any]) -> str:
    """Format one CloudWatch alarm dict into the house-style Telegram text.

    `{emoji} {plain-English line}` + newline + `{IST 12-hour} IST`.
    The raw NewStateReason NEVER enters this string.
    """
    name = str(alarm.get("AlarmName") or "unknown-alarm")
    state = str(alarm.get("NewStateValue") or "ALARM").upper()
    phrase = _alarm_phrase(name)
    ist = _ist_12h(str(alarm.get("StateChangeTime") or ""))
    if state == "OK":
        return f"✅ Recovered: {phrase} — {ist} IST"
    emoji = _severity_emoji("", state) if state else "🔔"
    return f"{emoji} {phrase}\n{ist} IST"


def _recovered_line(phrases: list[str], ist: str) -> str:
    """ONE green line covering one or more recovered alarms."""
    return f"✅ Recovered: {'; '.join(phrases)} — {ist} IST"


def _parse_alarm(message: Any) -> dict[str, Any] | None:
    """Return the CloudWatch alarm dict if `message` is an alarm JSON, else None."""
    if not isinstance(message, str):
        return None
    try:
        parsed = json.loads(message)
    except (TypeError, json.JSONDecodeError):
        return None
    if isinstance(parsed, dict) and "AlarmName" in parsed:
        return parsed
    return None


def _format_plain_sns(subject: str | None, message: Any) -> str:
    """Format a non-CloudWatch SNS publish (e.g., from deploy-aws workflow)."""
    emoji = _severity_emoji(subject or "", None)
    body = message if isinstance(message, str) else str(message)
    if subject:
        return f"{emoji} {subject}\n{body}"
    return f"{emoji} {body}"


def _should_suppress_ok(name: str, now_epoch: float, cache: dict[str, tuple[str, float]]) -> bool:
    """True when an OK for `name` repeats a recent OK (warm-container dedupe).

    ONLY ever called for OK-state records — ALARM records are never
    routed through this cache (never-drop law).
    """
    entry = cache.get(name)
    if entry is None:
        return False
    last_state, last_epoch = entry
    return last_state == "OK" and (now_epoch - last_epoch) < OK_REPEAT_SUPPRESS_SECS


def _fold_records(
    records: list[dict[str, Any]],
    now_epoch: float | None = None,
    cache: dict[str, tuple[str, float]] | None = None,
) -> list[str]:
    """Fold one SNS batch into the final list of Telegram texts.

    Rules (judge final contract, Module 5):
    - ALARM records stay INDIVIDUAL house lines — never digested,
      never suppressed (a later ALARM for the same alarm supersedes an
      earlier one in the same batch: still exactly one 🆘 delivered).
    - An ALARM followed by OK for the same alarm inside this batch
      folds to ONLY the recovered line. An OK followed by a re-ALARM
      keeps the ALARM — the final state per alarm decides, so a 🆘
      can never be dropped by an older ✅.
    - All lone-OK records fold into ONE recovered line.
    - Repeat OK within OK_REPEAT_SUPPRESS_SECS of a sent OK for the
      same alarm is suppressed (warm cache); ALARM is never consulted.
    - A malformed record folds to a safe generic line — never a crash,
      never raw JSON.
    """
    now = time.time() if now_epoch is None else now_epoch
    plain_texts: list[str] = []
    # Per alarm name (first-appearance order): the LAST record wins,
    # remembering whether an ALARM was seen anywhere in the batch.
    name_order: list[str] = []
    last_by_name: dict[str, dict[str, Any]] = {}
    saw_alarm: dict[str, bool] = {}

    for record in records:
        try:
            sns = (record or {}).get("Sns") or {}
            message = sns.get("Message", "")
            alarm = _parse_alarm(message)
            if alarm is not None:
                # Forensics stay in CloudWatch Logs — NEVER in Telegram text.
                print(
                    "alarm-forensics "
                    f"name={alarm.get('AlarmName')} "
                    f"state={alarm.get('NewStateValue')} "
                    f"reason={alarm.get('NewStateReason')}"
                )
                name = str(alarm.get("AlarmName") or "unknown-alarm")
                state = str(alarm.get("NewStateValue") or "ALARM").upper()
                if name not in last_by_name:
                    name_order.append(name)
                last_by_name[name] = alarm
                saw_alarm[name] = saw_alarm.get(name, False) or state == "ALARM"
            else:
                plain_texts.append(_format_plain_sns(sns.get("Subject"), message))
        except Exception:  # noqa: BLE001 — fail-open per contract
            logger.exception("Malformed SNS record — sending safe generic line")
            plain_texts.append(_GENERIC_SAFE_LINE)

    out: list[str] = []
    lone_ok_phrases: list[str] = []
    lone_ok_ist: str | None = None

    for name in name_order:
        # Per-record fail-open (never-drop law): one poisoned alarm must
        # never crash the render loop and take genuine 🆘 pages with it.
        try:
            alarm = last_by_name[name]
            final_state = str(alarm.get("NewStateValue") or "ALARM").upper()

            if final_state == "OK" and saw_alarm.get(name, False):
                # ALARM→OK inside one batch: ONLY the recovered line.
                out.append(_house_line(alarm))
                if cache is not None:
                    cache[name] = ("OK", now)
                continue

            if final_state == "OK":
                # Lone OK flip — warm-cache dedupe, then fold into ONE line.
                if cache is not None and _should_suppress_ok(name, now, cache):
                    print(f"ok-repeat-suppressed name={name}")
                    continue
                lone_ok_phrases.append(_alarm_phrase(name))
                lone_ok_ist = _ist_12h(str(alarm.get("StateChangeTime") or ""))
                if cache is not None:
                    cache[name] = ("OK", now)
                continue

            # ALARM / INSUFFICIENT_DATA final state — individual house line,
            # NEVER suppressed, NEVER folded away by an earlier OK.
            out.append(_house_line(alarm))
            if cache is not None:
                cache[name] = (final_state, now)
        except Exception:  # noqa: BLE001 — fail-open per contract
            logger.exception("Alarm render failed — sending safe generic line")
            out.append(_GENERIC_SAFE_LINE)

    if lone_ok_phrases:
        try:
            out.append(_recovered_line(lone_ok_phrases, lone_ok_ist or _ist_12h("")))
        except Exception:  # noqa: BLE001 — fail-open per contract
            logger.exception("Recovered-line render failed — sending safe generic line")
            out.append(_GENERIC_SAFE_LINE)

    out.extend(plain_texts)
    return out


def _post_to_telegram(token: str, chat_id: str, text: str) -> tuple[int, str]:
    """POST to the Telegram bot API. Returns (status_code, body_text).

    Plain-text mode on purpose: no markup-parsing field in the payload,
    so an alarm name containing '*', '`' or '[' can never cause a
    silent Markdown-parse 400 drop.
    """
    url = f"{TELEGRAM_API_BASE}/bot{token}/sendMessage"
    payload = urllib.parse.urlencode(
        {
            "chat_id": chat_id,
            "text": text,
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

    try:
        texts = _fold_records(records, now_epoch=time.time(), cache=_LAST_SENT)
    except Exception:  # noqa: BLE001 — never-drop law: rendering must not kill the batch
        logger.exception("Batch fold crashed — degrading to one safe generic line per record")
        texts = [_GENERIC_SAFE_LINE for _ in records]

    sent = 0
    failures: list[str] = []
    for text in texts:
        try:
            status, body = _post_to_telegram(token, chat_id, text)
            if status >= 400:
                failures.append(f"http {status}: {body[:200]}")
                logger.error("Telegram POST returned %s: %s", status, body[:200])
            else:
                sent += 1
        except Exception as exc:  # noqa: BLE001
            failures.append(str(exc))
            logger.exception("Failed to relay one message to Telegram")
        # Cheap rate-limit cushion. Telegram allows ~30 msg/sec per bot;
        # if 5+ alarms fire in the same SNS batch we don't want to flirt
        # with their throttle.
        time.sleep(0.05)

    return {"sent": sent, "failures": failures, "records": len(records)}
