#!/usr/bin/env python3
"""tickvault-logs MCP server — Phase 7.2 of .claude/plans/active-plan.md.

Exposes typed, structured access to the zero-touch observability
artefacts (errors.jsonl, errors.summary.md, auto-fix.log,
triage-seen.jsonl) over the Model Context Protocol stdio transport.

Design goals:
  - Zero pip dependencies — stdlib only (json, pathlib, datetime, sys,
    os). Works on any Python 3.10+ without `pip install`.
  - O(1) call surface from Claude's side: one tool per question,
    structured JSON out, no regex scraping of free-text logs.
  - Safe by default: never writes, never mutates; strict read-only.

MCP protocol: implements the minimum subset required for Claude Code
to enumerate tools and invoke them via JSON-RPC 2.0 over stdin/stdout.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Iterable


# ---------------------------------------------------------------------------
# Configuration — universal, branch-independent endpoints
# ---------------------------------------------------------------------------
#
# Resolution order for every endpoint URL (prom/questdb/alertmanager/...):
#   1. Explicit `base_url` argument passed to the tool (single-call override)
#   2. Env var TICKVAULT_<KIND>_URL (session-level override)
#   3. Active profile in config/claude-mcp-endpoints.toml (universal default)
#   4. Hardcoded localhost default (Mode A — everything on 127.0.0.1)
#
# The config file is committed to the repo so every clone on every branch
# inherits working endpoints without per-session setup.

_ENDPOINTS_CONFIG_FILENAME = "claude-mcp-endpoints.toml"


def _repo_root() -> Path:
    here = Path(__file__).resolve()
    # scripts/mcp-servers/tickvault-logs/server.py -> repo root
    return here.parent.parent.parent.parent


def _endpoints_config_path() -> Path:
    override = os.environ.get("TICKVAULT_MCP_ENDPOINTS_CONFIG")
    if override:
        return Path(override)
    return _repo_root() / "config" / _ENDPOINTS_CONFIG_FILENAME


_ENDPOINTS_CACHE: dict[str, Any] | None = None


def _load_endpoints_config() -> dict[str, Any]:
    """Parse config/claude-mcp-endpoints.toml once per process.

    Returns {"active": <profile>, "profiles": {<name>: {<kind>: <url>}}}.
    Falls back to {"active": "local", "profiles": {}} if the file is
    missing or unparseable — we never want the MCP server to crash just
    because the config file is absent.
    """
    global _ENDPOINTS_CACHE
    if _ENDPOINTS_CACHE is not None:
        return _ENDPOINTS_CACHE
    path = _endpoints_config_path()
    result: dict[str, Any] = {"active": "local", "profiles": {}}
    if not path.is_file():
        _ENDPOINTS_CACHE = result
        return result
    try:
        import tomllib  # Python 3.11+
        with path.open("rb") as fh:
            parsed = tomllib.load(fh)
    except Exception:  # noqa: BLE001
        # Best-effort: if the file is malformed, fall back to defaults
        # instead of crashing the MCP server.
        _ENDPOINTS_CACHE = result
        return result
    if isinstance(parsed.get("active"), str):
        result["active"] = parsed["active"]
    profiles = parsed.get("profiles", {})
    if isinstance(profiles, dict):
        for name, cfg in profiles.items():
            if isinstance(cfg, dict):
                result["profiles"][name] = cfg
    _ENDPOINTS_CACHE = result
    return result


def _active_profile() -> str:
    override = os.environ.get("TICKVAULT_MCP_PROFILE")
    if override:
        return override
    return _load_endpoints_config().get("active", "local")


def _endpoint_url(
    kind: str,
    env_var: str,
    default: str,
    explicit: str | None = None,
) -> str:
    """Resolve an endpoint URL via the 4-tier precedence order.

    `kind` is the key in the profile config (e.g. "prometheus_url").
    `env_var` is the legacy env-var override (e.g. "TICKVAULT_PROMETHEUS_URL").
    `default` is the Mode A localhost fallback.
    `explicit` is a single-call override from the tool invocation.
    """
    if explicit:
        return explicit
    from_env = os.environ.get(env_var)
    if from_env:
        return from_env
    profile = _active_profile()
    profile_cfg = _load_endpoints_config().get("profiles", {}).get(profile, {})
    from_config = profile_cfg.get(kind)
    if isinstance(from_config, str) and from_config:
        return from_config
    return default


def _logs_dir() -> Path:
    override = os.environ.get("TICKVAULT_LOGS_DIR")
    if override:
        return Path(override)
    profile = _active_profile()
    profile_cfg = _load_endpoints_config().get("profiles", {}).get(profile, {})
    from_config = profile_cfg.get("logs_dir_local")
    if isinstance(from_config, str) and from_config:
        cfg_path = Path(from_config)
        if not cfg_path.is_absolute():
            cfg_path = _repo_root() / cfg_path
        return cfg_path
    return _repo_root() / "data" / "logs"


def _logs_source() -> str:
    """Return "http" or "local" — how the MCP should fetch log files."""
    override = os.environ.get("TICKVAULT_LOGS_SOURCE")
    if override in {"http", "local"}:
        return override
    profile = _active_profile()
    profile_cfg = _load_endpoints_config().get("profiles", {}).get(profile, {})
    source = profile_cfg.get("logs_source")
    if source in {"http", "local"}:
        return source
    return "local"


def _state_dir() -> Path:
    return _repo_root() / ".claude" / "state"


ERRORS_JSONL_PREFIX = "errors.jsonl"
SUMMARY_FILENAME = "errors.summary.md"
AUTO_FIX_LOG = "auto-fix.log"
TRIAGE_SEEN = "triage-seen.jsonl"


# ---------------------------------------------------------------------------
# Tool implementations
# ---------------------------------------------------------------------------


def _iter_errors_jsonl_files(dir_path: Path) -> list[Path]:
    """Newest-first list of errors.jsonl.* files under dir_path."""
    if not dir_path.is_dir():
        return []
    out: list[Path] = []
    for entry in dir_path.iterdir():
        if entry.is_file() and entry.name.startswith(ERRORS_JSONL_PREFIX):
            out.append(entry)
    out.sort(key=lambda p: p.name, reverse=True)
    return out


def _parse_event(line: str) -> dict[str, Any] | None:
    line = line.strip()
    if not line:
        return None
    try:
        return json.loads(line)
    except json.JSONDecodeError:
        return None


def _signature_hash(code: str | None, target: str, message: str) -> str:
    """Same FNV-1a signature the Rust summary_writer uses — 16 hex chars."""
    fnv_offset = 0xCBF29CE484222325
    fnv_prime = 0x100000001B3
    mask = 0xFFFFFFFFFFFFFFFF
    h = fnv_offset
    truncated = (message or "")[:160]
    feed = "|".join(
        [code or "", target or "", truncated]
    ).encode("utf-8")
    # Re-split to match Rust's feed order: code, "|", target, "|", msg.
    # Both Python and Rust produce the same digest because we replicate
    # the sequence of bytes exactly.
    h = fnv_offset
    parts: list[bytes] = [
        (code or "").encode("utf-8"),
        b"|",
        (target or "").encode("utf-8"),
        b"|",
        truncated.encode("utf-8"),
    ]
    for part in parts:
        for b in part:
            h ^= b
            h = (h * fnv_prime) & mask
    return f"{h:016x}"


def tool_tail_errors(
    limit: int = 100, code: str | None = None
) -> dict[str, Any]:
    """Last `limit` ERROR events across errors.jsonl.* files.

    If `code` is given, only events with that `code` field are returned.
    Newest-first ordering.
    """
    dir_path = _logs_dir()
    files = _iter_errors_jsonl_files(dir_path)
    events: list[dict[str, Any]] = []
    for f in files:
        try:
            lines = f.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for line in reversed(lines):
            ev = _parse_event(line)
            if ev is None:
                continue
            if code is not None and ev.get("code") != code:
                continue
            events.append(ev)
            if len(events) >= limit:
                break
        if len(events) >= limit:
            break
    return {
        "dir": str(dir_path),
        "count": len(events),
        "files_scanned": [str(f.name) for f in files],
        "events": events,
    }


def tool_list_novel_signatures(since_minutes: int = 60) -> dict[str, Any]:
    """Signatures first seen within the last `since_minutes` minutes.

    A signature is "novel" if it has NOT been observed before the window
    starts. Uses the signature_hash (FNV-1a of code + target + first-160-
    chars-of-message) — identical to the Rust summary_writer.
    """
    dir_path = _logs_dir()
    files = _iter_errors_jsonl_files(dir_path)
    cutoff = datetime.now(tz=timezone.utc) - timedelta(minutes=since_minutes)

    first_seen: dict[str, dict[str, Any]] = {}
    for f in files:
        try:
            text = f.read_text(encoding="utf-8")
        except OSError:
            continue
        for line in text.splitlines():
            ev = _parse_event(line)
            if ev is None:
                continue
            sig = _signature_hash(
                ev.get("code"),
                ev.get("target") or "",
                ev.get("message") or "",
            )
            ts_str = ev.get("timestamp")
            ts: datetime | None = None
            if ts_str:
                try:
                    # strip trailing 'Z' and parse as fromisoformat
                    normalised = ts_str.replace("Z", "+00:00")
                    ts = datetime.fromisoformat(normalised)
                except ValueError:
                    ts = None
            if sig not in first_seen or (
                ts is not None
                and first_seen[sig].get("first_seen_ts") is not None
                and ts < first_seen[sig]["first_seen_ts"]
            ):
                first_seen[sig] = {
                    "signature": sig,
                    "code": ev.get("code"),
                    "severity": ev.get("severity"),
                    "target": ev.get("target"),
                    "message": (ev.get("message") or "")[:200],
                    "first_seen_ts": ts,
                }

    novel: list[dict[str, Any]] = []
    for sig, info in first_seen.items():
        ts = info.get("first_seen_ts")
        if ts is not None and ts >= cutoff:
            record = dict(info)
            record["first_seen_ts"] = ts.isoformat()
            novel.append(record)

    return {
        "dir": str(dir_path),
        "since_minutes": since_minutes,
        "cutoff_utc": cutoff.isoformat(),
        "novel_count": len(novel),
        "novel": novel,
    }


def tool_summary_snapshot() -> dict[str, Any]:
    """Returns the current errors.summary.md contents as a string."""
    path = _logs_dir() / SUMMARY_FILENAME
    if not path.exists():
        return {
            "path": str(path),
            "exists": False,
            "markdown": "",
        }
    try:
        markdown = path.read_text(encoding="utf-8")
    except OSError as err:
        return {
            "path": str(path),
            "exists": True,
            "error": str(err),
            "markdown": "",
        }
    return {
        "path": str(path),
        "exists": True,
        "markdown": markdown,
        "line_count": markdown.count("\n"),
    }


def tool_triage_log_tail(limit: int = 50) -> dict[str, Any]:
    """Last `limit` lines of data/logs/auto-fix.log."""
    path = _logs_dir() / AUTO_FIX_LOG
    if not path.exists():
        return {"path": str(path), "exists": False, "lines": []}
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as err:
        return {"path": str(path), "exists": True, "error": str(err), "lines": []}
    lines = text.splitlines()
    tail = lines[-limit:] if len(lines) > limit else lines
    return {
        "path": str(path),
        "exists": True,
        "total_lines": len(lines),
        "returned": len(tail),
        "lines": tail,
    }


def tool_signature_history(signature: str, limit: int = 500) -> dict[str, Any]:
    """All events whose computed signature hash equals `signature`."""
    dir_path = _logs_dir()
    files = _iter_errors_jsonl_files(dir_path)
    matches: list[dict[str, Any]] = []
    for f in files:
        try:
            lines = f.read_text(encoding="utf-8").splitlines()
        except OSError:
            continue
        for line in lines:
            ev = _parse_event(line)
            if ev is None:
                continue
            sig = _signature_hash(
                ev.get("code"),
                ev.get("target") or "",
                ev.get("message") or "",
            )
            if sig == signature:
                matches.append(ev)
                if len(matches) >= limit:
                    break
        if len(matches) >= limit:
            break
    return {
        "signature": signature,
        "count": len(matches),
        "events": matches,
    }


# ---------------------------------------------------------------------------
# Tool registry — the MCP advertised tools
# ---------------------------------------------------------------------------


def tool_prometheus_query(query: str, base_url: str | None = None) -> dict[str, Any]:
    """Run an instant PromQL query against the local Prometheus.

    Returns the raw API response as a dict. Lets Claude ask live
    questions like "what's tv_ticks_dropped_total right now?" without
    needing shell access or curl + jq chains.
    """
    import urllib.parse
    import urllib.request

    prom_url = _endpoint_url(
        "prometheus_url",
        "TICKVAULT_PROMETHEUS_URL",
        "http://127.0.0.1:9090",
        explicit=base_url,
    )
    full = f"{prom_url}/api/v1/query?query={urllib.parse.quote(query)}"
    try:
        with urllib.request.urlopen(full, timeout=5) as resp:  # noqa: S310
            body = resp.read().decode("utf-8")
        return {"ok": True, "query": query, "response": json.loads(body)}
    except Exception as err:  # noqa: BLE001
        return {"ok": False, "query": query, "error": str(err)}


def tool_find_runbook_for_code(code: str) -> dict[str, Any]:
    """Given an ErrorCode string (e.g. "DH-904"), find the runbook(s)
    that reference it and return the full markdown path + a preview
    of the relevant section.

    Lets Claude jump from a Telegram alert to the operator runbook in
    one tool call, without path-guessing.
    """
    here = Path(__file__).resolve()
    root = here.parent.parent.parent.parent
    runbooks_dir = root / "docs" / "runbooks"
    rules_dir = root / ".claude" / "rules"

    matches: list[dict[str, Any]] = []
    for search_dir in [runbooks_dir, rules_dir]:
        if not search_dir.exists():
            continue
        for md in search_dir.rglob("*.md"):
            try:
                text = md.read_text(encoding="utf-8")
            except OSError:
                continue
            if code in text:
                # Grab up to 3 lines before and after the first mention.
                lines = text.splitlines()
                for idx, line in enumerate(lines):
                    if code in line:
                        start = max(0, idx - 2)
                        end = min(len(lines), idx + 4)
                        preview = "\n".join(lines[start:end])
                        matches.append(
                            {
                                "file": str(md.relative_to(root)),
                                "first_line": idx + 1,
                                "preview": preview,
                            }
                        )
                        break

    return {
        "code": code,
        "match_count": len(matches),
        "matches": matches,
    }


def tool_questdb_sql(query: str, base_url: str | None = None) -> dict[str, Any]:
    """Run an arbitrary SQL query against QuestDB via the HTTP /exec
    endpoint. Returns the columnar JSON response.

    Gives Claude read/write access to the trading data plane — the
    `ticks` table, `orders`, `historical_candles`, materialized views,
    instrument snapshots, etc. — without needing the pg CLI.

    WARNING: this tool can execute DDL (DROP TABLE etc.) on a live
    QuestDB. The caller is responsible for query safety. In production,
    consider wrapping with a read-only user.
    """
    import urllib.parse
    import urllib.request

    qdb = _endpoint_url(
        "questdb_url",
        "TICKVAULT_QUESTDB_URL",
        "http://127.0.0.1:9000",
        explicit=base_url,
    )
    full = f"{qdb}/exec?query={urllib.parse.quote(query)}"
    try:
        with urllib.request.urlopen(full, timeout=15) as resp:  # noqa: S310
            body = resp.read().decode("utf-8")
        return {"ok": True, "query": query, "response": json.loads(body)}
    except Exception as err:  # noqa: BLE001
        return {"ok": False, "query": query, "error": str(err)}


def tool_grep_codebase(
    pattern: str,
    path: str | None = None,
    file_glob: str | None = None,
    max_matches: int = 200,
) -> dict[str, Any]:
    """Regex-search the repo for `pattern`. Uses the stdlib `re` module
    so no ripgrep dependency. Returns a list of {file, line, text}
    matches.

    Lets Claude find any function, error string, config key, or test
    name across the workspace in one tool call.
    """
    import re

    here = Path(__file__).resolve()
    root = here.parent.parent.parent.parent
    search_root = (root / path) if path else root

    # Compile once
    try:
        regex = re.compile(pattern)
    except re.error as err:
        return {"ok": False, "pattern": pattern, "error": f"invalid regex: {err}"}

    matches: list[dict[str, Any]] = []
    skip_dirs = {"target", ".git", "node_modules", "data", ".terraform"}

    for dirpath, dirnames, filenames in os.walk(search_root):
        dirnames[:] = [d for d in dirnames if d not in skip_dirs]
        for name in filenames:
            if file_glob:
                import fnmatch
                if not fnmatch.fnmatch(name, file_glob):
                    continue
            full_path = Path(dirpath) / name
            # Skip binaries + huge files
            try:
                if full_path.stat().st_size > 2_000_000:
                    continue
            except OSError:
                continue
            try:
                with open(full_path, "r", encoding="utf-8", errors="ignore") as f:
                    for idx, line in enumerate(f, 1):
                        if regex.search(line):
                            matches.append(
                                {
                                    "file": str(full_path.relative_to(root)),
                                    "line": idx,
                                    "text": line.rstrip("\n")[:500],
                                }
                            )
                            if len(matches) >= max_matches:
                                break
            except OSError:
                continue
        if len(matches) >= max_matches:
            break

    return {
        "ok": True,
        "pattern": pattern,
        "match_count": len(matches),
        "truncated": len(matches) >= max_matches,
        "matches": matches,
    }


def tool_run_doctor() -> dict[str, Any]:
    """Run `bash scripts/doctor.sh` and return the parsed output as
    JSON. Gives Claude a one-call total-system health check.

    Runs with a 120s timeout. If doctor hangs (bug), this returns
    {"ok": false, "error": "timeout"} instead of blocking Claude.
    """
    import subprocess

    here = Path(__file__).resolve()
    root = here.parent.parent.parent.parent
    try:
        out = subprocess.run(
            ["bash", "scripts/doctor.sh"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=120,
        )
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "doctor.sh timed out after 120s"}
    except FileNotFoundError as err:
        return {"ok": False, "error": f"bash not available: {err}"}

    # Parse [PASS]/[FAIL]/[WARN]/[SKIP] lines into structured rows.
    rows: list[dict[str, str]] = []
    pass_count = 0
    fail_count = 0
    for line in out.stdout.splitlines():
        line = line.rstrip()
        for status in ("[PASS]", "[FAIL]", "[WARN]", "[SKIP]"):
            if line.startswith(status):
                rest = line[len(status):].strip()
                rows.append({"status": status.strip("[]"), "detail": rest})
                if status == "[PASS]":
                    pass_count += 1
                elif status == "[FAIL]":
                    fail_count += 1
                break

    return {
        "ok": out.returncode == 0,
        "exit_code": out.returncode,
        "pass_count": pass_count,
        "fail_count": fail_count,
        "rows": rows,
        "raw_stdout_tail": out.stdout[-2000:] if out.stdout else "",
    }


def tool_git_recent_log(limit: int = 20) -> dict[str, Any]:
    """Last `limit` commits on the current branch with subject +
    author + ISO date. Lets Claude investigate "what changed recently"
    without leaving the MCP surface.
    """
    import subprocess

    here = Path(__file__).resolve()
    root = here.parent.parent.parent.parent
    fmt = "%H%x1f%an%x1f%aI%x1f%s"
    try:
        out = subprocess.run(
            ["git", "log", f"-{int(limit)}", f"--pretty=format:{fmt}"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "git log timed out"}
    except FileNotFoundError:
        return {"ok": False, "error": "git not available"}
    if out.returncode != 0:
        return {"ok": False, "error": out.stderr.strip()}
    commits: list[dict[str, str]] = []
    for line in out.stdout.splitlines():
        parts = line.split("\x1f")
        if len(parts) != 4:
            continue
        commits.append(
            {
                "sha": parts[0][:12],
                "author": parts[1],
                "date": parts[2],
                "subject": parts[3],
            }
        )
    return {"ok": True, "count": len(commits), "commits": commits}


def tool_list_active_alerts(base_url: str | None = None) -> dict[str, Any]:
    """List every active (firing) Alertmanager alert. Gives Claude a
    live view of WHAT is currently broken without parsing
    errors.summary.md (the summary_writer lags up to 60s).
    """
    import urllib.request

    am_url = _endpoint_url(
        "alertmanager_url",
        "TICKVAULT_ALERTMANAGER_URL",
        "http://127.0.0.1:9093",
        explicit=base_url,
    )
    full = f"{am_url}/api/v2/alerts?active=true&silenced=false&inhibited=false"
    try:
        with urllib.request.urlopen(full, timeout=5) as resp:  # noqa: S310
            body = resp.read().decode("utf-8")
        parsed = json.loads(body)
    except Exception as err:  # noqa: BLE001
        return {"ok": False, "error": str(err)}
    summary: list[dict[str, Any]] = []
    for alert in parsed:
        labels = alert.get("labels") or {}
        annotations = alert.get("annotations") or {}
        summary.append(
            {
                "alertname": labels.get("alertname"),
                "severity": labels.get("severity"),
                "starts_at": alert.get("startsAt"),
                "summary": annotations.get("summary"),
                "runbook": annotations.get("runbook"),
            }
        )
    return {"ok": True, "active_count": len(summary), "alerts": summary}


def tool_tickvault_api(
    path: str,
    base_url: str | None = None,
) -> dict[str, Any]:
    """HTTP GET against the tickvault app's own REST API
    (`/health`, `/api/stats`, `/api/quote/{id}`, `/api/top-movers`,
    `/api/instruments/diagnostic`, `/api/option-chain`, `/api/pcr`,
    `/api/index-constituency`).

    Lets Claude inspect the running app's own state without the
    operator copy-pasting curl output.
    """
    import urllib.request

    api_url = _endpoint_url(
        "tickvault_api_url",
        "TICKVAULT_API_URL",
        "http://127.0.0.1:3001",
        explicit=base_url,
    )
    if not path.startswith("/"):
        path = f"/{path}"
    full = f"{api_url}{path}"
    try:
        with urllib.request.urlopen(full, timeout=10) as resp:  # noqa: S310
            body = resp.read().decode("utf-8")
            status = resp.status
    except Exception as err:  # noqa: BLE001
        return {"ok": False, "error": str(err), "url": full}
    # Try JSON — fall back to raw text if not JSON.
    try:
        parsed = json.loads(body)
        return {"ok": True, "status": status, "url": full, "json": parsed}
    except json.JSONDecodeError:
        return {"ok": True, "status": status, "url": full, "text": body[:4000]}


def tool_grafana_query(
    endpoint: str,
    base_url: str | None = None,
) -> dict[str, Any]:
    """HTTP GET against the Grafana HTTP API.

    `endpoint` is the path component, e.g.
      - `/api/health` — Grafana liveness
      - `/api/search?query=tickvault` — list dashboards matching a term
      - `/api/dashboards/uid/<UID>` — dashboard JSON by UID
      - `/api/alertmanager/grafana/api/v2/alerts` — Grafana-managed alerts
      - `/api/datasources/proxy/uid/<ds_uid>/api/v1/query?query=...` —
        proxy a Prometheus/other datasource query

    For anonymous Grafana (the tickvault default), unauthenticated
    reads work. For authenticated, set
    `TICKVAULT_GRAFANA_API_TOKEN` env var.
    """
    import urllib.request

    gf_url = _endpoint_url(
        "grafana_url",
        "TICKVAULT_GRAFANA_URL",
        "http://127.0.0.1:3000",
        explicit=base_url,
    )
    if not endpoint.startswith("/"):
        endpoint = f"/{endpoint}"
    full = f"{gf_url}{endpoint}"
    req = urllib.request.Request(full)
    token = os.environ.get("TICKVAULT_GRAFANA_API_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:  # noqa: S310
            body = resp.read().decode("utf-8")
            status = resp.status
    except Exception as err:  # noqa: BLE001
        return {"ok": False, "error": str(err), "url": full}
    try:
        parsed = json.loads(body)
        return {"ok": True, "status": status, "url": full, "json": parsed}
    except json.JSONDecodeError:
        return {"ok": True, "status": status, "url": full, "text": body[:4000]}


def tool_docker_status() -> dict[str, Any]:
    """Return `docker compose ps --format json` for the tickvault
    Docker stack. Shows every container's name / service / state /
    health / image without shelling into the host.
    """
    import subprocess

    repo = Path(__file__).resolve().parents[3]
    compose_file = repo / "deploy" / "docker" / "docker-compose.yml"
    if not compose_file.exists():
        return {"ok": False, "error": f"compose file not found: {compose_file}"}
    try:
        proc = subprocess.run(
            [
                "docker",
                "compose",
                "-f",
                str(compose_file),
                "ps",
                "--format",
                "json",
            ],
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except FileNotFoundError:
        return {"ok": False, "error": "docker CLI not available"}
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "docker compose ps timed out after 15s"}
    if proc.returncode != 0:
        return {
            "ok": False,
            "exit_code": proc.returncode,
            "stderr": proc.stderr.strip()[:1000],
        }
    # docker compose ps --format json prints one JSON object per line.
    containers: list[dict[str, Any]] = []
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            containers.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return {
        "ok": True,
        "count": len(containers),
        "containers": containers,
    }


def tool_app_log_tail(
    limit: int = 100,
    date: str | None = None,
) -> dict[str, Any]:
    """Return the last `limit` lines of the app.YYYY-MM-DD.log file.

    If `date` (YYYY-MM-DD) is omitted, uses today's UTC date. Useful
    for INFO/DEBUG-level boot-sequence debugging that the ERROR-only
    errors.jsonl doesn't capture.
    """
    import datetime as dt

    log_dir = _logs_dir()
    if date is None:
        date = dt.datetime.utcnow().strftime("%Y-%m-%d")
    log_file = log_dir / f"app.{date}.log"
    if not log_file.exists():
        return {
            "ok": False,
            "error": f"log file not found: {log_file}",
            "log_dir": str(log_dir),
        }
    try:
        with open(log_file, "r", encoding="utf-8", errors="replace") as fh:
            lines = fh.readlines()
    except OSError as err:
        return {"ok": False, "error": str(err), "path": str(log_file)}
    tail = lines[-int(limit):] if limit > 0 else lines
    return {
        "ok": True,
        "path": str(log_file),
        "total_lines": len(lines),
        "returned": len(tail),
        "lines": [ln.rstrip("\n") for ln in tail],
    }


@dataclass
class ToolSpec:
    name: str
    description: str
    input_schema: dict[str, Any]
    handler: Any  # callable


TOOLS: list[ToolSpec] = [
    ToolSpec(
        name="tail_errors",
        description=(
            "Return the last N ERROR events from data/logs/errors.jsonl.*. "
            "Optionally filter by `code` (e.g. 'I-P1-11', 'DH-904')."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max events to return (default 100)",
                    "default": 100,
                },
                "code": {
                    "type": "string",
                    "description": "Optional ErrorCode.code_str() filter",
                },
            },
        },
        handler=lambda args: tool_tail_errors(
            limit=int(args.get("limit", 100)),
            code=args.get("code"),
        ),
    ),
    ToolSpec(
        name="list_novel_signatures",
        description=(
            "Signatures first observed within the last N minutes. Uses the "
            "same FNV-1a signature hash as the Rust summary_writer."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "since_minutes": {
                    "type": "integer",
                    "description": "Lookback window in minutes (default 60)",
                    "default": 60,
                },
            },
        },
        handler=lambda args: tool_list_novel_signatures(
            since_minutes=int(args.get("since_minutes", 60)),
        ),
    ),
    ToolSpec(
        name="summary_snapshot",
        description=(
            "Return the current errors.summary.md markdown. "
            "Regenerated every 60s by the Rust summary_writer task."
        ),
        input_schema={"type": "object", "properties": {}},
        handler=lambda _args: tool_summary_snapshot(),
    ),
    ToolSpec(
        name="triage_log_tail",
        description=(
            "Last N lines of data/logs/auto-fix.log — audit trail of the "
            "error-triage hook's actions."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max lines to return (default 50)",
                    "default": 50,
                },
            },
        },
        handler=lambda args: tool_triage_log_tail(
            limit=int(args.get("limit", 50)),
        ),
    ),
    ToolSpec(
        name="signature_history",
        description=(
            "All events whose computed signature hash equals `signature`. "
            "Use after list_novel_signatures to drill into a specific "
            "signature's full history."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "signature": {
                    "type": "string",
                    "description": "16-hex-char signature hash",
                },
                "limit": {
                    "type": "integer",
                    "description": "Max events to return (default 500)",
                    "default": 500,
                },
            },
            "required": ["signature"],
        },
        handler=lambda args: tool_signature_history(
            signature=args["signature"],
            limit=int(args.get("limit", 500)),
        ),
    ),
    ToolSpec(
        name="prometheus_query",
        description=(
            "Run an instant PromQL query against the local Prometheus "
            "and return the raw API response. Lets Claude answer "
            "\"what's the value of tv_ticks_dropped_total right now?\" "
            "in one tool call, no shell required."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "PromQL query (e.g. 'tv_ticks_dropped_total' or 'rate(tv_ticks_processed_total[1m])')",
                },
            },
            "required": ["query"],
        },
        handler=lambda args: tool_prometheus_query(query=args["query"]),
    ),
    ToolSpec(
        name="find_runbook_for_code",
        description=(
            "Given an ErrorCode string (e.g. 'DH-904', 'I-P1-11'), "
            "search every file under docs/runbooks/ AND .claude/rules/ "
            "for the code and return matching file paths + a preview "
            "of the relevant section. Lets Claude jump from a Telegram "
            "alert to the operator runbook in one tool call."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "ErrorCode.code_str() e.g. 'DH-904', 'OMS-GAP-03'",
                },
            },
            "required": ["code"],
        },
        handler=lambda args: tool_find_runbook_for_code(code=args["code"]),
    ),
    ToolSpec(
        name="list_active_alerts",
        description=(
            "List every currently-firing Alertmanager alert (active, "
            "not silenced, not inhibited). Live view of what's broken "
            "— more up-to-date than errors.summary.md (which has a 60s "
            "refresh lag)."
        ),
        input_schema={"type": "object", "properties": {}},
        handler=lambda _args: tool_list_active_alerts(),
    ),
    ToolSpec(
        name="questdb_sql",
        description=(
            "Run arbitrary SQL against the local QuestDB (HTTP /exec). "
            "Returns columnar JSON. Lets Claude query the trading data "
            "plane — ticks, orders, historical_candles, materialized "
            "views — without pg CLI."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "SQL query (e.g. 'SELECT count() FROM ticks')",
                },
            },
            "required": ["query"],
        },
        handler=lambda args: tool_questdb_sql(query=args["query"]),
    ),
    ToolSpec(
        name="grep_codebase",
        description=(
            "Regex-search the repo for a pattern. Skips target/, .git/, "
            "node_modules/, data/, .terraform/. Returns up to 200 "
            "matches of {file, line, text}. Lets Claude find any "
            "function, error string, config key, test name across the "
            "workspace in one call."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Python regex (anchors, character classes supported)",
                },
                "path": {
                    "type": "string",
                    "description": "Optional relative path to narrow the search",
                },
                "file_glob": {
                    "type": "string",
                    "description": "Optional glob like '*.rs' to limit file types",
                },
            },
            "required": ["pattern"],
        },
        handler=lambda args: tool_grep_codebase(
            pattern=args["pattern"],
            path=args.get("path"),
            file_glob=args.get("file_glob"),
        ),
    ),
    ToolSpec(
        name="run_doctor",
        description=(
            "Invoke `bash scripts/doctor.sh` and return the parsed "
            "output as {pass_count, fail_count, rows[{status, detail}]}. "
            "One-call total-system health check for Claude."
        ),
        input_schema={"type": "object", "properties": {}},
        handler=lambda _args: tool_run_doctor(),
    ),
    ToolSpec(
        name="git_recent_log",
        description=(
            "Return the last N commits on the current branch with "
            "sha/author/date/subject. Lets Claude investigate 'what "
            "changed recently' without leaving the MCP surface."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Number of commits (default 20)",
                    "default": 20,
                },
            },
        },
        handler=lambda args: tool_git_recent_log(limit=int(args.get("limit", 20))),
    ),
    ToolSpec(
        name="tickvault_api",
        description=(
            "HTTP GET against the tickvault app's own REST API (port "
            "3001 by default). Paths: /health, /api/stats, "
            "/api/quote/{security_id}, /api/top-movers, "
            "/api/instruments/diagnostic, /api/option-chain, /api/pcr, "
            "/api/index-constituency. Returns status + json (or text)."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "API path starting with /",
                },
                "base_url": {
                    "type": "string",
                    "description": "Override TICKVAULT_API_URL for a single call.",
                },
            },
            "required": ["path"],
        },
        handler=lambda args: tool_tickvault_api(
            path=args["path"],
            base_url=args.get("base_url"),
        ),
    ),
    ToolSpec(
        name="grafana_query",
        description=(
            "HTTP GET against the Grafana HTTP API. Endpoints: "
            "/api/health, /api/search?query=X, /api/dashboards/uid/UID, "
            "/api/datasources/proxy/uid/DS_UID/api/v1/query?query=Q. "
            "Unauthenticated reads supported; set "
            "TICKVAULT_GRAFANA_API_TOKEN for authenticated endpoints."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "description": "Grafana API path starting with /",
                },
                "base_url": {
                    "type": "string",
                    "description": "Override TICKVAULT_GRAFANA_URL.",
                },
            },
            "required": ["endpoint"],
        },
        handler=lambda args: tool_grafana_query(
            endpoint=args["endpoint"],
            base_url=args.get("base_url"),
        ),
    ),
    ToolSpec(
        name="docker_status",
        description=(
            "Return `docker compose ps --format json` for the tickvault "
            "stack. Gives Claude container name/service/state/health "
            "without shelling into the host."
        ),
        input_schema={"type": "object", "properties": {}},
        handler=lambda _args: tool_docker_status(),
    ),
    ToolSpec(
        name="app_log_tail",
        description=(
            "Last N lines of data/logs/app.YYYY-MM-DD.log (full "
            "INFO/DEBUG output, not just ERRORs). Optional `date` "
            "(YYYY-MM-DD) picks a different day; defaults to today UTC."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max lines (default 100)",
                    "default": 100,
                },
                "date": {
                    "type": "string",
                    "description": "YYYY-MM-DD (default: today UTC)",
                },
            },
        },
        handler=lambda args: tool_app_log_tail(
            limit=int(args.get("limit", 100)),
            date=args.get("date"),
        ),
    ),
]


# ---------------------------------------------------------------------------
# JSON-RPC stdio loop — minimum MCP 2024-11-05 subset
# ---------------------------------------------------------------------------


def _respond(id_: Any, result: Any) -> None:
    sys.stdout.write(
        json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n"
    )
    sys.stdout.flush()


def _respond_error(id_: Any, code: int, message: str) -> None:
    sys.stdout.write(
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": id_,
                "error": {"code": code, "message": message},
            }
        )
        + "\n"
    )
    sys.stdout.flush()


def _handle_request(req: dict[str, Any]) -> None:
    method = req.get("method", "")
    req_id = req.get("id")
    params = req.get("params") or {}

    if method == "initialize":
        _respond(
            req_id,
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "tickvault-logs",
                    "version": "0.1.0",
                },
            },
        )
        return

    if method == "tools/list":
        _respond(
            req_id,
            {
                "tools": [
                    {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    }
                    for t in TOOLS
                ]
            },
        )
        return

    if method == "tools/call":
        tool_name = params.get("name")
        arguments = params.get("arguments") or {}
        for t in TOOLS:
            if t.name == tool_name:
                try:
                    result = t.handler(arguments)
                except Exception as exc:  # noqa: BLE001
                    _respond_error(
                        req_id, -32000, f"tool {tool_name} failed: {exc}"
                    )
                    return
                _respond(
                    req_id,
                    {
                        "content": [
                            {
                                "type": "text",
                                "text": json.dumps(result, indent=2),
                            }
                        ]
                    },
                )
                return
        _respond_error(req_id, -32601, f"unknown tool: {tool_name}")
        return

    if method == "notifications/initialized":
        return  # no-op; notifications have no id / no response

    _respond_error(req_id, -32601, f"method not found: {method}")


def _run_stdio_loop() -> None:
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
        except json.JSONDecodeError:
            _respond_error(None, -32700, "parse error")
            continue
        _handle_request(req)


# ---------------------------------------------------------------------------
# Self-test: print tool list + run a no-arg demo of each tool
# ---------------------------------------------------------------------------


def _self_test() -> int:
    print(f"tickvault-logs MCP server self-test")
    print(f"logs dir: {_logs_dir()}")
    print(f"state dir: {_state_dir()}")
    print()
    print(f"tools registered: {len(TOOLS)}")
    for t in TOOLS:
        print(f"  - {t.name}: {t.description[:70]}...")
    print()
    print("--- tool_summary_snapshot() demo ---")
    print(json.dumps(tool_summary_snapshot(), indent=2)[:400])
    print()
    print("--- tool_triage_log_tail(limit=3) demo ---")
    print(json.dumps(tool_triage_log_tail(limit=3), indent=2)[:400])
    print()
    print("--- tool_tail_errors(limit=3) demo ---")
    print(json.dumps(tool_tail_errors(limit=3), indent=2)[:400])
    print()
    print("--- tool_list_novel_signatures(since_minutes=60) demo ---")
    out = tool_list_novel_signatures(since_minutes=60)
    # Truncate for readability
    compact = {
        k: (
            v
            if k != "novel"
            else [
                {kk: vv for kk, vv in ev.items() if kk != "message"}
                for ev in v[:3]
            ]
        )
        for k, v in out.items()
    }
    print(json.dumps(compact, indent=2)[:400])
    print()
    print("self-test done")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return _self_test()
    _run_stdio_loop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
