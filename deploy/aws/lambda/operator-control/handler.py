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
    token the operator types is kept ONLY in the browser's localStorage and
    sent as `Authorization: Bearer <token>` on every POST to this same URL."""
    return """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>tickvault — operator console</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin:0; font-family:-apple-system,Segoe UI,Roboto,sans-serif;
         background:#0b0e14; color:#e6e6e6; padding:16px; max-width:760px; margin:0 auto; }
  h1 { font-size:20px; margin:8px 0 4px; }
  .sub { color:#8a93a6; font-size:13px; margin-bottom:16px; }
  .card { background:#141925; border:1px solid #232a3a; border-radius:12px;
          padding:14px; margin-bottom:14px; }
  label { font-size:12px; color:#8a93a6; display:block; margin-bottom:6px; }
  input[type=password] { width:100%; padding:10px; border-radius:8px; border:1px solid #2a3346;
          background:#0b0e14; color:#e6e6e6; font-size:14px; }
  .row { display:flex; gap:8px; flex-wrap:wrap; }
  button { flex:1 1 auto; min-width:120px; padding:12px; border-radius:10px; border:0;
           font-size:14px; font-weight:600; cursor:pointer; color:#0b0e14; }
  .b-view { background:#4f8cff; color:#fff; }
  .b-go { background:#33c47d; }
  .b-warn { background:#f0a93b; }
  .b-stop { background:#ff5d5d; color:#fff; }
  .grid { display:grid; grid-template-columns:repeat(auto-fit,minmax(110px,1fr)); gap:10px; }
  .metric { background:#0b0e14; border:1px solid #232a3a; border-radius:10px; padding:10px; }
  .metric .v { font-size:20px; font-weight:700; }
  .metric .k { font-size:11px; color:#8a93a6; margin-top:2px; }
  .ok { color:#33c47d; } .bad { color:#ff5d5d; } .warn { color:#f0a93b; }
  pre { background:#0b0e14; border:1px solid #232a3a; border-radius:10px; padding:10px;
        overflow:auto; font-size:12px; white-space:pre-wrap; max-height:200px; }
  .muted { color:#8a93a6; font-size:12px; }
  .toast { position:fixed; left:50%; bottom:20px; transform:translateX(-50%);
           background:#232a3a; padding:10px 16px; border-radius:8px; font-size:13px;
           opacity:0; transition:opacity .2s; pointer-events:none; }
  .toast.show { opacity:1; }
</style>
</head>
<body>
  <h1>🛰️ tickvault operator console</h1>
  <div class="sub">One page. View the box and control it. Token stays on this device only.</div>

  <div class="card">
    <label>Operator token (Bearer secret — saved on this device)</label>
    <input id="tok" type="password" placeholder="paste token" autocomplete="off">
    <div class="muted" style="margin-top:6px">Header sent: <code>Authorization: Bearer &lt;token&gt;</code></div>
  </div>

  <div class="card">
    <div class="row">
      <button class="b-view" onclick="refresh()">🔄 Refresh status</button>
    </div>
    <div class="grid" id="metrics" style="margin-top:12px"></div>
    <div class="muted" id="updated" style="margin-top:8px"></div>
  </div>

  <div class="card">
    <label>Recent errors (last 5)</label>
    <pre id="errors">—</pre>
  </div>

  <div class="card">
    <label>Control</label>
    <div class="row" style="margin-bottom:8px">
      <button class="b-go" onclick="act('start')">▶ Start instance</button>
      <button class="b-warn" onclick="act('restart-app')">♻ Restart app</button>
      <button class="b-warn" onclick="act('restart-questdb')">♻ Restart QuestDB</button>
    </div>
    <div class="row">
      <button class="b-stop" onclick="act('stop')">⏹ Stop instance</button>
      <button class="b-stop" onclick="act('stop-app')">⏹ Stop app</button>
    </div>
    <div class="muted" style="margin-top:8px">Stop / restart are blocked 9:15 AM–3:30 PM IST Mon–Fri.
      Tick “force” to override.</div>
    <label style="margin-top:8px"><input type="checkbox" id="force"> force (override market-hours guard)</label>
  </div>

  <div class="toast" id="toast"></div>

<script>
const TOK = document.getElementById('tok');
TOK.value = localStorage.getItem('tv_token') || '';
TOK.addEventListener('input', () => localStorage.setItem('tv_token', TOK.value));

function toast(msg){ const t=document.getElementById('toast'); t.textContent=msg;
  t.classList.add('show'); setTimeout(()=>t.classList.remove('show'),2600); }

async function call(action, extra){
  const token = TOK.value.trim();
  if(!token){ toast('Enter the token first'); return null; }
  const body = Object.assign({action}, extra||{});
  try {
    const r = await fetch(location.href, { method:'POST',
      headers:{'authorization':'Bearer '+token,'content-type':'application/json'},
      body: JSON.stringify(body) });
    const j = await r.json().catch(()=>({}));
    if(r.status===401){ toast('Wrong token (401)'); return null; }
    if(r.status===409){ toast(j.error||'Blocked during market hours'); return null; }
    if(!r.ok){ toast(j.error||('Error '+r.status)); return null; }
    return j;
  } catch(e){ toast('Network error'); return null; }
}

function metric(k,v,cls){ return `<div class="metric"><div class="v ${cls||''}">${v}</div><div class="k">${k}</div></div>`; }

async function refresh(){
  toast('Loading…');
  const j = await call('view');
  if(!j) return;
  const appOk = j.app==='active';
  const c = j.candles||{};
  document.getElementById('metrics').innerHTML =
    metric('Instance', j.instance_state||'?', j.instance_state==='running'?'ok':'bad') +
    metric('App', appOk?'up':(j.app||'down'), appOk?'ok':'bad') +
    metric('Ticks today', j.ticks_today||'0') +
    metric('1m candles', c['1m']||'0') +
    metric('5m candles', c['5m']||'0') +
    metric('15m candles', c['15m']||'0') +
    metric('60m candles', c['60m']||'0') +
    metric('1d candles', c['1d']||'0') +
    metric('Market', j.market_hours?'OPEN':'closed', j.market_hours?'warn':'');
  const errs = (j.recent_errors||[]);
  document.getElementById('errors').textContent = errs.length? errs.join('\\n') : 'none 🎉';
  document.getElementById('updated').textContent = 'Updated '+new Date().toLocaleTimeString();
}

async function act(action){
  if(!confirm('Run "'+action+'" on the trading box?')) return;
  const force = document.getElementById('force').checked;
  toast(action+'…');
  const j = await call(action, {force});
  if(j){ toast('✅ '+action+' sent'); setTimeout(refresh, 1500); }
}

if(TOK.value) refresh();
</script>
</body>
</html>"""
