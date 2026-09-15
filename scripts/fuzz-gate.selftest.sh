#!/usr/bin/env bash
# Execute the actual workflow step bodies against deterministic stand-ins.
# No compiler, external requests or real fuzz run is involved.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin"

extract_step() {
  local id="$1"
  awk -v id="$id" '
    $0 == "        id: " id { found = 1; next }
    found && $0 == "        run: |" { body = 1; next }
    body && substr($0, 1, 10) == "          " { print substr($0, 11); next }
    body && $0 ~ /^[[:space:]]*$/ { print; next }
    body { exit }
  ' "$ROOT/.github/workflows/fuzz.yml" \
    | sed 's/${{ env.NIGHTLY_VERSION }}/nightly-fixture/g; s/${{ matrix.target }}/fixture/g' \
    > "$TMP/$id.sh"
  test -s "$TMP/$id.sh"
}
extract_step build
extract_step fuzz

cat > "$TMP/bin/cargo" <<'SH'
#!/usr/bin/env bash
if [[ "$3" == build ]]; then
  [[ "$TV_FUZZ_FIXTURE" != build_fail ]] || exit 77
  exit 0
fi
case "$TV_FUZZ_FIXTURE" in
  crash) echo 'ERROR: deterministic fixture failure'; exit 77 ;;
  empty) exit 0 ;;
  zero) echo '#0 DONE cov: 0'; exit 0 ;;
  *) echo '#37 DONE cov: 1'; exit 0 ;;
esac
SH
cat > "$TMP/bin/timeout" <<'SH'
#!/usr/bin/env bash
case "$TV_FUZZ_FIXTURE" in
  timeout) exit 124 ;;
  killed) exit 137 ;;
esac
shift 2
exec "$@"
SH
chmod +x "$TMP/bin/cargo" "$TMP/bin/timeout"

count=0
check() {
  local step="$1" mode="$2" expected="$3" duration="${4:-1}" code=0
  env PATH="$TMP/bin:$PATH" TV_FUZZ_FIXTURE="$mode" FUZZ_SECS="$duration" \
    GITHUB_OUTPUT="$TMP/outputs" bash -e "$TMP/$step.sh" > "$TMP/log" 2>&1 || code=$?
  if [[ "$code" != "$expected" ]]; then
    cat "$TMP/log" >&2
    echo "FAIL: $step/$mode/$duration returned $code, expected $expected" >&2
    exit 1
  fi
  count=$((count + 1))
}

check build success 0
check build build_fail 1
check fuzz success 0
check fuzz crash 1
check fuzz timeout 1
check fuzz killed 1
check fuzz empty 1
check fuzz zero 1
check fuzz success 1 0
check fuzz success 1 invalid
check fuzz success 1 99999
check fuzz success 0 00009
echo "fuzz gate self-test: $count cases passed"
