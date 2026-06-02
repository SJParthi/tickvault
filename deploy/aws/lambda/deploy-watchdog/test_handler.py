"""Pure-function tests for the deploy-watchdog Lambda decision logic.

These tests cover the ONLY branch that can cause action (`is_stale`) without
touching AWS or GitHub — the IO helpers are thin urllib/boto3 wrappers exercised
in the live deploy. The decision contract is what must never regress:

  * dispatch ONLY when both shas are known AND differ;
  * NEVER dispatch on uncertainty (either sha unknown);
  * whitespace-insensitive equality.
"""

from __future__ import annotations

import handler


def test_is_stale_true_when_shas_differ() -> None:
    assert handler.is_stale("aaaaaaa", "bbbbbbb") is True


def test_is_stale_false_when_shas_equal() -> None:
    assert handler.is_stale("deadbeef", "deadbeef") is False


def test_is_stale_false_when_desired_unknown() -> None:
    # GitHub API blip fetching main HEAD — must NOT dispatch on uncertainty.
    assert handler.is_stale(None, "deadbeef") is False
    assert handler.is_stale("", "deadbeef") is False


def test_is_stale_false_when_deployed_unknown() -> None:
    # No successful deploy run yet (fresh repo) — must NOT dispatch (would loop).
    assert handler.is_stale("deadbeef", None) is False
    assert handler.is_stale("deadbeef", "") is False


def test_is_stale_false_when_both_unknown() -> None:
    assert handler.is_stale(None, None) is False


def test_is_stale_ignores_surrounding_whitespace() -> None:
    assert handler.is_stale(" deadbeef ", "deadbeef") is False
    assert handler.is_stale("deadbeef\n", "deadbeef") is False


def test_is_stale_is_case_sensitive_on_real_shas() -> None:
    # Git shas are lowercase hex; a case flip is a genuine difference, not noise.
    assert handler.is_stale("ABCDEF0", "abcdef0") is True
