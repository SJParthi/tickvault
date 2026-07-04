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
# Resolution order for every endpoint URL (questdb/tickvault_api/...):
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
    if override and not (
        override.strip().startswith("${") and override.strip().endswith("}")
    ):
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


def _is_resolved(value: str | None) -> bool:
    """An env var is "resolved" iff it is set AND not a literal shell
    placeholder like `${TICKVAULT_LOGS_DIR}`. Claude Code's MCP launcher
    passes `env:` values from `.mcp.json` through literally when the
    shell env var is missing, so the MCP server receives the raw token.
    We treat that as unset and fall through to the TOML config."""
    if value is None or value == "":
        return False
    stripped = value.strip()
    if stripped.startswith("${") and stripped.endswith("}"):
        return False
    return True


def _active_profile() -> str:
    override = os.environ.get("TICKVAULT_MCP_PROFILE")
    if _is_resolved(override):
        return override  # type: ignore[return-value]
    return _load_endpoints_config().get("active", "local")


def _endpoint_url(
    kind: str,
    env_var: str,
    default: str,
    explicit: str | None = None,
) -> str:
    """Resolve an endpoint URL via the 4-tier precedence order.

    `kind` is the key in the profile config (e.g. "questdb_url").
    `env_var` is the legacy env-var override (e.g. "TICKVAULT_QUESTDB_URL").
    `default` is the Mode A localhost fallback.
    `explicit` is a single-call override from the tool invocation.
    """
    if explicit:
        return explicit
    from_env = os.environ.get(env_var)
    if _is_resolved(from_env):
        return from_env  # type: ignore[return-value]
    profile = _active_profile()
    profile_cfg = _load_endpoints_config().get("profiles", {}).get(profile, {})
    from_config = profile_cfg.get(kind)
    if isinstance(from_config, str) and from_config:
        return from_config
    return default


def _logs_dir() -> Path:
    override = os.environ.get("TICKVAULT_LOGS_DIR")
    if _is_resolved(override):
        return Path(override)  # type: ignore[arg-type]
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
    if _is_resolved(override) and override in {"http", "local"}:
        return override  # type: ignore[return-value]
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

    2026-07-04 tunnel-exposure hardening: the `/api/debug/*` routes are now
    BEARER-GATED in crates/api. When the operator exports TICKVAULT_API_TOKEN
    (the same bearer the app reads from SSM), it is sent as the Authorization
    header so remote sessions keep read access. The token is NEVER included
    in the returned dict or any log line. Without the env var, gated routes
    answer 401 — honest, and the canonical file-reading MCP log tools are
    unaffected (they read data/logs/* directly).
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
    headers: dict[str, str] = {}
    api_token = os.environ.get("TICKVAULT_API_TOKEN", "").strip()
    if api_token:
        headers["Authorization"] = f"Bearer {api_token}"
    req = urllib.request.Request(full, headers=headers)  # noqa: S310
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:  # noqa: S310
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


# ---------------------------------------------------------------------------
# CloudWatch Logs (PROD) — fully-automated read access for Claude sessions.
# ---------------------------------------------------------------------------
#
# Goal (operator 2026-06-12): "whenever logs need to be read it must be
# automatic — no per-read permission, no human effort." This tool gives any
# Claude session direct, read-only access to the prod CloudWatch log group
# (/tickvault/prod/app) so the operator never pastes/downloads a CSV again.
#
# HONEST one-time setup (the ONLY human step, ever — a security boundary that
# cannot be coded away): read-only AWS credentials must exist in the session
# environment so the `aws` CLI can call CloudWatch Logs. Wire ONCE via the
# Claude Code environment config:
#   - AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY for a read-only IAM user scoped
#     to logs:FilterLogEvents + logs:GetLogEvents on
#     arn:aws:logs:ap-south-1:*:log-group:/tickvault/prod/*
#   - AWS_REGION (default ap-south-1) + the `aws` CLI on PATH.
# After that: every session reads prod logs automatically, zero further effort.


def _cloudwatch_log_group() -> str:
    return os.environ.get("TICKVAULT_CLOUDWATCH_LOG_GROUP", "/tickvault/prod/app")


def _aws_region() -> str:
    return os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "ap-south-1"


def _aws_credentials() -> tuple[str, str, str | None]:
    """(access_key, secret_key, session_token|None) from the standard env vars.
    Empty strings if absent. This is the ONLY thing needed to read CloudWatch
    directly — no aws CLI, no portal."""
    return (
        os.environ.get("AWS_ACCESS_KEY_ID", ""),
        os.environ.get("AWS_SECRET_ACCESS_KEY", ""),
        os.environ.get("AWS_SESSION_TOKEN") or None,
    )


def _sigv4_signing_key(secret: str, date_stamp: str, region: str, service: str) -> bytes:
    """Derive the AWS SigV4 signing key (HMAC chain). Pure — unit-testable
    against AWS's published test vectors."""
    import hashlib  # noqa: PLC0415
    import hmac  # noqa: PLC0415

    def _h(key: bytes, msg: str) -> bytes:
        return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()

    k_date = _h(("AWS4" + secret).encode("utf-8"), date_stamp)
    k_region = _h(k_date, region)
    k_service = _h(k_region, service)
    return _h(k_service, "aws4_request")


def build_cloudwatch_sigv4_request(
    region: str,
    log_group: str,
    start_ms: int,
    limit: int,
    filter_pattern: str | None,
    access_key: str,
    secret_key: str,
    session_token: str | None,
    amz_now,
) -> tuple[str, bytes, dict[str, str]]:
    """Pure builder for a SigV4-signed CloudWatch Logs `FilterLogEvents` POST.
    Returns `(url, body_bytes, headers)`. `amz_now` is a `datetime` (UTC) so the
    signing is deterministic + testable. No network, no aws CLI, no boto3."""
    import hashlib  # noqa: PLC0415
    import hmac  # noqa: PLC0415

    service = "logs"
    host = f"logs.{region}.amazonaws.com"
    url = f"https://{host}/"
    amz_date = amz_now.strftime("%Y%m%dT%H%M%SZ")
    date_stamp = amz_now.strftime("%Y%m%d")
    target = "Logs_20140328.FilterLogEvents"
    content_type = "application/x-amz-json-1.1"

    body_obj: dict[str, Any] = {
        "logGroupName": log_group,
        "startTime": int(start_ms),
        "limit": max(1, min(int(limit), 10000)),
    }
    if filter_pattern:
        body_obj["filterPattern"] = filter_pattern
    body = json.dumps(body_obj, separators=(",", ":")).encode("utf-8")
    payload_hash = hashlib.sha256(body).hexdigest()

    # Canonical headers MUST be sorted by lowercase name. Build a list and sort,
    # so x-amz-security-token slots into the correct position (before x-amz-target,
    # since "...se" < "...ta") when a session token is present — an out-of-order
    # canonical header set yields an invalid signature (AWS 403).
    header_pairs: list[tuple[str, str]] = [
        ("content-type", content_type),
        ("host", host),
        ("x-amz-content-sha256", payload_hash),
        ("x-amz-date", amz_date),
        ("x-amz-target", target),
    ]
    if session_token:
        header_pairs.append(("x-amz-security-token", session_token))
    header_pairs.sort(key=lambda kv: kv[0])
    canon_headers = "".join(f"{name}:{value}\n" for name, value in header_pairs)
    signed_headers = ";".join(name for name, _ in header_pairs)

    canonical_request = (
        f"POST\n/\n\n{canon_headers}\n{signed_headers}\n{payload_hash}"
    )
    scope = f"{date_stamp}/{region}/{service}/aws4_request"
    string_to_sign = (
        "AWS4-HMAC-SHA256\n"
        f"{amz_date}\n"
        f"{scope}\n"
        f"{hashlib.sha256(canonical_request.encode('utf-8')).hexdigest()}"
    )
    signing_key = _sigv4_signing_key(secret_key, date_stamp, region, service)
    signature = hmac.new(
        signing_key, string_to_sign.encode("utf-8"), hashlib.sha256
    ).hexdigest()
    authorization = (
        f"AWS4-HMAC-SHA256 Credential={access_key}/{scope}, "
        f"SignedHeaders={signed_headers}, Signature={signature}"
    )
    headers = {
        "Content-Type": content_type,
        "X-Amz-Date": amz_date,
        "X-Amz-Content-Sha256": payload_hash,
        "X-Amz-Target": target,
        "Authorization": authorization,
    }
    if session_token:
        headers["X-Amz-Security-Token"] = session_token
    return url, body, headers


def _tool_cloudwatch_logs_via_sigv4(
    minutes: int, filter_pattern: str | None, limit: int
) -> dict[str, Any]:
    """Direct CloudWatch Logs read via SigV4-signed HTTPS — the path the
    operator wants: drop an AWS read-only key in the env and logs flow, no aws
    CLI, no portal. Uses stdlib only. Single-page (no nextToken paging): returns
    up to `limit` (server-capped at 10000, and one response page is ~1 MB), which
    is ample for a 'recent logs' tail."""
    import datetime as _dt  # noqa: PLC0415
    import time as _time  # noqa: PLC0415
    import urllib.error  # noqa: PLC0415
    import urllib.request  # noqa: PLC0415

    import re as _re  # noqa: PLC0415

    region = _aws_region()
    group = _cloudwatch_log_group()
    # Validate region charset before it lands in the request host — a region
    # with `/`, `@`, `:` etc. would build a malformed/attacker-shaped host.
    # AWS region names are only lowercase letters, digits and hyphens.
    if not _re.fullmatch(r"[a-z0-9-]+", region):
        return {
            "ok": False,
            "source": "cloudwatch_sigv4",
            "log_group": group,
            "error": "invalid AWS region — set AWS_DEFAULT_REGION to a region like "
            "ap-south-1 (lowercase letters, digits, hyphens only).",
        }
    access_key, secret_key, session_token = _aws_credentials()
    start_ms = int((_time.time() - max(1, int(minutes)) * 60) * 1000)
    url, body, headers = build_cloudwatch_sigv4_request(
        region,
        group,
        start_ms,
        limit,
        filter_pattern,
        access_key,
        secret_key,
        session_token,
        _dt.datetime.now(_dt.timezone.utc),
    )
    req = urllib.request.Request(url, data=body, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310 — signed https to AWS
            payload = resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        # AWS returns the real reason (e.g. {"__type":"ResourceNotFoundException",
        # "message":"..."} or ExpiredTokenException / AccessDenied) in the HTTP
        # body. A bare str(HTTPError) is only "HTTP Error 400: Bad Request", which
        # hides WHY the read failed. Read the (bounded) body so the operator/Claude
        # sees the actual AWS error type — the body carries no signed headers and
        # no secret, so this is safe to surface.
        try:
            err_body = exc.read().decode("utf-8", "replace")[:400]
        except Exception:  # noqa: BLE001 — body already consumed / unreadable
            err_body = ""
        return {
            "ok": False,
            "source": "cloudwatch_sigv4",
            "log_group": group,
            "region": region,
            "error": f"CloudWatch FilterLogEvents HTTP {exc.code}: "
            f"{err_body or exc.reason}",
        }
    except Exception as exc:  # noqa: BLE001 — surface as ok=False, never crash
        # Bound the exception text and use only the type + a short str — never
        # the request object — so a future stdlib change can't echo a signed
        # header (the secret is not in any header, but defence-in-depth).
        return {
            "ok": False,
            "source": "cloudwatch_sigv4",
            "log_group": group,
            "region": region,
            "error": f"CloudWatch FilterLogEvents failed: {type(exc).__name__}: "
            f"{str(exc)[:300]}",
        }
    events = parse_cloudwatch_events(payload, limit)
    return {
        "ok": True,
        "source": "cloudwatch_sigv4",
        "log_group": group,
        "region": region,
        "lookback_minutes": int(minutes),
        "filter_pattern": filter_pattern or "",
        "returned": len(events),
        "events": events,
    }


def build_cloudwatch_filter_args(
    log_group: str,
    region: str,
    start_ms: int,
    limit: int,
    filter_pattern: str | None,
) -> list[str]:
    """Pure builder for the `aws logs filter-log-events` argv (unit-testable
    without AWS access). `filter_pattern` is a CloudWatch filter pattern, e.g.
    `"ERROR"` or `"WS-GAP-05"`. Empty/None means no server-side filter.
    """
    args = [
        "aws",
        "logs",
        "filter-log-events",
        "--region",
        region,
        "--log-group-name",
        log_group,
        "--start-time",
        str(int(start_ms)),
        "--limit",
        str(max(1, min(int(limit), 10000))),
        "--output",
        "json",
    ]
    if filter_pattern:
        args += ["--filter-pattern", filter_pattern]
    return args


def parse_cloudwatch_events(stdout: str, limit: int) -> list[dict[str, Any]]:
    """Pure parser: turn `aws logs filter-log-events` JSON into a compact list
    of `{ts_ms, stream, message}` (newest last), trimmed to `limit`.
    """
    try:
        payload = json.loads(stdout) if stdout.strip() else {}
    except json.JSONDecodeError:
        return []
    events = payload.get("events", []) if isinstance(payload, dict) else []
    out: list[dict[str, Any]] = []
    for ev in events:
        if not isinstance(ev, dict):
            continue
        out.append(
            {
                "ts_ms": ev.get("timestamp"),
                "stream": ev.get("logStreamName"),
                "message": (ev.get("message") or "").rstrip("\n"),
            }
        )
    out.sort(key=lambda e: e.get("ts_ms") or 0)
    return out[-int(limit):] if limit > 0 else out


def _portal_url() -> str:
    """Operator-control portal base URL (the existing dashboard's API Gateway /
    Function URL). When set with TICKVAULT_PORTAL_TOKEN, prod logs are read via
    the portal's `logs` action — no `aws` CLI, no AWS key in this session; the
    portal Lambda reads the box with its OWN IAM role. The operator already has
    this URL + bearer secret (it powers the dashboard)."""
    return os.environ.get("TICKVAULT_PORTAL_URL", "").rstrip("/")


def _portal_token() -> str:
    return os.environ.get("TICKVAULT_PORTAL_TOKEN", "")


def parse_portal_logs_raw(raw: str) -> list[dict[str, Any]]:
    """Pure parser for the portal `logs` action's `raw` field — the box's
    journalctl tail wrapped in ERR_BEGIN/ERR_END + APP_BEGIN/APP_END markers
    (see operator-control handler.py `logs` action). Returns
    `[{section, message}]` in file order (err section first, then app), with the
    marker lines themselves dropped. Unit-testable without any network/AWS.
    """
    out: list[dict[str, Any]] = []
    section: str | None = None
    section_for = {
        "ERR_BEGIN": "err",
        "APP_BEGIN": "app",
    }
    for line in (raw or "").splitlines():
        stripped = line.strip()
        if stripped in section_for:
            section = section_for[stripped]
            continue
        if stripped in ("ERR_END", "APP_END"):
            section = None
            continue
        if section is None:
            continue
        out.append({"section": section, "message": line.rstrip("\n")})
    return out


def filter_and_trim_portal_events(
    events: list[dict[str, Any]], filter_pattern: str | None, limit: int
) -> list[dict[str, Any]]:
    """Apply a client-side substring `filter_pattern` (the portal `logs` action
    is a fixed journalctl tail, so filtering is done here, not server-side) and
    trim to the newest `limit`. Pure."""
    out = events
    if filter_pattern:
        needle = filter_pattern.strip()
        out = [e for e in out if needle in (e.get("message") or "")]
    return out[-int(limit):] if limit > 0 else out


def _call_portal_logs(url: str, token: str, timeout: float = 30.0) -> dict[str, Any]:
    """POST {"action":"logs"} to the operator-control portal with Bearer auth,
    returning the parsed JSON dict. Stdlib-only (urllib) — no aws CLI, no boto3.
    Raises on transport/HTTP error (caller turns it into an ok=False result)."""
    import urllib.request  # noqa: PLC0415

    body = json.dumps({"action": "logs"}).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 — operator-set https URL
        payload = resp.read().decode("utf-8", "replace")
    parsed = json.loads(payload) if payload.strip() else {}
    return parsed if isinstance(parsed, dict) else {}


def _tool_cloudwatch_logs_via_portal(
    filter_pattern: str | None, limit: int
) -> dict[str, Any]:
    """Portal-backed log read (preferred): reuses the existing dashboard's
    `logs` endpoint. The portal returns the box's live journalctl err+app tail.
    `minutes` is not honoured (the portal serves a fixed recent tail); we apply
    `filter_pattern` (substring) + `limit` client-side."""
    url = _portal_url()
    token = _portal_token()
    # SECURITY: refuse plaintext — a misconfigured http:// would send the
    # bearer token unencrypted (security review LOW). https only.
    if not url.startswith("https://"):
        return {
            "ok": False,
            "source": "portal",
            "portal_url": url,
            "error": "TICKVAULT_PORTAL_URL must use https:// (refusing to send the "
            "bearer token over plaintext).",
        }
    try:
        parsed = _call_portal_logs(url, token)
    except Exception as exc:  # noqa: BLE001 — surface as ok=False, never crash
        return {
            "ok": False,
            "source": "portal",
            "portal_url": url,
            "error": f"portal logs call failed: {type(exc).__name__}: {exc}",
        }
    if not parsed.get("ok"):
        return {
            "ok": False,
            "source": "portal",
            "portal_url": url,
            "error": "portal returned ok=false (check the bearer token / box state)",
            "portal_response": str(parsed)[:400],
        }
    events = filter_and_trim_portal_events(
        parse_portal_logs_raw(str(parsed.get("raw", ""))), filter_pattern, limit
    )
    return {
        "ok": True,
        "source": "portal",
        "portal_url": url,
        "filter_pattern": filter_pattern or "",
        "returned": len(events),
        "events": events,
        "note": "live journalctl err+app tail via the operator portal (no aws CLI / no AWS key needed)",
    }


def tool_cloudwatch_logs(
    minutes: int = 60,
    filter_pattern: str | None = None,
    limit: int = 100,
) -> dict[str, Any]:
    """Read recent prod logs, fully automated. PREFERRED path: direct CloudWatch
    read via a SigV4-signed HTTPS call — the ONLY input is a read-only AWS key in
    the env (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY [+ AWS_SESSION_TOKEN];
    no aws CLI, no portal, no boto3). NEXT: the operator-control portal's `logs`
    endpoint (set TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN — the portal
    Lambda reads the box with its own role). FALLBACK: the read-only `aws` CLI on
    the /tickvault/prod/app CloudWatch group. Looks back `minutes` (SigV4 + aws
    paths), optional `filter_pattern` (server-side CloudWatch filter on the SigV4
    + aws paths; substring on the portal path), up to `limit` newest events.
    Fails clearly (ok=False) if none of the three paths is configured. The AWS
    secret/token is NEVER logged nor placed in the returned dict."""
    import subprocess
    import time as _time

    # PREFERRED (operator goal): direct CloudWatch read with a SigV4-signed
    # HTTPS request — only a read-only AWS key in the env is needed. Chosen
    # first whenever both key parts are present (no aws CLI, no portal).
    access_key, secret_key, _session_token = _aws_credentials()
    if access_key and secret_key:
        return _tool_cloudwatch_logs_via_sigv4(minutes, filter_pattern, limit)

    # NEXT: reuse the operator dashboard's already-working logs endpoint.
    if _portal_url() and _portal_token():
        return _tool_cloudwatch_logs_via_portal(filter_pattern, limit)

    region = _aws_region()
    group = _cloudwatch_log_group()
    start_ms = int((_time.time() - max(1, int(minutes)) * 60) * 1000)
    argv = build_cloudwatch_filter_args(group, region, start_ms, limit, filter_pattern)
    try:
        proc = subprocess.run(
            argv, check=False, capture_output=True, text=True, timeout=30
        )
    except FileNotFoundError:
        return {
            "ok": False,
            "error": "no log reader configured — set AWS_ACCESS_KEY_ID + "
            "AWS_SECRET_ACCESS_KEY (+ AWS_DEFAULT_REGION) for the direct SigV4 "
            "path (no aws CLI, no portal), OR set TICKVAULT_PORTAL_URL + "
            "TICKVAULT_PORTAL_TOKEN to read via the operator dashboard, OR wire "
            "a read-only AWS credential + aws CLI. None is present in this session.",
            "log_group": group,
        }
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "aws logs filter-log-events timed out after 30s"}
    if proc.returncode != 0:
        return {
            "ok": False,
            "exit_code": proc.returncode,
            "error": "aws logs call failed — most likely no read-only AWS credentials in "
            "this environment yet. Prefer the direct SigV4 path (AWS_ACCESS_KEY_ID + "
            "AWS_SECRET_ACCESS_KEY) or the portal path (TICKVAULT_PORTAL_URL + "
            "TICKVAULT_PORTAL_TOKEN). stderr below.",
            "stderr": proc.stderr.strip()[:800],
            "log_group": group,
        }
    events = parse_cloudwatch_events(proc.stdout, limit)
    return {
        "ok": True,
        "source": "aws_cli",
        "log_group": group,
        "region": region,
        "lookback_minutes": int(minutes),
        "filter_pattern": filter_pattern or "",
        "returned": len(events),
        "events": events,
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
    ToolSpec(
        name="cloudwatch_logs",
        description=(
            "Read recent PROD logs — fully automated, no human paste/download. "
            "PREFERRED: direct CloudWatch read via a SigV4-signed HTTPS request — "
            "the ONLY input is a read-only AWS key in the env (AWS_ACCESS_KEY_ID + "
            "AWS_SECRET_ACCESS_KEY [+ AWS_SESSION_TOKEN], AWS_DEFAULT_REGION); no "
            "aws CLI, no portal, no boto3. NEXT: the operator-control dashboard's "
            "logs endpoint (TICKVAULT_PORTAL_URL + TICKVAULT_PORTAL_TOKEN). "
            "FALLBACK: read-only aws CLI on /tickvault/prod/app. Args: minutes "
            "(lookback; SigV4 + aws paths), filter_pattern (CloudWatch filter on "
            "SigV4 + aws, substring on portal, e.g. \"ERROR\" or \"WS-GAP-05\"), "
            "limit (default 100). The AWS secret/token is NEVER logged nor "
            "returned. Clear ok=false error if no path is configured."
        ),
        input_schema={
            "type": "object",
            "properties": {
                "minutes": {
                    "type": "integer",
                    "description": "Lookback window in minutes (default 60)",
                    "default": 60,
                },
                "filter_pattern": {
                    "type": "string",
                    "description": "Optional CloudWatch filter pattern (e.g. ERROR)",
                },
                "limit": {
                    "type": "integer",
                    "description": "Max events (default 100)",
                    "default": 100,
                },
            },
        },
        handler=lambda args: tool_cloudwatch_logs(
            minutes=int(args.get("minutes", 60)),
            filter_pattern=args.get("filter_pattern"),
            limit=int(args.get("limit", 100)),
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
