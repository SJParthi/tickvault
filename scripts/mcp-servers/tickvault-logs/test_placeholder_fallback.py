#!/usr/bin/env python3
"""Ratchet test — when Claude Code's MCP launcher passes literal `${...}`
placeholder strings to the MCP server (because the shell env var was
missing at launch time), the server MUST NOT treat those as valid
values. It MUST fall through to `config/claude-mcp-endpoints.toml`.

Before this test existed, the server accepted `${TICKVAULT_LOGS_DIR}`
as a literal directory name and every log tool returned "file not found
under ${TICKVAULT_LOGS_DIR}/..." — the exact failure surfaced in the
review of PR #288 on 2026-04-19.

Run: `python3 scripts/mcp-servers/tickvault-logs/test_placeholder_fallback.py`
Exit code: 0 = all pass, 1 = any fail.
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# Set placeholders BEFORE importing server so the module-level cache
# (_ENDPOINTS_CACHE) is initialized with the placeholder-aware logic.
PLACEHOLDERS = {
    "TICKVAULT_LOGS_DIR": "${TICKVAULT_LOGS_DIR}",
    "TICKVAULT_PROMETHEUS_URL": "${TICKVAULT_PROMETHEUS_URL}",
    "TICKVAULT_ALERTMANAGER_URL": "${TICKVAULT_ALERTMANAGER_URL}",
    "TICKVAULT_QUESTDB_URL": "${TICKVAULT_QUESTDB_URL}",
    "TICKVAULT_GRAFANA_URL": "${TICKVAULT_GRAFANA_URL}",
    "TICKVAULT_API_URL": "${TICKVAULT_API_URL}",
    "TICKVAULT_MCP_PROFILE": "${TICKVAULT_MCP_PROFILE}",
    "TICKVAULT_LOGS_SOURCE": "${TICKVAULT_LOGS_SOURCE}",
    "TICKVAULT_GRAFANA_API_TOKEN": "${TICKVAULT_GRAFANA_API_TOKEN}",
    "TICKVAULT_MCP_ENDPOINTS_CONFIG": "${TICKVAULT_MCP_ENDPOINTS_CONFIG}",
}
for k, v in PLACEHOLDERS.items():
    os.environ[k] = v

import server  # noqa: E402

failures: list[str] = []


def assert_eq(name: str, got, expected_startswith: str | None = None, not_equal: str | None = None):
    if expected_startswith is not None:
        ok = str(got).startswith(expected_startswith) or str(got) == expected_startswith
        if not ok:
            failures.append(f"{name}: got {got!r}, expected startswith {expected_startswith!r}")
    if not_equal is not None and str(got) == not_equal:
        failures.append(f"{name}: got {got!r}, which is the placeholder we were supposed to reject")


# 1. _is_resolved() is the gatekeeper.
assert server._is_resolved(None) is False, "None must be unresolved"
assert server._is_resolved("") is False, "empty string must be unresolved"
assert server._is_resolved("${FOO}") is False, "literal placeholder must be unresolved"
assert server._is_resolved("  ${FOO}  ") is False, "whitespace-padded placeholder must be unresolved"
assert server._is_resolved("http://127.0.0.1:9090") is True, "real URL must be resolved"
assert server._is_resolved("/home/user/data") is True, "real path must be resolved"

# 2. Every accessor must reject the placeholder and fall through to config.
profile = server._active_profile()
assert_eq("active_profile", profile, not_equal="${TICKVAULT_MCP_PROFILE}")
if profile not in {"local", "mac-dev", "aws-prod"}:
    failures.append(f"active_profile: got {profile!r}, expected one of local/mac-dev/aws-prod")

logs = server._logs_dir()
assert_eq("logs_dir", logs, not_equal="${TICKVAULT_LOGS_DIR}")
if "${" in str(logs):
    failures.append(f"logs_dir: placeholder leaked through: {logs!r}")

prom = server._endpoint_url("prometheus_url", "TICKVAULT_PROMETHEUS_URL", "http://127.0.0.1:9090")
assert_eq("prom_url", prom, not_equal="${TICKVAULT_PROMETHEUS_URL}")
if "${" in prom:
    failures.append(f"prom_url: placeholder leaked through: {prom!r}")

logs_src = server._logs_source()
if logs_src not in {"http", "local"}:
    failures.append(f"logs_source: got {logs_src!r}, expected http|local")

endpoints_path = server._endpoints_config_path()
if "${" in str(endpoints_path):
    failures.append(f"endpoints_config_path: placeholder leaked through: {endpoints_path!r}")

# 3. Real values must still win over placeholders when both are set.
os.environ["TICKVAULT_MCP_PROFILE"] = "local"
server._ENDPOINTS_CACHE = None  # force re-read
if server._active_profile() != "local":
    failures.append("real profile override did not win over placeholder detection")

if failures:
    print("FAIL:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)

print("PASS: placeholder fallback works for all 10 env vars.")
sys.exit(0)
