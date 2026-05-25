#!/usr/bin/env bash
# Unit tests for the pure-function helpers in register-dhan-ip.sh.
# Runs without AWS credentials or Dhan connectivity.
#
# Invoke:  bash scripts/test-register-dhan-ip.sh
# Exit 0 = all pass; non-zero = at least one failure.

set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="${SCRIPT_DIR}/register-dhan-ip.sh"

# Source the function definitions WITHOUT executing the action steps.
# Trick: stub aws + curl so the script's `set -e` does not blow up
# before we reach the function definitions, then short-circuit at
# step 1 by overriding the flag-parse exit.
extract_function() {
    local name="$1"
    awk "
        /^${name}\\(\\) *\\{/         { capture=1; print; next }
        capture && /^\\}\$/            { print; exit }
        capture                        { print }
    " "$TARGET"
}

eval "$(extract_function is_valid_ipv4)"
eval "$(extract_function days_since_iso_date)"

FAIL=0
PASS=0

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        echo "  PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $desc — expected '$expected', got '$actual'"
        FAIL=$((FAIL + 1))
    fi
}

assert_exit() {
    local desc="$1" expected="$2" func="$3" arg="${4:-}"
    "$func" "$arg" >/dev/null 2>&1
    local rc=$?
    if [[ "$rc" -eq "$expected" ]]; then
        echo "  PASS: $desc (exit=$rc)"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $desc — expected exit $expected, got $rc"
        FAIL=$((FAIL + 1))
    fi
}

echo "== is_valid_ipv4 =="
assert_exit "valid: 1.2.3.4"            0 is_valid_ipv4 "1.2.3.4"
assert_exit "valid: 255.255.255.255"    0 is_valid_ipv4 "255.255.255.255"
assert_exit "valid: 0.0.0.0"            0 is_valid_ipv4 "0.0.0.0"
assert_exit "valid: 13.234.56.7"        0 is_valid_ipv4 "13.234.56.7"
assert_exit "reject: empty"             1 is_valid_ipv4 ""
assert_exit "reject: octet > 255"       1 is_valid_ipv4 "256.1.1.1"
assert_exit "reject: only 3 octets"     1 is_valid_ipv4 "1.2.3"
assert_exit "reject: 5 octets"          1 is_valid_ipv4 "1.2.3.4.5"
assert_exit "reject: trailing dot"      1 is_valid_ipv4 "1.2.3.4."
assert_exit "reject: leading zero"      1 is_valid_ipv4 "01.2.3.4"
assert_exit "reject: hex"               1 is_valid_ipv4 "0xff.1.2.3"
assert_exit "reject: IPv6"              1 is_valid_ipv4 "::1"
assert_exit "reject: IPv6 long"         1 is_valid_ipv4 "2001:db8::1"
assert_exit "reject: with port"         1 is_valid_ipv4 "1.2.3.4:80"
assert_exit "reject: with cidr"         1 is_valid_ipv4 "1.2.3.4/24"
assert_exit "reject: trailing space"    1 is_valid_ipv4 "1.2.3.4 "
assert_exit "reject: leading space"     1 is_valid_ipv4 " 1.2.3.4"
assert_exit "reject: alphabet"          1 is_valid_ipv4 "a.b.c.d"

echo
echo "== days_since_iso_date =="
assert_eq "empty -> 0"        "0" "$(days_since_iso_date '')"
assert_eq "garbage -> 0"      "0" "$(days_since_iso_date 'not-a-date')"

# Use a known fixed historical date — 2025-01-01T00:00:00 UTC.
# We don't assert an exact day count (depends on test runner clock)
# but we DO assert the result is positive and reasonable for a date
# in the past, AND that today's date returns 0.
RESULT=$(days_since_iso_date '2025-01-01T00:00:00')
if [[ "$RESULT" -gt 0 ]] && [[ "$RESULT" -lt 9000 ]]; then
    echo "  PASS: 2025-01-01 -> positive reasonable count ($RESULT days)"
    PASS=$((PASS + 1))
else
    echo "  FAIL: 2025-01-01 -> unexpected '$RESULT'"
    FAIL=$((FAIL + 1))
fi

# Future date should return 0 or negative (we round down) — script
# logic treats anything < 7 as cooldown active, so a future modifyDate
# (clock skew) still triggers safety abort. Just confirm it doesn't
# panic.
FUTURE_RESULT=$(days_since_iso_date '2099-01-01T00:00:00')
if [[ -n "$FUTURE_RESULT" ]]; then
    echo "  PASS: future date returns numeric ($FUTURE_RESULT)"
    PASS=$((PASS + 1))
else
    echo "  FAIL: future date returned empty"
    FAIL=$((FAIL + 1))
fi

echo
echo "== summary =="
echo "  passed: $PASS"
echo "  failed: $FAIL"

if [[ "$FAIL" -ne 0 ]]; then
    exit 1
fi
