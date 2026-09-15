#!/usr/bin/env bash
# Isolated filesystem regression tests. No AWS, systemctl or real app is used.
set -euo pipefail
export LC_ALL=C
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
source "$REPO/scripts/tickvault-start-guard.sh"
tv_guard_tools
OWNER=$(id -u)
BASE=$(mktemp -d)
BASE=$("$TV_REALPATH" -e "$BASE")
BG_PID=
trap '[ -z "$BG_PID" ] || kill "$BG_PID" 2>/dev/null || true; rm -rf -- "$BASE"' EXIT
COUNT=0
FIXTURES=0
SOURCE_SHA=1111111111111111111111111111111111111111

fixture() {
  FIXTURES=$((FIXTURES + 1))
  CASE=$BASE/case-$FIXTURES
  STATE=$CASE/state; WAL=$CASE/wal; BINARY=$CASE/approved-app
  mkdir -m 0755 -p "$STATE/approved" "$STATE/evidence" "$WAL"
  printf '#!/bin/sh\nprintf "FIXTURE_ONLY_EXECUTED namespace=%%s\\n" "$TV_WS_WAL_DIR"\n' > "$BINARY"
  chmod 0755 "$BINARY"
  # Synthetic local authority-presence fixture. CRC/bounds/history are the
  # Rust inspector's responsibility and are deliberately not claimed here.
  printf '%052d' 0 > "$WAL/sequence.tvsq"
  marker=$(printf '%s' "$WAL" | sha256sum); marker=${marker%% *}
  for ((index=0; index<64; index+=2)); do printf '%b' "\\x${marker:index:2}"; done > "$WAL/sequence.initialized"
  printf '{"fixture_only":true}\n' > "$STATE/migration-receipt.json"
  tv_guard_sha "$STATE/migration-receipt.json"; RECEIPT_SHA=$TV_DIGEST
  printf 'TVSG1\n%s\ntvsq-v1\n%s\n' "$WAL" "$RECEIPT_SHA" > "$STATE/policy"
  tv_guard_sha "$STATE/policy"; POLICY_SHA=$TV_DIGEST
  printf 'Isolated fixture validation evidence.\n' > "$CASE/evidence"
  tv_guard_sha "$CASE/evidence"; EVIDENCE_SHA=$TV_DIGEST
  cp "$CASE/evidence" "$STATE/evidence/$EVIDENCE_SHA.evidence"
  tv_guard_sha "$BINARY"; BINARY_SHA=$TV_DIGEST
  printf 'TVSG1\n%s\n%s\n%s\n%s\n%s\n' "$WAL" "$SOURCE_SHA" "$BINARY_SHA" "$POLICY_SHA" "$EVIDENCE_SHA" > "$STATE/approved/$BINARY_SHA.permit"
  : > "$STATE/start.lock"
  chmod 0644 "$STATE/policy" "$STATE/migration-receipt.json" "$STATE/approved/$BINARY_SHA.permit" "$STATE/evidence/$EVIDENCE_SHA.evidence" "$STATE/start.lock"
}

allowed() {
  if ! tv_guard_verify "$STATE" "$BINARY" "$OWNER" "$BINARY_SHA" "$SOURCE_SHA" "$WAL" > "$CASE/stdout" 2> "$CASE/stderr"; then
    cat "$CASE/stderr" >&2; printf 'fixture expected admission\n' >&2; exit 1
  fi
  COUNT=$((COUNT + 1))
}

refused() {
  local expected=$1
  if tv_guard_verify "$STATE" "$BINARY" "$OWNER" "$BINARY_SHA" "$SOURCE_SHA" "$WAL" > "$CASE/stdout" 2> "$CASE/stderr"; then
    printf 'fixture unexpectedly admitted: %s\n' "$expected" >&2; exit 1
  fi
  grep -q -- "$expected" "$CASE/stderr" || { cat "$CASE/stderr" >&2; exit 1; }
  COUNT=$((COUNT + 1))
}

fixture
before=$(find "$STATE" "$WAL" -type f -exec sha256sum {} + | sort | sha256sum)
allowed
after=$(find "$STATE" "$WAL" -type f -exec sha256sum {} + | sort | sha256sum)
[ "$before" = "$after" ] || { printf 'read-only check changed payloads\n' >&2; exit 1; }

fixture
printf 'unapproved previous binary\n' >> "$BINARY"
tv_guard_sha "$BINARY"; BINARY_SHA=$TV_DIGEST
refused path_unreadable

fixture
SOURCE_SHA=2222222222222222222222222222222222222222
refused candidate_source_mismatch
SOURCE_SHA=1111111111111111111111111111111111111111

fixture
printf 'TVSG1\noperator_maintenance\n' > "$STATE/maintenance"
refused maintenance_fenced
before=$(sha256sum "$STATE/maintenance")
tv_guard_fence "$STATE" "$OWNER" repeat_fence > /dev/null
[ "$before" = "$(sha256sum "$STATE/maintenance")" ] || exit 1

fixture
rm "$STATE/policy"
refused path_unreadable

fixture
printf 'TVSG1\nextra\n' >> "$STATE/approved/$BINARY_SHA.permit"
refused record_extra_lines

fixture
printf '\0' >> "$STATE/policy"
refused noncanonical_record

fixture
printf 'changed evidence\n' >> "$STATE/evidence/$EVIDENCE_SHA.evidence"
refused artifact_evidence_changed

fixture
printf 'changed receipt\n' >> "$STATE/migration-receipt.json"
refused migration_receipt_changed

fixture
chmod 0664 "$STATE/policy"
refused unsafe_regular_file

fixture
chmod 0775 "$STATE/approved"
refused writable_directory

fixture
chmod 0775 "$CASE"
refused writable_directory

fixture
ln "$STATE/policy" "$CASE/hardlink"
refused unsafe_regular_file

fixture
mv "$STATE/policy" "$CASE/policy-original"
ln -s "$CASE/policy-original" "$STATE/policy"
refused path_alias_or_symlink

fixture
mv "$STATE/approved" "$CASE/approved-original"
ln -s "$CASE/approved-original" "$STATE/approved"
refused path_alias_or_symlink

fixture
ln -s "$CASE/missing" "$STATE/maintenance"
refused path_unreadable

fixture
mkfifo "$STATE/maintenance"
refused unsafe_regular_file

fixture
chmod 0777 "$BINARY"
refused unsafe_regular_file

fixture
ln "$BINARY" "$CASE/second-app-link"
refused unsafe_regular_file

fixture
mv "$BINARY" "$CASE/moved-app"
ln -s "$CASE/moved-app" "$BINARY"
refused path_alias_or_symlink

fixture
rm "$WAL/sequence.initialized"
refused authority_missing_or_unsafe

fixture
printf pending > "$WAL/sequence.tvsq.tmp"
refused authority_refill_unverified

fixture
printf '%032d' 0 > "$WAL/sequence.initialized"
refused authority_directory_mismatch

fixture
OLD_WAL=$WAL; WAL=$CASE/other-wal; mkdir "$WAL"
refused candidate_namespace_mismatch
WAL=$OLD_WAL

ROOT_EXECUTION=not_run
if [ "$OWNER" = 0 ] && [ "$(uname -s)" = Linux ]; then
  fixture
  # Some root user namespaces cannot chown to an unmapped uid. Exercise the
  # real metadata owner comparison against a different trusted-owner fixture
  # parameter; production fixes that parameter to uid0, without an override.
  OWNER=1
  refused unsafe_regular_file
  OWNER=0

  # Exercise the actual run branch against a harmless fixture executable.
  # It is never /opt/tickvault/bin/tickvault and calls no real systemctl.
  fixture
  cp "$REPO/scripts/tickvault-start-guard.sh" "$CASE/installed-guard"
  cp "$REPO/deploy/systemd/tickvault-sequence-guard.conf" "$CASE/installed-dropin"
  chmod 0755 "$CASE/installed-guard"
  (
    TV_GUARD_STATE=$STATE; TV_GUARD_BINARY=$BINARY
    TV_GUARD_INSTALLED=$CASE/installed-guard; TV_GUARD_DROPIN=$CASE/installed-dropin
    tv_guard_main run
  ) > "$CASE/run-output" 2> "$CASE/run-error" || { cat "$CASE/run-error" >&2; exit 1; }
  grep -q "^FIXTURE_ONLY_EXECUTED namespace=$WAL$" "$CASE/run-output"
  COUNT=$((COUNT + 1))

  # A copied shell ELF proves argv[0], its own /proc identity and lock
  # lifetime. It receives only the bounded fixture commands below over a FIFO.
  # /proc/other-pid is not available in every isolated test executor.
  cp /bin/bash "$BINARY"
  chmod 0755 "$BINARY"
  tv_guard_sha "$BINARY"; BINARY_SHA=$TV_DIGEST
  printf 'TVSG1\n%s\n%s\n%s\n%s\n%s\n' "$WAL" "$SOURCE_SHA" "$BINARY_SHA" "$POLICY_SHA" "$EVIDENCE_SHA" > "$STATE/approved/$BINARY_SHA.permit"
  chmod 0644 "$STATE/approved/$BINARY_SHA.permit"
  mkfifo "$CASE/cat-input"
  exec 7<> "$CASE/cat-input"
  (
    TV_GUARD_STATE=$STATE; TV_GUARD_BINARY=$BINARY
    TV_GUARD_INSTALLED=$CASE/installed-guard; TV_GUARD_DROPIN=$CASE/installed-dropin
    tv_guard_main run
  ) < "$CASE/cat-input" > "$CASE/elf-output" 2> "$CASE/elf-error" &
  BG_PID=$!
  printf 'exec 6< /proc/self/exe\nsha256sum /proc/self/fd/6\nprintf "ELF_ARGV0=%%s\\n" "$0"\n' >&7
  started=no
  for ((attempt=0; attempt<200; attempt++)); do
    kill -0 "$BG_PID" 2>/dev/null || { cat "$CASE/elf-error" >&2; exit 1; }
    if grep -q "^ELF_ARGV0=$BINARY$" "$CASE/elf-output"; then started=yes; break; fi
    sleep 0.01
  done
  [ "$started" = yes ] || { cat "$CASE/elf-error" >&2; exit 1; }
  grep -q "^$BINARY_SHA  /proc/self/fd/6$" "$CASE/elf-output"
  if flock --exclusive --nonblock "$STATE/start.lock" true; then
    printf 'managed start lock did not survive ELF exec\n' >&2; exit 1
  fi
  tv_guard_fence "$STATE" "$OWNER" fixture_maintenance > /dev/null
  refused maintenance_fenced
  kill "$BG_PID"
  wait "$BG_PID" 2>/dev/null || true
  BG_PID=
  exec 7>&-
  flock --exclusive --nonblock "$STATE/start.lock" true
  COUNT=$((COUNT + 1))

  # Kernel shebang execution must ignore BASH_ENV before any helper code runs.
  # No command arguments means the guard refuses before looking at host state.
  printf 'touch %s/injected\nexit 0\n' "$CASE" > "$CASE/bash-env"
  if BASH_ENV="$CASE/bash-env" "$CASE/installed-guard" > "$CASE/injection-output" 2>&1; then
    printf 'missing guard command unexpectedly succeeded\n' >&2; exit 1
  fi
  [ ! -e "$CASE/injected" ] || { printf 'BASH_ENV ran before admission\n' >&2; exit 1; }
  grep -q TV_START_GUARD_REFUSED "$CASE/injection-output"
  COUNT=$((COUNT + 1))
  ROOT_EXECUTION=passed
fi
printf 'TV_START_GUARD_FIXTURE_OK cases=%s root_linux_execution=%s\n' "$COUNT" "$ROOT_EXECUTION"
