#!/usr/bin/env python3
"""Standalone tests for the cloudwatch_logs MCP tool's PURE helpers.

Stdlib-only, no AWS access required (mirrors test_placeholder_fallback.py).
Run: python3 test_cloudwatch_logs.py  (exit 0 = pass, 1 = fail)

Verifies the argv builder + JSON parser + the graceful no-credentials path,
so the fully-automated CloudWatch read access has ratcheted coverage even
though the live `aws` call can only be exercised on a credentialed host.
"""

from __future__ import annotations

import json
import sys

import server

failures: list[str] = []


def check(cond: bool, msg: str) -> None:
    if not cond:
        failures.append(msg)


# --- build_cloudwatch_filter_args ---
args = server.build_cloudwatch_filter_args(
    "/tickvault/prod/app", "ap-south-1", 1_700_000_000_000, 50, "ERROR"
)
check(args[:3] == ["aws", "logs", "filter-log-events"], f"argv prefix: {args[:3]}")
check("/tickvault/prod/app" in args, "log group missing from argv")
check("ap-south-1" in args, "region missing from argv")
check("ERROR" in args and "--filter-pattern" in args, "filter pattern missing")

args_no_filter = server.build_cloudwatch_filter_args("/g", "r", 1, 5, None)
check("--filter-pattern" not in args_no_filter, "filter flag present with no pattern")

args_clamp = server.build_cloudwatch_filter_args("/g", "r", 1, 999_999, None)
limit_val = args_clamp[args_clamp.index("--limit") + 1]
check(limit_val == "10000", f"limit not clamped to 10000: {limit_val}")

# --- parse_cloudwatch_events (sorted oldest..newest, trimmed) ---
sample = json.dumps(
    {
        "events": [
            {"timestamp": 2, "logStreamName": "s", "message": "b\n"},
            {"timestamp": 1, "logStreamName": "s", "message": "a"},
        ]
    }
)
evs = server.parse_cloudwatch_events(sample, 100)
check([e["message"] for e in evs] == ["a", "b"], f"events not sorted oldest..newest: {evs}")
check(server.parse_cloudwatch_events("not json", 10) == [], "bad json should parse to []")
check(server.parse_cloudwatch_events("", 10) == [], "empty should parse to []")

trimmed = server.parse_cloudwatch_events(
    json.dumps({"events": [{"timestamp": i, "message": str(i)} for i in range(5)]}), 2
)
check([e["message"] for e in trimmed] == ["3", "4"], f"limit-trim keeps newest: {trimmed}")

# --- graceful failure when no aws CLI / creds (must NOT crash) ---
result = server.tool_cloudwatch_logs(minutes=5, limit=3)
check(result.get("ok") is False, f"expected ok=False without creds: {result}")
check("log_group" in result, "error result should still report the log_group")
check(bool(result.get("error")), "error result should carry a human-readable message")

if failures:
    print("FAIL:")
    for f in failures:
        print("  -", f)
    sys.exit(1)
print("cloudwatch_logs helper tests: ALL PASS")
sys.exit(0)
