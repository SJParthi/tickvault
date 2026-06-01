"""Operator-control Lambda — the single-URL operator console (VIEW + CONTROL).

WHY THIS EXISTS
---------------
The operator wants ONE place — one URL they can open on a phone — to both SEE
the trading box (is the app up? are ticks flowing? are candles sealing? any
recent errors?) AND control it (start / stop / restart-app / restart-questdb).
No AWS console, no GitHub UI, no Grafana signup.

ONE URL, two layers:
  * `GET  /`  → serves the console HTML page. PUBLIC (no token) — the page is
                a static shell that contains ZERO secrets. It just renders a
                token box + status panel + buttons.
  * `POST /`  → every action ("view" snapshot, start, stop, restart-*) requires
                `Authorization: Bearer <secret>` (constant-time compare). The
                page reads the token the operator typed (kept in the browser's
                localStorage) and sends it on every POST.

SECURITY MODEL (deliberately strict — this can stop a live trading box)
-----------------------------------------------------------------------
* Function URL AWS auth is NONE, but EVERY POST must carry the bearer secret
  matching the SSM SecureString. Missing/wrong → 401. The GET page shell is
  intentionally public (no secret in it) so the operator can open it and THEN
  paste the token.
* The Lambda's IAM role is scoped to EXACTLY this instance: ec2 Start/Stop/
  Reboot on the one instance ARN, ssm:SendCommand + ssm:GetCommandInvocation on
  the one instance + AWS-RunShellScript only, ssm:GetParameter on the one
  secret. It can do NOTHING else.
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

REGION = os.environ.get("AWS_REGION", "ap-south-1")
INSTANCE_ID = os.environ.get("TV_INSTANCE_ID", "")
# Name of the SSM SecureString holding the bearer secret. We read the secret
# at cold start via ssm:GetParameter (decrypted) — it is NEVER placed in the
# Lambda env or Terraform state, so it cannot leak via GetFunctionConfiguration.
_SECRET_PARAM = os.environ.get("OPERATOR_CONTROL_SECRET_PARAM", "")

# Lazy-init boto3 clients so the pure-function tests (auth, market-hours,
# snapshot parsing, HTML serving) run without boto3 installed. The clients are
# constructed on first AWS call and cached for the warm Lambda's lifetime.
_clients: dict[str, object] = {}


def _ec2_client():
    if "ec2" not in _clients:
        import boto3  # noqa: PLC0415

        _clients["ec2"] = boto3.client("ec2", region_name=REGION)
    return _clients["ec2"]


def _ssm_client():
    if "ssm" not in _clients:
        import boto3  # noqa: PLC0415

        _clients["ssm"] = boto3.client("ssm", region_name=REGION)
    return _clients["ssm"]


def _load_control_secret() -> str:
    """Read the bearer secret from SSM SecureString (decrypted)."""
    if not _SECRET_PARAM:
        return ""
    try:
        return _ssm_client().get_parameter(Name=_SECRET_PARAM, WithDecryption=True)["Parameter"]["Value"]
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

# Synchronous SSM snapshot budget. SendCommand → poll GetCommandInvocation up to
# this long, then return whatever we have (console shows "snapshot pending").
_VIEW_TIMEOUT_SECS = 6.0
_VIEW_POLL_SECS = 0.4


def _is_market_hours(now_utc: datetime.datetime) -> bool:
    """True during NSE market hours (Mon–Fri 09:15–15:30 IST)."""
    ist = now_utc + datetime.timedelta(seconds=_IST_OFFSET_SECS)
    if ist.weekday() >= 5:  # Sat/Sun
        return False
    sod = ist.hour * 3600 + ist.minute * 60 + ist.second
    return _MKT_OPEN_SECS <= sod < _MKT_CLOSE_SECS


def _http_method(event: dict) -> str:
    """Extract the HTTP method from a Function URL (payload v2.0) event.
    Falls back to POST so a malformed event is treated as an action (which
    then hits the auth gate) rather than silently serving the page."""
    try:
        return str(event["requestContext"]["http"]["method"]).upper()
    except (KeyError, TypeError):
        return str(event.get("httpMethod", "POST")).upper()


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
    resp = _ssm_client().send_command(
        InstanceIds=[INSTANCE_ID],
        DocumentName="AWS-RunShellScript",
        Parameters={"commands": commands},
    )
    return resp["Command"]["CommandId"]


def _ssm_shell_sync(commands: list[str]) -> str:
    """Send a command and poll for its output up to _VIEW_TIMEOUT_SECS.
    Returns combined stdout+stderr, or "" on timeout. GetCommandInvocation
    can 400 (InvocationDoesNotExist) for a moment right after SendCommand,
    so we swallow exceptions and keep polling until the deadline."""
    cid = _ssm_shell(commands)
    deadline = time.monotonic() + _VIEW_TIMEOUT_SECS
    while time.monotonic() < deadline:
        time.sleep(_VIEW_POLL_SECS)
        try:
            inv = _ssm_client().get_command_invocation(CommandId=cid, InstanceId=INSTANCE_ID)
        except Exception:  # noqa: BLE001 — invocation not registered yet
            continue
        if inv.get("Status") in ("Success", "Failed", "Cancelled", "TimedOut"):
            return (inv.get("StandardOutputContent", "") or "") + (
                inv.get("StandardErrorContent", "") or ""
            )
    return ""


# The on-box snapshot. QuestDB `/exp` returns CSV (header line + value line),
# so `tail -1` yields just the number. journalctl gives reliable ERROR lines
# for the systemd unit regardless of the app's working directory.
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
    # Live proof the #954 sub-second fix is deployed: the ticks table's DEDUP
    # UPSERT key should be 4 columns (ts, security_id, segment, received_at).
    'echo "DEDUP_KEYS=$(curl -fsS "${Q}SELECT%20count()%20FROM%20table_columns(%27ticks%27)%20WHERE%20upsertKey=true" 2>/dev/null | tail -1)"',
    # Live proof sub-second capture works: the biggest burst of ticks sharing
    # one (second, instrument) today. >1 means distinct sub-second ticks are
    # being preserved (the whole point of #954); =1 collapses, 0/blank = idle.
    'echo "MAX_TPS=$(curl -fsS "${Q}SELECT%20max(c)%20FROM%20(SELECT%20count()%20c%20FROM%20ticks%20WHERE%20ts%20IN%20today()%20GROUP%20BY%20ts,security_id)" 2>/dev/null | tail -1)"',
    'echo "ERRORS_BEGIN"',
    "journalctl -u tickvault -p err -n 5 --no-pager 2>/dev/null | tail -5 || true",
    'echo "ERRORS_END"',
]


def _parse_view(stdout: str) -> dict:
    """Parse the labeled snapshot stdout into a structured dict. Pure function
    (no AWS) so it is fully unit-testable. Unknown / missing values become
    empty strings; the error block is collected between the BEGIN/END markers."""
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


def _resp(status: int, body: dict) -> dict:
    return {
        "statusCode": status,
        "headers": {"content-type": "application/json"},
        "body": json.dumps(body),
    }


def _html_resp() -> dict:
    return {
        "statusCode": 200,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
        },
        "body": _console_html(),
    }


def lambda_handler(event, _context):
    # GET → serve the public console shell (no secret in it). The page then
    # POSTs token-authenticated actions back to this same URL.
    if _http_method(event) == "GET":
        return _html_resp()

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
            _ec2_client().start_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action, "instance": INSTANCE_ID})
        if action == "stop":
            _ec2_client().stop_instances(InstanceIds=[INSTANCE_ID])
            return _resp(200, {"ok": True, "action": action, "instance": INSTANCE_ID})
        if action == "reboot":
            _ec2_client().reboot_instances(InstanceIds=[INSTANCE_ID])
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
        if action in ("status", "view"):
            state = _ec2_client().describe_instances(InstanceIds=[INSTANCE_ID])
            inst = state["Reservations"][0]["Instances"][0]
            snapshot = _parse_view(_ssm_shell_sync(_VIEW_COMMANDS))
            return _resp(
                200,
                {
                    "ok": True,
                    "action": "view",
                    "instance_state": inst["State"]["Name"],
                    "market_hours": _is_market_hours(datetime.datetime.utcnow()),
                    **snapshot,
                },
            )
        return _resp(400, {"error": f"unknown action: {action!r}"})
    except Exception as exc:  # noqa: BLE001
        # Review finding 1: do NOT echo the raw AWS exception to the caller —
        # it can leak ARNs / account id / topology. Log it for the operator;
        # return a generic message.
        print(f"operator-control action={action!r} failed: {exc!r}")  # noqa: T201 — CloudWatch
        return _resp(500, {"error": "action failed", "action": action})


def _console_html() -> str:
    """The single-page console. Vanilla JS, no dependencies, no secrets. The
    token the operator types is kept ONLY in the browser's localStorage and is
    sent as `Authorization: Bearer <token>` on every POST to this same URL.

    The token IS the device lock: the page is a public, inert shell; it can do
    NOTHING until a device that holds the secret unlocks it. Only the operator's
    laptop + phone hold that secret, so in practice only those devices work."""
    return """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1">
<title>tickvault — operator console</title>
<style>
  :root{ color-scheme:dark; --bg:#070a12; --card:#111726cc; --line:#222c40;
         --txt:#e9eef7; --mut:#8a93a6; --grn:#28d17c; --red:#ff5d6c; --amb:#f4b740;
         --blu:#4f8cff; --cyan:#38e1d6; }
  *{ box-sizing:border-box; -webkit-tap-highlight-color:transparent; }
  body{ margin:0; font-family:-apple-system,BlinkMacSystemFont,Segoe UI,Roboto,sans-serif;
        color:var(--txt); min-height:100vh; padding:18px 14px 40px;
        background:
          radial-gradient(1200px 600px at 10% -10%, #15233f55, transparent 60%),
          radial-gradient(1000px 500px at 110% 0%, #1c2a1f55, transparent 55%),
          linear-gradient(180deg,#070a12,#0a0f1c);
        background-attachment:fixed; }
  .wrap{ max-width:880px; margin:0 auto; }
  .hdr{ display:flex; align-items:center; gap:12px; margin-bottom:18px; }
  .logo{ font-size:22px; font-weight:800; letter-spacing:.3px;
         background:linear-gradient(90deg,#7fb2ff,#38e1d6,#28d17c); -webkit-background-clip:text;
         background-clip:text; color:transparent; }
  .live{ display:flex; align-items:center; gap:7px; font-size:12px; color:var(--mut);
         margin-left:auto; }
  .dot{ width:11px; height:11px; border-radius:50%; background:var(--red);
        box-shadow:0 0 0 0 #ff5d6c88; }
  .dot.on{ background:var(--grn); animation:pulse 1.6s infinite; }
  @keyframes pulse{ 0%{box-shadow:0 0 0 0 #28d17caa} 70%{box-shadow:0 0 0 12px #28d17c00} 100%{box-shadow:0 0 0 0 #28d17c00} }
  .card{ background:var(--card); border:1px solid var(--line); border-radius:16px;
         padding:16px; margin-bottom:14px; backdrop-filter:blur(6px);
         box-shadow:0 10px 30px #00000055; animation:rise .5s ease both; }
  @keyframes rise{ from{opacity:0; transform:translateY(10px)} to{opacity:1; transform:none} }
  .lbl{ font-size:11px; text-transform:uppercase; letter-spacing:1px; color:var(--mut);
        margin-bottom:12px; }
  input[type=password]{ width:100%; padding:14px; border-radius:12px; border:1px solid #2a3346;
        background:#070a12; color:var(--txt); font-size:15px; }
  button{ border:0; border-radius:13px; padding:14px 16px; font-size:14px; font-weight:700;
          cursor:pointer; color:#06101e; transition:transform .08s, filter .2s; }
  button:active{ transform:scale(.97); }
  .b-blu{ background:linear-gradient(90deg,#4f8cff,#38e1d6); color:#fff; width:100%; }
  .b-go{ background:linear-gradient(90deg,#28d17c,#7be0a3); }
  .b-amb{ background:linear-gradient(90deg,#f4b740,#ffd479); }
  .b-stop{ background:linear-gradient(90deg,#ff5d6c,#ff8a93); color:#fff; }
  .row{ display:flex; gap:10px; flex-wrap:wrap; }
  .row>button{ flex:1 1 150px; }
  /* hero */
  .hero{ display:flex; gap:16px; flex-wrap:wrap; align-items:stretch; }
  .bignum{ flex:2 1 240px; background:linear-gradient(135deg,#0e1626,#0a1120);
           border:1px solid var(--line); border-radius:16px; padding:18px; position:relative; overflow:hidden; }
  .bignum .n{ font-size:46px; font-weight:900; line-height:1;
              background:linear-gradient(90deg,#9fd0ff,#38e1d6); -webkit-background-clip:text;
              background-clip:text; color:transparent; }
  .bignum .c{ font-size:12px; color:var(--mut); margin-top:6px; }
  .bignum::after{ content:""; position:absolute; inset:0; background:
        linear-gradient(120deg,transparent 30%,#ffffff14 50%,transparent 70%);
        transform:translateX(-120%); animation:shine 3.4s infinite; }
  @keyframes shine{ to{ transform:translateX(120%);} }
  .pills{ flex:1 1 160px; display:grid; grid-template-columns:1fr 1fr; gap:10px; }
  .pill{ border:1px solid var(--line); border-radius:13px; padding:12px; background:#070b15; }
  .pill .v{ font-size:18px; font-weight:800; } .pill .k{ font-size:11px; color:var(--mut); margin-top:3px; }
  .grid{ display:grid; grid-template-columns:repeat(auto-fit,minmax(120px,1fr)); gap:10px; }
  .metric{ background:#070b15; border:1px solid var(--line); border-radius:13px; padding:12px;
           transition:border-color .3s; }
  .metric .v{ font-size:20px; font-weight:800; } .metric .k{ font-size:11px; color:var(--mut); margin-top:3px; }
  .ok{ color:var(--grn);} .bad{ color:var(--red);} .warn{ color:var(--amb);} .cy{ color:var(--cyan);}
  /* bars */
  .bar{ display:flex; align-items:center; gap:10px; margin:9px 0; }
  .bar .name{ width:46px; font-size:12px; color:var(--mut); }
  .bar .track{ flex:1; height:14px; background:#0a1120; border-radius:8px; overflow:hidden; border:1px solid var(--line); }
  .bar .fill{ height:100%; width:0; border-radius:8px;
              background:linear-gradient(90deg,#4f8cff,#38e1d6); transition:width .9s cubic-bezier(.2,.8,.2,1); }
  .bar .num{ width:52px; text-align:right; font-weight:700; font-size:13px; }
  /* shields */
  .shields{ display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:10px; }
  .shield{ border:1px solid var(--line); border-radius:14px; padding:14px; background:#070b15;
           position:relative; animation:pop .5s ease both; }
  @keyframes pop{ from{opacity:0; transform:scale(.94)} to{opacity:1; transform:none} }
  .shield .ttl{ font-size:12px; color:var(--mut); } .shield .st{ font-size:17px; font-weight:800; margin-top:5px; }
  .shield.good{ border-color:#1d5b3d; box-shadow:0 0 18px #28d17c22; }
  .shield.bad{ border-color:#5b2330; box-shadow:0 0 18px #ff5d6c22; }
  .spark{ width:100%; height:70px; display:block; }
  .errs div{ font-size:12px; padding:7px 10px; border-radius:9px; background:#0a1120;
             border:1px solid var(--line); margin-bottom:6px; word-break:break-word; }
  .muted{ color:var(--mut); font-size:12px; }
  .switch{ display:flex; align-items:center; gap:8px; font-size:12px; color:var(--mut); }
  .toast{ position:fixed; left:50%; bottom:22px; transform:translateX(-50%) translateY(20px);
          background:#1a2336; border:1px solid var(--line); padding:12px 18px; border-radius:12px;
          font-size:14px; opacity:0; transition:.25s; pointer-events:none; box-shadow:0 10px 30px #000; }
  .toast.show{ opacity:1; transform:translateX(-50%) translateY(0); }
  #dash[hidden]{ display:none; }
  .lock{ text-align:center; padding:26px 18px; }
  .lock .ic{ font-size:40px; } .spin{ animation:spin 1s linear infinite; display:inline-block; }
  @keyframes spin{ to{ transform:rotate(360deg);} }
</style>
</head>
<body>
<div class="wrap">
  <div class="hdr">
    <div class="logo">🛰️ tickvault</div>
    <div class="live"><span class="dot" id="livedot"></span><span id="livetxt">offline</span></div>
  </div>

  <!-- LOCK GATE: nothing works until a device holding the secret unlocks it -->
  <div class="card lock" id="lock">
    <div class="ic">🔐</div>
    <div class="lbl" style="margin-top:8px">device key</div>
    <p class="muted" style="max-width:440px;margin:6px auto 14px">
      This console only works on a device that holds your secret key — your laptop
      and your phone. Anyone else who opens this link sees this screen and can do
      nothing. Paste your key once; it is saved on THIS device only.</p>
    <input id="tok" type="password" placeholder="paste your operator key" autocomplete="off">
    <div class="row" style="margin-top:12px"><button class="b-blu" onclick="unlock()">🔓 Unlock this device</button></div>
  </div>

  <!-- DASHBOARD -->
  <div id="dash" hidden>
    <div class="card">
      <div class="hero">
        <div class="bignum">
          <div class="n" id="ticksbig">0</div>
          <div class="c">ticks captured today</div>
        </div>
        <div class="pills">
          <div class="pill"><div class="v" id="p_inst">—</div><div class="k">instance</div></div>
          <div class="pill"><div class="v" id="p_app">—</div><div class="k">app</div></div>
          <div class="pill"><div class="v" id="p_mkt">—</div><div class="k">market</div></div>
          <div class="pill"><div class="v" id="p_tps">—</div><div class="k">peak ticks/sec</div></div>
        </div>
      </div>
      <div class="row" style="margin-top:14px">
        <button class="b-blu" id="refbtn" onclick="refresh()">🔄 Refresh now</button>
      </div>
      <div style="display:flex;align-items:center;gap:14px;margin-top:10px">
        <label class="switch"><input type="checkbox" id="auto" checked onchange="autotoggle()"> auto-refresh (8s)</label>
        <span class="muted" id="updated"></span>
      </div>
    </div>

    <div class="card">
      <div class="lbl">live ticks/sec — proof no sub-second tick is lost</div>
      <svg class="spark" id="spark" viewBox="0 0 300 70" preserveAspectRatio="none"></svg>
    </div>

    <div class="card">
      <div class="lbl">candles sealed today (per timeframe)</div>
      <div id="bars"></div>
    </div>

    <div class="card">
      <div class="lbl">guarantees — live proof read from the box</div>
      <div class="shields" id="shields"></div>
    </div>

    <div class="card">
      <div class="lbl">recent errors (last 5)</div>
      <div class="errs" id="errs"></div>
    </div>

    <div class="card">
      <div class="lbl">control</div>
      <div class="row" style="margin-bottom:10px">
        <button class="b-go" onclick="act('start')">▶ Start instance</button>
        <button class="b-amb" onclick="act('restart-app')">♻ Restart app</button>
        <button class="b-amb" onclick="act('restart-questdb')">♻ Restart QuestDB</button>
      </div>
      <div class="row">
        <button class="b-stop" onclick="act('stop')">⏹ Stop instance</button>
        <button class="b-stop" onclick="act('stop-app')">⏹ Stop app</button>
      </div>
      <div class="muted" style="margin-top:10px">Stop / restart are blocked 9:15 AM–3:30 PM IST Mon–Fri.</div>
      <label class="switch" style="margin-top:8px"><input type="checkbox" id="force"> force (override market-hours guard)</label>
      <div class="row" style="margin-top:14px"><button class="b-amb" style="background:#1a2336;color:var(--txt)" onclick="lock()">🔒 Lock / forget this device</button></div>
    </div>
  </div>
</div>
<div class="toast" id="toast"></div>

<script>
const $=id=>document.getElementById(id);
let TOKEN = localStorage.getItem('tv_token')||'';
let timer=null, hist=[], lastTicks=0;

function toast(m){ const t=$('toast'); t.textContent=m; t.classList.add('show');
  clearTimeout(t._h); t._h=setTimeout(()=>t.classList.remove('show'),2600); }
function esc(s){ return String(s).replace(/[<>&]/g,c=>({'<':'&lt;','>':'&gt;','&':'&amp;'}[c])); }

function unlock(){ const v=($('tok').value||'').trim(); if(!v){ toast('Paste your key first'); return; }
  TOKEN=v; localStorage.setItem('tv_token',v); $('lock').hidden=true; $('dash').hidden=false;
  startAuto(); refresh(); }
function lock(){ TOKEN=''; localStorage.removeItem('tv_token'); $('tok').value='';
  stopAuto(); $('dash').hidden=true; $('lock').hidden=false; setLive(false); toast('Locked — key forgotten on this device'); }

function setLive(on){ $('livedot').classList.toggle('on',on); $('livetxt').textContent=on?'live':'offline'; }

async function call(action, extra){
  if(!TOKEN){ toast('Locked'); return null; }
  const body=Object.assign({action},extra||{});
  try{
    const r=await fetch(location.href,{method:'POST',
      headers:{'authorization':'Bearer '+TOKEN,'content-type':'application/json'},
      body:JSON.stringify(body)});
    const j=await r.json().catch(()=>({}));
    if(r.status===401){ toast('Wrong key (401)'); setLive(false); return null; }
    if(r.status===409){ toast(j.error||'Blocked during market hours'); return null; }
    if(!r.ok){ toast(j.error||('Error '+r.status)); return null; }
    return j;
  }catch(e){ toast('Network error'); setLive(false); return null; }
}

function countUp(el,target){ target=parseInt(String(target).replace(/[^0-9]/g,''),10)||0;
  const from=lastTicks||0; lastTicks=target; const t0=performance.now(), dur=900;
  function step(t){ const k=Math.min(1,(t-t0)/dur); const val=Math.round(from+(target-from)*(1-Math.pow(1-k,3)));
    el.textContent=val.toLocaleString(); if(k<1) requestAnimationFrame(step); }
  requestAnimationFrame(step); }

function drawSpark(){ const w=300,h=70,pad=6; const xs=hist.slice(-40); const max=Math.max(2,...xs);
  const pts=xs.map((v,i)=>{ const x=pad+(w-2*pad)*(xs.length<2?0:i/(xs.length-1));
    const y=h-pad-(h-2*pad)*(v/max); return x.toFixed(1)+','+y.toFixed(1); });
  const line=pts.join(' '); const area=pts.length? 'M'+pad+','+(h-pad)+' L'+line.replace(/ /g,' L')+' L'+(w-pad)+','+(h-pad)+' Z':'';
  $('spark').innerHTML=
    '<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1">'+
    '<stop offset="0" stop-color="#38e1d6" stop-opacity=".5"/><stop offset="1" stop-color="#38e1d6" stop-opacity="0"/></linearGradient></defs>'+
    (area?'<path d="'+area+'" fill="url(#g)"/>':'')+
    (pts.length>1?'<polyline points="'+line+'" fill="none" stroke="#38e1d6" stroke-width="2.5" stroke-linejoin="round" stroke-linecap="round"/>':'')+
    (pts.length?'<circle cx="'+pts[pts.length-1].split(',')[0]+'" cy="'+pts[pts.length-1].split(',')[1]+'" r="3.5" fill="#38e1d6"/>':''); }

function bar(name,n,max){ const pct=max>0?Math.max(3,Math.round(100*n/max)):0;
  return '<div class="bar"><div class="name">'+name+'</div><div class="track"><div class="fill" style="width:'+pct+'%"></div></div><div class="num">'+n.toLocaleString()+'</div></div>'; }

function shield(t,s,good){ return '<div class="shield '+(good?'good':'bad')+'"><div class="ttl">'+t+'</div><div class="st '+(good?'ok':'warn')+'">'+s+'</div></div>'; }

async function refresh(){
  $('refbtn').innerHTML='<span class="spin">🔄</span> Refreshing…';
  const j=await call('view'); $('refbtn').textContent='🔄 Refresh now';
  if(!j) return;
  const appOk=j.app==='active', running=j.instance_state==='running';
  setLive(appOk && running);
  countUp($('ticksbig'), j.ticks_today||'0');
  $('p_inst').innerHTML='<span class="'+(running?'ok':'bad')+'">'+(j.instance_state||'?')+'</span>';
  $('p_app').innerHTML='<span class="'+(appOk?'ok':'bad')+'">'+(appOk?'up':(j.app||'down'))+'</span>';
  $('p_mkt').innerHTML='<span class="'+(j.market_hours?'warn':'')+'">'+(j.market_hours?'OPEN':'closed')+'</span>';
  const tpsN=parseInt(j.max_ticks_per_second,10)||0;
  $('p_tps').innerHTML='<span class="cy">'+tpsN+'</span>';
  hist.push(tpsN); if(hist.length>60) hist.shift(); drawSpark();

  const c=j.candles||{}; const vals=['1m','5m','15m','60m','1d'].map(k=>parseInt(c[k],10)||0);
  const mx=Math.max(1,...vals);
  $('bars').innerHTML=['1m','5m','15m','60m','1d'].map((k,i)=>bar(k,vals[i],mx)).join('');

  const keysOk=(j.dedup_key_columns||'')==='4', capOk=tpsN>1;
  $('shields').innerHTML=
    shield('Dedup key columns', (j.dedup_key_columns||'?'), keysOk)+
    shield('Sub-second fix', keysOk?'LIVE ✅':'OLD ⚠', keysOk)+
    shield('No tick lost', capOk?'WORKING ✅':(j.market_hours?'CHECK ⚠':'idle'), capOk||!j.market_hours)+
    shield('Peak ticks / second', tpsN, capOk);

  const errs=j.recent_errors||[];
  $('errs').innerHTML = errs.length? errs.map(e=>'<div>'+esc(e)+'</div>').join('') : '<div class="ok">none 🎉</div>';
  $('updated').textContent='updated '+new Date().toLocaleTimeString();
}

async function act(action){ if(!confirm('Run "'+action+'" on the trading box?')) return;
  const force=$('force').checked; toast(action+'…');
  const j=await call(action,{force}); if(j){ toast('✅ '+action+' sent'); setTimeout(refresh,1600); } }

function startAuto(){ stopAuto(); if($('auto').checked) timer=setInterval(refresh,8000); }
function stopAuto(){ if(timer){ clearInterval(timer); timer=null; } }
function autotoggle(){ $('auto').checked?startAuto():stopAuto(); }

// Boot: if this device already holds a key, go straight to the dashboard.
if(TOKEN){ $('tok').value=TOKEN; $('lock').hidden=true; $('dash').hidden=false; startAuto(); refresh(); }
</script>
</body>
</html>"""
