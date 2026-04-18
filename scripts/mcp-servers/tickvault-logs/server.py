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
# Configuration
# ---------------------------------------------------------------------------


def _logs_dir() -> Path:
    override = os.environ.get("TICKVAULT_LOGS_DIR")
    if override:
        return Path(override)
    # Default: repo_root/data/logs relative to this file.
    here = Path(__file__).resolve()
    # scripts/mcp-servers/tickvault-logs/server.py -> repo root
    return here.parent.parent.parent.parent / "data" / "logs"


def _state_dir() -> Path:
    here = Path(__file__).resolve()
    return here.parent.parent.parent.parent / ".claude" / "state"


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
