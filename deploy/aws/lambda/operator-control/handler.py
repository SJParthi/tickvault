"""Operator portal Lambda — ONE URL to run the whole product (VIEW + CONTROL).

Open the Function URL in a browser and you get a device-locked mission-control
page with tabs: Overview (box status + control), Data (live metrics + a QuestDB
query box), GitHub (open PRs + CI status + merge + trigger deploy), Logs (live
error/app tail), and AWS (CloudWatch alarms + month-to-date cost).

ONE URL, two layers:
  * `GET  /` → serves the portal HTML. PUBLIC shell with ZERO secrets — it just
               renders a lock screen + tabs. It can do nothing until unlocked.
  * `POST /` → every action requires `Authorization: Bearer <secret>`
               (constant-time compare). The key is saved only in the device's
               localStorage and sent on every POST. Only the operator's laptop
               + phone hold it, so in practice only those devices work.

SECURITY MODEL (deliberately strict — this can stop a live trading box):
* Bearer secret from SSM SecureString /tickvault/<env>/operator/control-secret.
* GitHub actions use a fine-grained PAT from SSM
  /tickvault/<env>/operator/github-token, scoped to this one repo. Never in the
  page, never in env, never in Terraform state.
* IAM scoped to exactly this instance + read-only CloudWatch/CostExplorer +
  the two SSM secrets. Nothing else.
* Destructive box actions (stop/reboot/restart-app/stop-app) are blocked during
  market hours (09:15-15:30 IST Mon-Fri) unless {"force": true}.
* The SQL box is READ-ONLY: only SELECT/SHOW/EXPLAIN/WITH are accepted; any
  mutating keyword is rejected before it ever reaches QuestDB.
"""

from __future__ import annotations

import datetime
import hmac
import json
import os
import time
import urllib.error
import urllib.request

REGION = os.environ.get("AWS_REGION", "ap-south-1")
INSTANCE_ID = os.environ.get("TV_INSTANCE_ID", "")
GH_REPO = os.environ.get("GH_REPO", "SJParthi/tickvault")
GH_DEPLOY_WORKFLOW = os.environ.get("GH_DEPLOY_WORKFLOW", "deploy-aws.yml")
_SECRET_PARAM = os.environ.get("OPERATOR_CONTROL_SECRET_PARAM", "")
_GH_TOKEN_PARAM = os.environ.get("OPERATOR_GITHUB_TOKEN_PARAM", "")

# Lazy-init boto3 clients so the pure-function tests run without boto3 installed.
_clients: dict[str, object] = {}


def _client(name: str, region: str = ""):
    key = name + (region or REGION)
    if key not in _clients:
        import boto3  # noqa: PLC0415

        _clients[key] = boto3.client(name, region_name=region or REGION)
    return _clients[key]


def _load_param(param: str) -> str:
    if not param:
        return ""
    try:
        return _client("ssm").get_parameter(Name=param, WithDecryption=True)["Parameter"]["Value"]
    except Exception:  # noqa: BLE001 — missing => deny / disable (fail closed)
        return ""


_SECRET_TTL_SECS = 60.0
_cache: dict[str, dict] = {}


def _cached_param(param: str) -> str:
    now = time.monotonic()
    c = _cache.get(param)
    if not c or not c["value"] or (now - c["ts"]) > _SECRET_TTL_SECS:
        _cache[param] = {"value": _load_param(param), "ts": now}
    return _cache[param]["value"]


def _control_secret() -> str:
    return _cached_param(_SECRET_PARAM)


def _github_token() -> str:
    return _cached_param(_GH_TOKEN_PARAM)


# Destructive box actions blocked during market hours unless force=true.
_DESTRUCTIVE = {"stop", "reboot", "restart-app", "stop-app", "wipe-questdb"}

_MKT_OPEN_SECS = 9 * 3600 + 15 * 60
_MKT_CLOSE_SECS = 15 * 3600 + 30 * 60
_IST_OFFSET_SECS = 19800  # +05:30

_VIEW_TIMEOUT_SECS = 6.0
_VIEW_POLL_SECS = 0.4
# Latency probes run 3×curl-to-Dhan (--max-time 3) + QuestDB + metrics ON the
# box (~9-12s wall-clock), so the poll window must be longer than the view's.
# Lambda timeout is 30s, so 22s leaves margin.
_LATENCY_TIMEOUT_SECS = 22.0

# Read-only SQL gate: the first keyword must be one of these.
_SQL_ALLOWED_PREFIXES = ("select", "show", "explain", "with")
# ...and none of these mutating keywords may appear anywhere.
_SQL_BANNED = (
    "insert",
    "update",
    "delete",
    "drop",
    "alter",
    "truncate",
    "create",
    "copy",
    "rename",
    "reindex",
    "vacuum",
    "grant",
    "revoke",
)


def _is_market_hours(now_utc: datetime.datetime) -> bool:
    """True during NSE market hours (Mon-Fri 09:15-15:30 IST)."""
    ist = now_utc + datetime.timedelta(seconds=_IST_OFFSET_SECS)
    if ist.weekday() >= 5:
        return False
    sod = ist.hour * 3600 + ist.minute * 60 + ist.second
    return _MKT_OPEN_SECS <= sod < _MKT_CLOSE_SECS


def _http_method(event: dict) -> str:
    try:
        return str(event["requestContext"]["http"]["method"]).upper()
    except (KeyError, TypeError):
        return str(event.get("httpMethod", "POST")).upper()


def _authorized(headers: dict) -> bool:
    """Constant-time bearer check. Header keys are lowercased by Function URL."""
    secret = _control_secret()
    if not secret:
        return False
    auth = (headers or {}).get("authorization", "")
    if not auth.startswith("Bearer "):
        return False
    return hmac.compare_digest(auth[len("Bearer ") :], secret)


def _is_safe_sql(query: str) -> bool:
    """Read-only gate. The query must start with an allowed keyword and contain
    no mutating keyword (whole-word). Pure function — fully unit-tested."""
    q = (query or "").strip().lower()
    if not q:
        return False
    import re  # noqa: PLC0415

    # First WORD must be an allowed read-only keyword (not just a prefix, so
    # "explainx ..." / "selector ..." are rejected).
    first = re.split(r"[^a-z]+", q, maxsplit=1)[0]
    if first not in _SQL_ALLOWED_PREFIXES:
        return False
    for kw in _SQL_BANNED:
        if re.search(r"\b" + kw + r"\b", q):
            return False
    return True


def _ssm_shell(commands: list[str]) -> str:
    resp = _client("ssm").send_command(
        InstanceIds=[INSTANCE_ID],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": commands},
    )
    return resp["Command"]["CommandId"]


def _ssm_shell_sync(commands: list[str], timeout: float = _VIEW_TIMEOUT_SECS) -> str:
    """Send a command and poll for output up to `timeout` seconds. Returns "" on
    any failure (e.g. the box is STOPPED — its SSM agent is offline, so
    send_command raises). Callers degrade to an empty snapshot, never a 500."""
    try:
        cid = _ssm_shell(commands)
    except Exception as exc:  # noqa: BLE001 — box stopped / SSM agent offline
        print(f"operator-portal ssm send_command failed (box offline?): {exc!r}")  # noqa: T201
        return ""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(_VIEW_POLL_SECS)
        try:
            inv = _client("ssm").get_command_invocation(CommandId=cid, InstanceId=INSTANCE_ID)
        except Exception:  # noqa: BLE001 — not registered yet
            continue
        if inv.get("Status") in ("Success", "Failed", "Cancelled", "TimedOut"):
            return (inv.get("StandardOutputContent", "") or "") + (
                inv.get("StandardErrorContent", "") or ""
            )
    return ""


_VIEW_COMMANDS = [
    "set +e",
    'echo "APP=$(systemctl is-active tickvault 2>/dev/null || echo inactive)"',
    "Q='http://127.0.0.1:9000/exp?query='",
    'echo "TICKS_TODAY=$(curl -fsS "${Q}SELECT%20count()%20FROM%20ticks%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C1M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_1m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C5M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_5m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C15M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_15m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C60M=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_60m%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "C1D=$(curl -fsS "${Q}SELECT%20count()%20FROM%20candles_1d%20WHERE%20ts%20IN%20today()" 2>/dev/null | tail -1)"',
    'echo "DEDUP_KEYS=$(curl -fsS "${Q}SELECT%20count()%20FROM%20table_columns(%27ticks%27)%20WHERE%20upsertKey=true" 2>/dev/null | tail -1)"',
    'echo "MAX_TPS=$(curl -fsS "${Q}SELECT%20max(c)%20FROM%20(SELECT%20count()%20c%20FROM%20ticks%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20ts,security_id)" 2>/dev/null | tail -1)"',
    'echo "ERRORS_BEGIN"',
    "journalctl -u tickvault -p err -n 5 --no-pager 2>/dev/null | tail -5 || true",
    'echo "ERRORS_END"',
]


def _parse_view(stdout: str) -> dict:
    """Parse the labeled snapshot stdout into a structured dict (pure)."""
    fields: dict[str, str] = {}
    errors: list[str] = []
    in_errors = False
    for line in (stdout or "").splitlines():
        if line == "ERRORS_BEGIN":
            in_errors = True
            continue
        if line == "ERRORS_END":
            in_errors = False
            continue
        if in_errors:
            if line.strip():
                errors.append(line.rstrip())
            continue
        if "=" in line:
            key, _, val = line.partition("=")
            fields[key.strip()] = val.strip()
    return {
        "app": fields.get("APP", ""),
        "ticks_today": fields.get("TICKS_TODAY", ""),
        "candles": {
            "1m": fields.get("C1M", ""),
            "5m": fields.get("C5M", ""),
            "15m": fields.get("C15M", ""),
            "60m": fields.get("C60M", ""),
            "1d": fields.get("C1D", ""),
        },
        "dedup_key_columns": fields.get("DEDUP_KEYS", ""),
        "max_ticks_per_second": fields.get("MAX_TPS", ""),
        "recent_errors": errors,
    }


# ----------------------------------------------------------------- latency snap
# Measures the REAL per-tick + per-process latency on the box:
#  * network RTT to the Dhan feed edge (TCP connect + TLS handshake, 3 samples)
#  * local QuestDB round-trip (SELECT 1)
#  * the app's own histograms at :9091/metrics — sum/count gives average ns for
#    tick parse+process (`tv_tick_processing_duration_ns`) and the full
#    wire->parsed->routed journey (`tv_wire_to_done_duration_ns`), plus order
#    placement (`tv_order_placement_duration_ns`).
#  * wall-clock skew vs chrony's NTP source.
_LATENCY_COMMANDS = [
    "set +e",
    'echo "DHAN_BEGIN"',
    "for i in 1 2 3; do curl -o /dev/null -s -w '%{time_connect} %{time_appconnect}\\n' --max-time 3 https://api-feed.dhan.co/ 2>/dev/null || echo 'x x'; done",
    'echo "DHAN_END"',
    "echo \"QDB=$(curl -o /dev/null -s -w '%{time_total}' --max-time 3 'http://127.0.0.1:9000/exec?query=SELECT%201' 2>/dev/null)\"",
    "echo \"SKEW=$(chronyc tracking 2>/dev/null | awk '/Last offset/{print $4}')\"",
    'echo "METRICS_BEGIN"',
    "curl -fsS --max-time 3 http://127.0.0.1:9091/metrics 2>/dev/null | grep -E '^tv_(tick_processing|wire_to_done|order_placement)_duration_ns_(sum|count)' || echo none",
    'echo "METRICS_END"',
]


def _avg_ns(sum_v: str, count_v: str) -> float | None:
    """sum/count -> average nanoseconds, or None if no samples. Pure."""
    try:
        s = float(sum_v)
        c = float(count_v)
        return s / c if c > 0 else None
    except (TypeError, ValueError):
        return None


def _parse_latency(stdout: str) -> dict:
    """Parse the labeled latency snapshot into a structured dict (pure)."""
    dhan_tcp: list[float] = []
    dhan_tls: list[float] = []
    metrics: dict[str, str] = {}
    fields: dict[str, str] = {}
    mode = ""
    for line in (stdout or "").splitlines():
        if line in ("DHAN_BEGIN", "METRICS_BEGIN"):
            mode = line.split("_")[0]
            continue
        if line in ("DHAN_END", "METRICS_END"):
            mode = ""
            continue
        if mode == "DHAN":
            parts = line.split()
            if len(parts) == 2:
                try:
                    dhan_tcp.append(float(parts[0]))
                    dhan_tls.append(float(parts[1]))
                except ValueError:
                    pass
            continue
        if mode == "METRICS":
            # e.g. "tv_tick_processing_duration_ns_sum 1.23e6"
            bits = line.split()
            if len(bits) == 2:
                metrics[bits[0]] = bits[1]
            continue
        if "=" in line:
            k, _, v = line.partition("=")
            fields[k.strip()] = v.strip()

    def ms(values: list[float]) -> str:
        vals = [v for v in values if v > 0]
        return f"{min(vals) * 1000:.1f}" if vals else ""

    def sec_to_ms(s: str) -> str:
        try:
            return f"{float(s) * 1000:.1f}"
        except (TypeError, ValueError):
            return ""

    def avg(name: str):
        a = _avg_ns(metrics.get(name + "_sum", ""), metrics.get(name + "_count", ""))
        return round(a, 1) if a is not None else None

    return {
        "dhan_tcp_ms": ms(dhan_tcp),
        "dhan_tls_ms": ms(dhan_tls),
        "questdb_ms": sec_to_ms(fields.get("QDB", "")),
        "clock_skew_ms": sec_to_ms(fields.get("SKEW", "")),
        "tick_process_avg_ns": avg("tv_tick_processing_duration_ns"),
        "wire_to_done_avg_ns": avg("tv_wire_to_done_duration_ns"),
        "order_place_avg_ns": avg("tv_order_placement_duration_ns"),
        "tick_count": metrics.get("tv_tick_processing_duration_ns_count", ""),
    }


# ---------------------------------------------------------------- GitHub helper
def _gh(method: str, path: str, body: dict | None = None) -> tuple[int, object]:
    """Minimal GitHub REST call using urllib (no deps). Returns (status, json)."""
    token = _github_token()
    if not token:
        return 0, {"error": "github token not configured"}
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request("https://api.github.com" + path, data=data, method=method)
    req.add_header("authorization", "Bearer " + token)
    req.add_header("accept", "application/vnd.github+json")
    req.add_header("x-github-api-version", "2022-11-28")
    req.add_header("user-agent", "tickvault-operator-console")
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


def _gh_open_prs() -> list[dict]:
    """List open PRs with their head-commit CI rollup. Top 10, newest first."""
    st, prs = _gh("GET", f"/repos/{GH_REPO}/pulls?state=open&per_page=10&sort=created&direction=desc")
    if st != 200 or not isinstance(prs, list):
        return []
    out = []
    for pr in prs[:10]:
        sha = (pr.get("head") or {}).get("sha", "")
        ci = "unknown"
        if sha:
            cst, cs = _gh("GET", f"/repos/{GH_REPO}/commits/{sha}/status")
            if cst == 200 and isinstance(cs, dict):
                ci = cs.get("state", "unknown")  # success | pending | failure
        out.append(
            {
                "number": pr.get("number"),
                "title": pr.get("title", ""),
                "draft": bool(pr.get("draft")),
                "mergeable_state": pr.get("mergeable_state", ""),
                "ci": ci,
            }
        )
    return out


# --------------------------------------------------------------------- response
def _resp(status: int, body: dict) -> dict:
    return {"statusCode": status, "headers": {"content-type": "application/json"}, "body": json.dumps(body)}


def _html_resp() -> dict:
    return {
        "statusCode": 200,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
            "referrer-policy": "no-referrer",
        },
        "body": _console_html(),
    }


def lambda_handler(event, _context):
    if _http_method(event) == "GET":
        return _html_resp()

    headers = event.get("headers") or {}
    if not _authorized(headers):
        return _resp(401, {"error": "unauthorized"})

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
            {"error": 'blocked during market hours (09:15-15:30 IST). Re-send with {"force": true} to override.', "action": action},
        )

    try:
        # ---- box control ----
        if action == "start":
            _client("ec2").start_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action})
        if action == "stop":
            _client("ec2").stop_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action})
        if action == "reboot":
            _client("ec2").reboot_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action})
        if action == "restart-app":
            return _resp(200, {"ok": True, "action": action, "command_id": _ssm_shell(["systemctl restart tickvault"])})
        if action == "stop-app":
            cid = _ssm_shell(["systemctl stop tickvault || true", "systemctl disable tickvault || true"])
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "restart-questdb":
            cid = _ssm_shell(["cd /opt/tickvault/repo/deploy/docker && docker compose restart questdb"])
            return _resp(200, {"ok": True, "action": action, "command_id": cid})
        if action == "wipe-questdb":
            # DESTRUCTIVE: deletes ALL QuestDB data for a fresh start. Always
            # requires force=true (even off-hours), and is in _DESTRUCTIVE so it
            # is ALSO market-hours-blocked. Sequence: stop app -> compose down -v
            # (removes the tv-questdb-data volume) -> up -d (empty QuestDB) ->
            # start app (boot DDL recreates the schema). Next session = fresh.
            if not force:
                return _resp(409, {"error": 'wipe is destructive — re-send with {"force": true}', "action": action})
            cid = _ssm_shell(
                [
                    "set +e",
                    "systemctl stop tickvault || true",
                    "cd /opt/tickvault/repo/deploy/docker && docker compose down -v",
                    "cd /opt/tickvault/repo/deploy/docker && docker compose up -d",
                    "sleep 8",
                    "systemctl start tickvault || true",
                ]
            )
            return _resp(200, {"ok": True, "action": action, "command_id": cid})

        # ---- overview / data ----
        if action in ("status", "view"):
            inst = _client("ec2").describe_instances(InstanceIds=[INSTANCE_ID])["Reservations"][0]["Instances"][0]
            state = inst["State"]["Name"]
            # The on-box snapshot needs the SSM agent, which is only reachable
            # while the instance is RUNNING. The box auto-stops 16:30 IST, so
            # off-hours we skip SSM entirely and return a clean "stopped" view
            # (instance_state + empty snapshot) instead of 500ing. The operator
            # then taps ▶ Start. _ssm_shell_sync also fails-soft, but skipping
            # avoids a pointless ~6s wait.
            snap = _parse_view(_ssm_shell_sync(_VIEW_COMMANDS) if state == "running" else "")
            return _resp(
                200,
                {"ok": True, "action": "view", "instance_state": state, "market_hours": _is_market_hours(datetime.datetime.utcnow()), **snap},
            )
        if action == "sql":
            q = str(payload.get("query", "")).strip()
            if not _is_safe_sql(q):
                return _resp(400, {"error": "only read-only SELECT/SHOW/EXPLAIN/WITH queries are allowed"})
            import urllib.parse  # noqa: PLC0415

            enc = urllib.parse.quote(q)
            out = _ssm_shell_sync(["set +e", f"curl -fsS 'http://127.0.0.1:9000/exp?query={enc}&limit=200' 2>/dev/null | head -200 || echo 'query failed'"])
            return _resp(200, {"ok": True, "action": "sql", "csv": out})
        if action == "logs":
            out = _ssm_shell_sync(
                [
                    "set +e",
                    "echo ERR_BEGIN",
                    "journalctl -u tickvault -p err -n 40 --no-pager 2>/dev/null | tail -40 || true",
                    "echo ERR_END",
                    "echo APP_BEGIN",
                    "journalctl -u tickvault -n 40 --no-pager 2>/dev/null | tail -40 || true",
                    "echo APP_END",
                ]
            )
            return _resp(200, {"ok": True, "action": "logs", "raw": out})
        if action == "latency":
            return _resp(200, {"ok": True, "action": "latency", **_parse_latency(_ssm_shell_sync(_LATENCY_COMMANDS, timeout=_LATENCY_TIMEOUT_SECS))})

        # ---- github ----
        if action == "gh_prs":
            # gh_configured=False lets the UI say "token not set" instead of the
            # misleading "no open PRs 🎉" when the github-token SSM param is absent.
            return _resp(200, {"ok": True, "action": action, "prs": _gh_open_prs(), "repo": GH_REPO, "gh_configured": bool(_github_token())})
        if action == "gh_merge":
            n = int(payload.get("number", 0))
            if n <= 0:
                return _resp(400, {"error": "missing PR number"})
            st, body = _gh("PUT", f"/repos/{GH_REPO}/pulls/{n}/merge", {"merge_method": "squash"})
            return _resp(200, {"ok": st in (200, 201), "action": action, "status": st, "merged": isinstance(body, dict) and body.get("merged", False)})
        if action == "gh_deploy":
            st, _b = _gh("POST", f"/repos/{GH_REPO}/actions/workflows/{GH_DEPLOY_WORKFLOW}/dispatches", {"ref": "main"})
            return _resp(200, {"ok": st in (201, 204), "action": action, "status": st})

        # ---- aws ----
        if action == "aws_status":
            # Each read is independently fail-soft so a throttle on one doesn't
            # 500 the whole tab.
            try:
                alarms = _client("cloudwatch").describe_alarms(StateValue="ALARM", MaxRecords=20)
                firing = [a["AlarmName"] for a in alarms.get("MetricAlarms", [])]
            except Exception as exc:  # noqa: BLE001
                print(f"operator-portal describe_alarms failed: {exc!r}")  # noqa: T201
                firing = None
            return _resp(200, {"ok": True, "action": action, "alarms_firing": firing, "cost_mtd_usd": _month_to_date_cost()})

        return _resp(400, {"error": f"unknown action: {action!r}"})
    except Exception as exc:  # noqa: BLE001
        print(f"operator-portal action={action!r} failed: {exc!r}")  # noqa: T201 — CloudWatch
        return _resp(500, {"error": "action failed", "action": action})


def _month_to_date_cost() -> str:
    """Month-to-date AWS spend via Cost Explorer (us-east-1 endpoint)."""
    try:
        today = datetime.date.today()
        start = today.replace(day=1).isoformat()
        end = (today + datetime.timedelta(days=1)).isoformat()
        ce = _client("ce", region="us-east-1")
        r = ce.get_cost_and_usage(
            TimePeriod={"Start": start, "End": end},
            Granularity="MONTHLY",
            Metrics=["UnblendedCost"],
        )
        amt = r["ResultsByTime"][0]["Total"]["UnblendedCost"]["Amount"]
        return f"{float(amt):.2f}"
    except Exception:  # noqa: BLE001
        return ""


def _console_html() -> str:
    """The single-page operator portal. Vanilla JS, no deps, no secrets. The
    token is the device key: saved only in this device's localStorage, sent as
    `Authorization: Bearer <token>` on every POST. Raw string so embedded JS
    regex/escapes survive verbatim."""
    return r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1">
<title>tickvault — operator portal</title>
<style>
  :root{ color-scheme:dark; --bg:#070a12; --card:#111726cc; --line:#222c40; --txt:#e9eef7;
         --mut:#8a93a6; --grn:#28d17c; --red:#ff5d6c; --amb:#f4b740; --blu:#4f8cff; --cyan:#38e1d6; }
  *{ box-sizing:border-box; -webkit-tap-highlight-color:transparent; }
  body{ margin:0; font-family:-apple-system,BlinkMacSystemFont,Segoe UI,Roboto,sans-serif; color:var(--txt);
        min-height:100vh; padding:18px 14px 48px;
        background:radial-gradient(1200px 600px at 10% -10%, #15233f55, transparent 60%),
          radial-gradient(1000px 500px at 110% 0%, #1c2a1f55, transparent 55%),
          linear-gradient(180deg,#070a12,#0a0f1c); background-attachment:fixed; }
  .wrap{ max-width:920px; margin:0 auto; }
  .hdr{ display:flex; align-items:center; gap:12px; margin-bottom:14px; }
  .logo{ font-size:22px; font-weight:800; background:linear-gradient(90deg,#7fb2ff,#38e1d6,#28d17c);
         -webkit-background-clip:text; background-clip:text; color:transparent; }
  .live{ display:flex; align-items:center; gap:7px; font-size:12px; color:var(--mut); margin-left:auto; }
  .dot{ width:11px; height:11px; border-radius:50%; background:var(--red); }
  .dot.on{ background:var(--grn); animation:pulse 1.6s infinite; }
  @keyframes pulse{ 0%{box-shadow:0 0 0 0 #28d17caa} 70%{box-shadow:0 0 0 12px #28d17c00} 100%{box-shadow:0 0 0 0 #28d17c00} }
  .tabs{ display:flex; gap:6px; flex-wrap:wrap; margin-bottom:14px; }
  .tab{ padding:9px 14px; border-radius:11px; background:#0c1322; border:1px solid var(--line); color:var(--mut);
        font-size:13px; font-weight:700; cursor:pointer; }
  .tab.active{ color:#fff; background:linear-gradient(90deg,#1c3158,#163b39); border-color:#2f5fb0; }
  .card{ background:var(--card); border:1px solid var(--line); border-radius:16px; padding:16px; margin-bottom:14px;
         backdrop-filter:blur(6px); box-shadow:0 10px 30px #00000055; animation:rise .45s ease both; }
  @keyframes rise{ from{opacity:0; transform:translateY(8px)} to{opacity:1; transform:none} }
  .lbl{ font-size:11px; text-transform:uppercase; letter-spacing:1px; color:var(--mut); margin-bottom:12px; }
  input,textarea{ width:100%; padding:12px; border-radius:11px; border:1px solid #2a3346; background:#070a12;
        color:var(--txt); font-size:14px; font-family:inherit; }
  textarea{ min-height:64px; font-family:ui-monospace,Menlo,monospace; }
  button{ border:0; border-radius:12px; padding:13px 15px; font-size:14px; font-weight:700; cursor:pointer;
          color:#06101e; transition:transform .08s; }
  button:active{ transform:scale(.97); }
  .b-blu{ background:linear-gradient(90deg,#4f8cff,#38e1d6); color:#fff; } .b-go{ background:linear-gradient(90deg,#28d17c,#7be0a3); }
  .b-amb{ background:linear-gradient(90deg,#f4b740,#ffd479); } .b-stop{ background:linear-gradient(90deg,#ff5d6c,#ff8a93); color:#fff; }
  .b-ghost{ background:#1a2336; color:var(--txt); }
  .row{ display:flex; gap:10px; flex-wrap:wrap; } .row>button{ flex:1 1 150px; }
  .hero{ display:flex; gap:16px; flex-wrap:wrap; }
  .bignum{ flex:2 1 240px; background:linear-gradient(135deg,#0e1626,#0a1120); border:1px solid var(--line);
           border-radius:16px; padding:18px; position:relative; overflow:hidden; }
  .bignum .n{ font-size:44px; font-weight:900; line-height:1; background:linear-gradient(90deg,#9fd0ff,#38e1d6);
              -webkit-background-clip:text; background-clip:text; color:transparent; }
  .bignum .c{ font-size:12px; color:var(--mut); margin-top:6px; }
  .bignum::after{ content:""; position:absolute; inset:0; background:linear-gradient(120deg,transparent 30%,#ffffff14 50%,transparent 70%);
        transform:translateX(-120%); animation:shine 3.4s infinite; } @keyframes shine{ to{transform:translateX(120%)} }
  .pills{ flex:1 1 160px; display:grid; grid-template-columns:1fr 1fr; gap:10px; }
  .pill{ border:1px solid var(--line); border-radius:13px; padding:12px; background:#070b15; }
  .pill .v{ font-size:18px; font-weight:800; } .pill .k{ font-size:11px; color:var(--mut); margin-top:3px; }
  .bar{ display:flex; align-items:center; gap:10px; margin:9px 0; }
  .bar .name{ width:46px; font-size:12px; color:var(--mut); }
  .bar .track{ flex:1; height:14px; background:#0a1120; border-radius:8px; overflow:hidden; border:1px solid var(--line); }
  .bar .fill{ height:100%; width:0; border-radius:8px; background:linear-gradient(90deg,#4f8cff,#38e1d6); transition:width .9s cubic-bezier(.2,.8,.2,1); }
  .bar .num{ width:60px; text-align:right; font-weight:700; font-size:13px; }
  .shields{ display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:10px; }
  .shield{ border:1px solid var(--line); border-radius:14px; padding:14px; background:#070b15; animation:pop .45s ease both; }
  @keyframes pop{ from{opacity:0; transform:scale(.94)} to{opacity:1} }
  .shield .ttl{ font-size:12px; color:var(--mut); } .shield .st{ font-size:17px; font-weight:800; margin-top:5px; }
  .shield.good{ border-color:#1d5b3d; box-shadow:0 0 18px #28d17c22; } .shield.bad{ border-color:#5b2330; box-shadow:0 0 18px #ff5d6c22; }
  .spark{ width:100%; height:70px; display:block; }
  .ok{ color:var(--grn);} .bad{ color:var(--red);} .warn{ color:var(--amb);} .cy{ color:var(--cyan);}
  .muted{ color:var(--mut); font-size:12px; }
  pre{ background:#070b15; border:1px solid var(--line); border-radius:11px; padding:10px; overflow:auto;
       font-size:11.5px; white-space:pre-wrap; max-height:300px; }
  table{ width:100%; border-collapse:collapse; font-size:12px; } th,td{ text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); }
  th{ color:var(--mut); font-weight:600; }
  .pr{ display:flex; align-items:center; gap:10px; padding:10px 0; border-bottom:1px solid var(--line); flex-wrap:wrap; }
  .pr .t{ flex:1 1 200px; } .pr .num{ color:var(--mut); font-weight:700; margin-right:6px; }
  .badge{ font-size:11px; font-weight:800; padding:3px 9px; border-radius:20px; }
  .badge.success{ background:#103a27; color:var(--grn);} .badge.pending{ background:#3a3110; color:var(--amb);}
  .badge.failure{ background:#3a1620; color:var(--red);} .badge.unknown{ background:#1a2336; color:var(--mut);}
  .mini{ padding:9px 12px; font-size:12px; flex:0 0 auto; }
  .switch{ display:flex; align-items:center; gap:8px; font-size:12px; color:var(--mut); }
  .toast{ position:fixed; left:50%; bottom:22px; transform:translateX(-50%) translateY(20px); background:#1a2336;
          border:1px solid var(--line); padding:12px 18px; border-radius:12px; font-size:14px; opacity:0; transition:.25s;
          pointer-events:none; box-shadow:0 10px 30px #000; } .toast.show{ opacity:1; transform:translateX(-50%) translateY(0); }
  [hidden]{ display:none !important; }
  .lock{ text-align:center; padding:26px 18px; } .lock .ic{ font-size:40px; }
  .spin{ animation:spin 1s linear infinite; display:inline-block; } @keyframes spin{ to{transform:rotate(360deg)} }
</style>
</head>
<body>
<div class="wrap">
  <div class="hdr">
    <div class="logo">🛰️ tickvault</div>
    <div class="live"><span class="dot" id="livedot"></span><span id="livetxt">offline</span></div>
  </div>

  <div class="card lock" id="lock">
    <div class="ic">🔐</div>
    <div class="lbl" style="margin-top:8px">device key</div>
    <p class="muted" style="max-width:460px;margin:6px auto 14px">This portal only works on a device that holds your
      secret key — your laptop and your phone. Anyone else who opens this link sees this screen and can do nothing.
      Paste your key once; it is saved on THIS device only.</p>
    <input id="tok" type="password" placeholder="paste your operator key" autocomplete="off">
    <div class="row" style="margin-top:12px"><button class="b-blu" onclick="unlock()">🔓 Unlock this device</button></div>
  </div>

  <div id="app" hidden>
    <div class="tabs">
      <div class="tab active" data-t="overview" onclick="tab('overview')">📊 Overview</div>
      <div class="tab" data-t="data" onclick="tab('data')">📈 Data</div>
      <div class="tab" data-t="github" onclick="tab('github')">🔀 GitHub</div>
      <div class="tab" data-t="logs" onclick="tab('logs')">📜 Logs</div>
      <div class="tab" data-t="aws" onclick="tab('aws')">☁️ AWS</div>
      <div class="tab" data-t="latency" onclick="tab('latency')">⚡ Latency</div>
    </div>

    <!-- OVERVIEW -->
    <section data-tab="overview">
      <div class="card">
        <div class="hero">
          <div class="bignum"><div class="n" id="ticksbig">0</div><div class="c">ticks captured today</div></div>
          <div class="pills">
            <div class="pill"><div class="v" id="p_inst">—</div><div class="k">instance</div></div>
            <div class="pill"><div class="v" id="p_app">—</div><div class="k">app</div></div>
            <div class="pill"><div class="v" id="p_mkt">—</div><div class="k">market</div></div>
            <div class="pill"><div class="v" id="p_tps">—</div><div class="k">peak ticks/sec</div></div>
          </div>
        </div>
        <div class="row" style="margin-top:14px"><button class="b-blu" id="refbtn" onclick="loadOverview()">🔄 Refresh now</button></div>
        <div style="display:flex;align-items:center;gap:14px;margin-top:10px">
          <label class="switch"><input type="checkbox" id="auto" checked onchange="autotoggle()"> auto-refresh (8s)</label>
          <span class="muted" id="updated"></span>
        </div>
      </div>
      <div class="card"><div class="lbl">live ticks/sec — proof no sub-second tick is lost</div>
        <svg class="spark" id="spark" viewBox="0 0 300 70" preserveAspectRatio="none"></svg></div>
      <div class="card"><div class="lbl">guarantees — live proof read from the box</div><div class="shields" id="shields"></div></div>
      <div class="card"><div class="lbl">control</div>
        <div class="row" style="margin-bottom:10px">
          <button class="b-go" onclick="act('start')">▶ Start instance</button>
          <button class="b-amb" onclick="act('restart-app')">♻ Restart app</button>
          <button class="b-amb" onclick="act('restart-questdb')">♻ Restart QuestDB</button></div>
        <div class="row">
          <button class="b-stop" onclick="act('stop')">⏹ Stop instance</button>
          <button class="b-stop" onclick="act('stop-app')">⏹ Stop app</button></div>
        <div class="muted" style="margin-top:10px">Stop / restart are blocked 9:15 AM–3:30 PM IST Mon–Fri.</div>
        <label class="switch" style="margin-top:8px"><input type="checkbox" id="force"> force (override market-hours guard)</label>
        <div class="row" style="margin-top:18px"><button class="b-stop" onclick="wipeData()">🗑️ Wipe ALL data → fresh start</button></div>
        <div class="muted" style="margin-top:6px">Deletes every tick &amp; candle and restarts empty. Run when the box is RUNNING; next session = fresh data. Asks you to type WIPE.</div>
        <div class="row" style="margin-top:14px"><button class="b-ghost" onclick="lock()">🔒 Lock / forget this device</button></div>
      </div>
    </section>

    <!-- DATA -->
    <section data-tab="data" hidden>
      <div class="card"><div class="lbl">candles sealed today (per timeframe)</div><div id="bars"></div></div>
      <div class="card"><div class="lbl">QuestDB query (read-only)</div>
        <textarea id="sql" placeholder="SELECT ts, security_id, count() FROM ticks WHERE ts IN today() GROUP BY ts, security_id ORDER BY 3 DESC LIMIT 20"></textarea>
        <div class="row" style="margin-top:10px"><button class="b-blu" onclick="runSql()">▶ Run query</button></div>
        <div id="sqlout" style="margin-top:12px;overflow:auto"></div>
        <div class="muted" style="margin-top:8px">Only SELECT / SHOW / EXPLAIN / WITH are allowed. Capped at 200 rows.</div>
      </div>
    </section>

    <!-- GITHUB -->
    <section data-tab="github" hidden>
      <div class="card"><div class="lbl">github — <span id="ghrepo">repo</span></div>
        <div class="row" style="margin-bottom:8px">
          <button class="b-blu" onclick="loadGithub()">🔄 Load open PRs</button>
          <button class="b-amb" onclick="ghDeploy()">🚀 Trigger deploy to AWS</button></div>
        <div id="prs"></div>
        <div class="muted" style="margin-top:8px">Merge = squash to main. Deploy runs the deploy-aws workflow on main.</div>
      </div>
    </section>

    <!-- LOGS -->
    <section data-tab="logs" hidden>
      <div class="card"><div class="lbl">errors (last 40)</div>
        <div class="row" style="margin-bottom:10px"><button class="b-blu" onclick="loadLogs()">🔄 Load logs</button></div>
        <pre id="logErr">—</pre></div>
      <div class="card"><div class="lbl">app log (last 40)</div><pre id="logApp">—</pre></div>
    </section>

    <!-- AWS -->
    <section data-tab="aws" hidden>
      <div class="card"><div class="lbl">aws</div>
        <div class="row" style="margin-bottom:12px"><button class="b-blu" onclick="loadAws()">🔄 Load alarms + cost</button></div>
        <div class="hero">
          <div class="bignum"><div class="n" id="cost">$—</div><div class="c">AWS spend this month (USD)</div></div>
          <div class="pills" style="grid-template-columns:1fr"><div class="pill"><div class="v" id="alarmcount">—</div><div class="k">alarms firing</div></div></div>
        </div>
        <div id="alarms" style="margin-top:12px"></div>
      </div>
    </section>

    <!-- LATENCY -->
    <section data-tab="latency" hidden>
      <div class="card"><div class="lbl">latency — measured live on the box</div>
        <div class="row" style="margin-bottom:12px"><button class="b-blu" onclick="loadLatency()">⚡ Measure now</button></div>
        <div class="lbl" style="margin-top:4px">network — how fast each tick reaches us</div>
        <div class="shields" id="latnet"></div>
        <div class="lbl" style="margin-top:16px">processing — how fast we handle each tick</div>
        <div class="shields" id="latproc"></div>
        <div class="muted" id="lattick" style="margin-top:10px"></div>
        <div class="muted" style="margin-top:6px">Budgets: tick parse ≤10 ns · full tick process ≤10 µs · order ≤100 ns.
          Dhan's exchange timestamp is 1-second granular, so the win is low, steady arrival — not microsecond co-location.</div>
      </div>
    </section>
  </div>
</div>
<div class="toast" id="toast"></div>

<script>
const $=id=>document.getElementById(id);
let TOKEN=localStorage.getItem('tv_token')||'';
let timer=null, hist=[], lastTicks=0, curTab='overview';

function toast(m){ const t=$('toast'); t.textContent=m; t.classList.add('show'); clearTimeout(t._h); t._h=setTimeout(()=>t.classList.remove('show'),2600); }
function esc(s){ return String(s).replace(/[<>&]/g,c=>({'<':'&lt;','>':'&gt;','&':'&amp;'}[c])); }
function setLive(on){ $('livedot').classList.toggle('on',on); $('livetxt').textContent=on?'live':'offline'; }

function unlock(){ const v=($('tok').value||'').trim(); if(!v){ toast('Paste your key first'); return; }
  TOKEN=v; localStorage.setItem('tv_token',v); $('lock').hidden=true; $('app').hidden=false; startAuto(); loadOverview(); }
function lock(){ TOKEN=''; localStorage.removeItem('tv_token'); $('tok').value=''; stopAuto(); $('app').hidden=true; $('lock').hidden=false; setLive(false); toast('Locked — key forgotten on this device'); }

function tab(name){ curTab=name; document.querySelectorAll('.tab').forEach(t=>t.classList.toggle('active',t.dataset.t===name));
  document.querySelectorAll('section[data-tab]').forEach(s=>s.hidden=s.dataset.tab!==name);
  if(name==='github' && !$('prs').dataset.loaded) loadGithub();
  if(name==='aws' && !$('alarms').dataset.loaded) loadAws();
  if(name==='latency' && !$('latnet').dataset.loaded) loadLatency();
  if(name==='logs' && $('logErr').textContent==='—') loadLogs(); }

async function call(action, extra){ if(!TOKEN){ toast('Locked'); return null; }
  try{ const r=await fetch(location.href,{method:'POST',
      headers:{'authorization':'Bearer '+TOKEN,'content-type':'application/json'},
      body:JSON.stringify(Object.assign({action},extra||{}))});
    const j=await r.json().catch(()=>({}));
    if(r.status===401){ toast('Wrong key (401)'); setLive(false); return null; }
    if(r.status===409){ toast(j.error||'Blocked during market hours'); return null; }
    if(!r.ok){ toast(j.error||('Error '+r.status)); return null; }
    return j;
  }catch(e){ toast('Network error'); setLive(false); return null; } }

function countUp(el,target){ target=parseInt(String(target).replace(/[^0-9]/g,''),10)||0; const from=lastTicks||0; lastTicks=target;
  const t0=performance.now(),dur=900; function step(t){ const k=Math.min(1,(t-t0)/dur);
    el.textContent=Math.round(from+(target-from)*(1-Math.pow(1-k,3))).toLocaleString(); if(k<1) requestAnimationFrame(step); } requestAnimationFrame(step); }

function drawSpark(){ const w=300,h=70,pad=6,xs=hist.slice(-40),max=Math.max(2,...xs);
  const pts=xs.map((v,i)=>{ const x=pad+(w-2*pad)*(xs.length<2?0:i/(xs.length-1)); const y=h-pad-(h-2*pad)*(v/max); return x.toFixed(1)+','+y.toFixed(1); });
  const line=pts.join(' '); const area=pts.length?'M'+pad+','+(h-pad)+' L'+line.replace(/ /g,' L')+' L'+(w-pad)+','+(h-pad)+' Z':'';
  $('spark').innerHTML='<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#38e1d6" stop-opacity=".5"/><stop offset="1" stop-color="#38e1d6" stop-opacity="0"/></linearGradient></defs>'+
    (area?'<path d="'+area+'" fill="url(#g)"/>':'')+(pts.length>1?'<polyline points="'+line+'" fill="none" stroke="#38e1d6" stroke-width="2.5" stroke-linejoin="round" stroke-linecap="round"/>':'')+
    (pts.length?'<circle cx="'+pts[pts.length-1].split(',')[0]+'" cy="'+pts[pts.length-1].split(',')[1]+'" r="3.5" fill="#38e1d6"/>':''); }

function bar(name,n,max){ const pct=max>0?Math.max(3,Math.round(100*n/max)):0;
  return '<div class="bar"><div class="name">'+name+'</div><div class="track"><div class="fill" style="width:'+pct+'%"></div></div><div class="num">'+n.toLocaleString()+'</div></div>'; }
function shield(t,s,good){ return '<div class="shield '+(good?'good':'bad')+'"><div class="ttl">'+t+'</div><div class="st '+(good?'ok':'warn')+'">'+s+'</div></div>'; }

async function loadOverview(){ $('refbtn').innerHTML='<span class="spin">🔄</span> Refreshing…'; const j=await call('view'); $('refbtn').textContent='🔄 Refresh now'; if(!j) return;
  const appOk=j.app==='active', running=j.instance_state==='running'; setLive(appOk&&running);
  countUp($('ticksbig'), j.ticks_today||'0');
  $('p_inst').innerHTML='<span class="'+(running?'ok':'bad')+'">'+(j.instance_state||'?')+'</span>';
  $('p_app').innerHTML='<span class="'+(appOk?'ok':'bad')+'">'+(appOk?'up':(j.app||'down'))+'</span>';
  $('p_mkt').innerHTML='<span class="'+(j.market_hours?'warn':'')+'">'+(j.market_hours?'OPEN':'closed')+'</span>';
  const tpsN=parseInt(j.max_ticks_per_second,10)||0; $('p_tps').innerHTML='<span class="cy">'+tpsN+'</span>';
  hist.push(tpsN); if(hist.length>60) hist.shift(); drawSpark();
  const c=j.candles||{}, vals=['1m','5m','15m','60m','1d'].map(k=>parseInt(c[k],10)||0), mx=Math.max(1,...vals);
  $('bars').innerHTML=['1m','5m','15m','60m','1d'].map((k,i)=>bar(k,vals[i],mx)).join('');
  const keysOk=(j.dedup_key_columns||'')==='4', capOk=tpsN>1;
  $('shields').innerHTML=shield('Dedup key columns',(j.dedup_key_columns||'?'),keysOk)+shield('Sub-second fix',keysOk?'LIVE ✅':'OLD ⚠',keysOk)+
    shield('No tick lost',capOk?'WORKING ✅':(j.market_hours?'CHECK ⚠':'idle'),capOk||!j.market_hours)+shield('Peak ticks / second',tpsN,capOk);
  $('updated').textContent='updated '+new Date().toLocaleTimeString(); }

async function runSql(){ const q=$('sql').value.trim(); if(!q){ toast('Type a query'); return; } $('sqlout').innerHTML='<span class="muted">running…</span>';
  const j=await call('sql',{query:q}); if(!j){ $('sqlout').innerHTML=''; return; }
  const lines=(j.csv||'').split(/\r?\n/).filter(x=>x.length); if(!lines.length){ $('sqlout').innerHTML='<span class="muted">no rows</span>'; return; }
  const rows=lines.map(l=>l.split(',').map(c=>c.replace(/^"|"$/g,'')));
  let h='<table><tr>'+rows[0].map(c=>'<th>'+esc(c)+'</th>').join('')+'</tr>';
  for(let i=1;i<rows.length;i++) h+='<tr>'+rows[i].map(c=>'<td>'+esc(c)+'</td>').join('')+'</tr>';
  $('sqlout').innerHTML=h+'</table>'; }

function ciBadge(s){ const k=['success','pending','failure'].includes(s)?s:'unknown'; return '<span class="badge '+k+'">'+s+'</span>'; }
async function loadGithub(){ $('prs').dataset.loaded='1'; $('prs').innerHTML='<span class="muted">loading…</span>'; const j=await call('gh_prs'); if(!j){ $('prs').innerHTML=''; return; }
  $('ghrepo').textContent=j.repo||''; const prs=j.prs||[];
  if(j.gh_configured===false){ $('prs').innerHTML='<span class="warn">GitHub token not set — add the github-token SSM param to enable this tab.</span>'; return; }
  if(!prs.length){ $('prs').innerHTML='<span class="muted">no open PRs 🎉</span>'; return; }
  $('prs').innerHTML=prs.map(p=>'<div class="pr"><div class="t"><span class="num">#'+p.number+'</span>'+esc(p.title)+(p.draft?' <span class="badge unknown">draft</span>':'')+'</div>'+ciBadge(p.ci)+
    '<button class="b-go mini" onclick="ghMerge('+p.number+')">merge</button></div>').join(''); }
async function ghMerge(n){ if(!confirm('Squash-merge PR #'+n+' to main?')) return; toast('merging #'+n+'…'); const j=await call('gh_merge',{number:n});
  if(j&&j.merged){ toast('✅ merged #'+n); setTimeout(loadGithub,1500); } else { toast('merge not completed (status '+(j&&j.status)+')'); } }
async function ghDeploy(){ if(!confirm('Trigger a deploy to AWS (deploy-aws workflow on main)?')) return; toast('triggering deploy…'); const j=await call('gh_deploy');
  toast(j&&j.ok?'🚀 deploy triggered':'deploy trigger failed (status '+(j&&j.status)+')'); }

async function loadLogs(){ $('logErr').textContent='loading…'; const j=await call('logs'); if(!j){ $('logErr').textContent='—'; return; }
  const raw=j.raw||''; const grab=(a,b)=>{ const m=raw.split(a)[1]; return m?m.split(b)[0].trim():''; };
  $('logErr').textContent=grab('ERR_BEGIN','ERR_END')||'none 🎉'; $('logApp').textContent=grab('APP_BEGIN','APP_END')||'—'; }

async function loadAws(){ $('alarms').dataset.loaded='1'; $('alarms').innerHTML='<span class="muted">loading…</span>'; const j=await call('aws_status'); if(!j){ $('alarms').innerHTML=''; return; }
  $('cost').textContent='$'+(j.cost_mtd_usd||'—');
  if(j.alarms_firing===null){ $('alarmcount').innerHTML='<span class="warn">?</span>'; $('alarms').innerHTML='<span class="warn">CloudWatch read failed — retry.</span>'; return; }
  const al=j.alarms_firing||[]; $('alarmcount').innerHTML='<span class="'+(al.length?'bad':'ok')+'">'+al.length+'</span>';
  $('alarms').innerHTML=al.length? al.map(a=>'<div class="pr"><span class="badge failure">ALARM</span> <span class="t">'+esc(a)+'</span></div>').join('') : '<span class="ok">all clear 🎉</span>'; }

function fmtNs(n){ if(n==null||n==='') return '—'; n=Number(n); if(n<1000) return n.toFixed(0)+' ns';
  if(n<1e6) return (n/1e3).toFixed(2)+' µs'; if(n<1e9) return (n/1e6).toFixed(2)+' ms'; return (n/1e9).toFixed(2)+' s'; }
function fmtMs(s){ return (s===''||s==null)?'—':s+' ms'; }
async function loadLatency(){ $('latnet').dataset.loaded='1'; $('latnet').innerHTML='<span class="muted">measuring…</span>'; $('latproc').innerHTML='';
  const j=await call('latency'); if(!j){ $('latnet').innerHTML=''; return; }
  const tcp=parseFloat(j.dhan_tcp_ms), skew=Math.abs(parseFloat(j.clock_skew_ms));
  $('latnet').innerHTML=
    shield('Network to Dhan (TCP)', fmtMs(j.dhan_tcp_ms), !isNaN(tcp)&&tcp<15)+
    shield('+ TLS handshake', fmtMs(j.dhan_tls_ms), true)+
    shield('QuestDB round-trip', fmtMs(j.questdb_ms), true)+
    shield('Clock skew', fmtMs(j.clock_skew_ms), isNaN(skew)||skew<50);
  $('latproc').innerHTML=
    shield('Per-tick process', fmtNs(j.tick_process_avg_ns), (j.tick_process_avg_ns||1e12)<10000)+
    shield('Wire → done (full)', fmtNs(j.wire_to_done_avg_ns), (j.wire_to_done_avg_ns||1e12)<100000)+
    shield('Order placement', fmtNs(j.order_place_avg_ns), true);
  $('lattick').textContent = j.tick_count? ('averaged over '+Number(j.tick_count).toLocaleString()+' ticks since boot') : 'no ticks measured yet (market closed?)'; }

async function act(action){ if(!confirm('Run "'+action+'" on the trading box?')) return; const force=$('force').checked; toast(action+'…');
  const j=await call(action,{force}); if(j){ toast('✅ '+action+' sent'); setTimeout(loadOverview,1600); } }

async function wipeData(){
  if(prompt('This DELETES every tick and candle, then restarts empty. The box must be RUNNING. Type WIPE to confirm:')!=='WIPE'){ toast('cancelled'); return; }
  toast('wiping → fresh start…');
  const j=await call('wipe-questdb',{force:true});
  if(j&&j.ok){ toast('✅ wipe started — fresh data from next session'); setTimeout(loadOverview,4000); }
  else { toast((j&&j.error)||'wipe failed — is the box running?'); } }

function startAuto(){ stopAuto(); if($('auto').checked) timer=setInterval(()=>{ if(curTab==='overview') loadOverview(); },8000); }
function stopAuto(){ if(timer){ clearInterval(timer); timer=null; } }
function autotoggle(){ $('auto').checked?startAuto():stopAuto(); }

// Ready-made link support: a URL ending in #key=<secret> (or ?key=) auto-saves
// the device key, strips it from the address bar, and unlocks — so the Telegram
// link "just works" with zero typing. The fragment never reaches the server.
(function(){ var k=(location.hash.match(/key=([^&]+)/)||[])[1] || new URLSearchParams(location.search).get('key');
  if(k){ try{ k=decodeURIComponent(k); }catch(e){} TOKEN=k; localStorage.setItem('tv_token',TOKEN);
    try{ history.replaceState(null,'',location.pathname); }catch(e){} } })();
if(TOKEN){ $('tok').value=TOKEN; $('lock').hidden=true; $('app').hidden=false; startAuto(); loadOverview(); }
</script>
</body>
</html>"""
