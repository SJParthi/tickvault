#!/bin/bash -p
# Explicit offline operator tool. Normal deployment must never call install,
# approve or release implicitly. No command here fabricates namespace evidence.
set -euo pipefail
export LC_ALL=C
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
umask 022

die() { printf 'TV_START_GUARD_ADMIN_REFUSED %s\n' "$*" >&2; exit 2; }
[ "$EUID" = 0 ] || die root_required
MODE=${1:-}; [ "$#" -gt 0 ] && shift
STATE=/var/lib/tickvault/start-guard
GUARD=/usr/local/libexec/tickvault-start-guard
DROPIN=/etc/systemd/system/tickvault.service.d/90-sequence-authority-guard.conf

digest_is() {
  local path=$1 wanted=$2 actual
  [[ "$wanted" =~ ^[0-9a-f]{64}$ ]] || die invalid_expected_digest
  actual=$(timeout 20s sha256sum -- "$path") || die digest_unverified
  [ "${actual%% *}" = "$wanted" ] || die input_digest_mismatch
}

# This small bootstrap cannot source the helper until its whole path is safe.
# Checking only the final root-owned file would leave a rename race through a
# runtime-user-owned parent directory before root executes the helper's code.
bootstrap_safe_source() {
  local path=$1 cursor= part uid mode links
  [[ "$path" =~ ^/[A-Za-z0-9._/-]+$ ]] && [ "$(realpath -e -- "$path")" = "$path" ] || die unsafe_source_path
  local parts=()
  IFS=/ read -r -a parts <<< "${path#/}"
  for part in "${parts[@]}"; do
    cursor=$cursor/$part
    [ ! -L "$cursor" ] || die source_symlink
    read -r uid mode links < <(stat -c '%u %a %h' -- "$cursor")
    [ "$uid" = 0 ] || die source_not_root_owned
    if [ "$cursor" = "$path" ]; then
      [ -f "$cursor" ] && [ "$links" = 1 ] && (( (8#$mode & 0022) == 0 )) || die unsafe_source_file
    else
      [ -d "$cursor" ] || die unsafe_source_parent
      (( (8#$mode & 0022) == 0 || (8#$mode & 01000) != 0 )) || die writable_source_parent
    fi
  done
}

if [ "$MODE" = install ]; then
  [ "$#" = 4 ] || die 'usage=install_GUARD_SOURCE_GUARD_SHA256_DROPIN_SOURCE_DROPIN_SHA256'
  GUARD_SOURCE=$1; GUARD_SHA=$2; DROPIN_SOURCE=$3; DROPIN_SHA=$4
  # Caller stages the reviewed sources in a private root-owned directory. Pin
  # the helper before sourcing its validation functions with root privileges.
  bootstrap_safe_source "$GUARD_SOURCE"
  digest_is "$GUARD_SOURCE" "$GUARD_SHA"
  source "$GUARD_SOURCE"
  tv_guard_tools
  tv_guard_secure_path "$GUARD_SOURCE" regular 0
  tv_guard_secure_path "$DROPIN_SOURCE" regular 0
  digest_is "$DROPIN_SOURCE" "$DROPIN_SHA"
  for directory in /var/lib/tickvault "$STATE" "$STATE/approved" "$STATE/evidence" /usr/local/libexec /etc/systemd/system/tickvault.service.d; do
    if [ ! -e "$directory" ] && [ ! -L "$directory" ]; then
      tv_guard_secure_path "$(dirname "$directory")" directory 0
      mkdir -m 0755 -- "$directory"
      sync "$directory" "$(dirname "$directory")"
    fi
    tv_guard_secure_path "$directory" directory 0
    # Traverse these specific state/helper directories as the runtime user.
    # Never recursively expose rollback, backup or credential files.
    chmod 0755 "$directory"
  done
  # Initial installation and every helper replacement are explicitly fenced.
  # This command does not stop the old process: stopping/exclusion is the next
  # separate operator step, under the durable fence.
  tv_guard_fence "$STATE" 0 guard_installation
  # These deployment-owned parents/artifacts previously belonged to ec2-user.
  # The app's permitted writes remain in its existing data/config/repo child
  # directories; never recursively chown or alter those runtime paths here.
  for directory in /opt/tickvault /opt/tickvault/bin; do
    [ -d "$directory" ] && [ ! -L "$directory" ] && [ "$(realpath -e "$directory")" = "$directory" ] || die unsafe_binary_directory
    chown root:root "$directory"
    chmod 0755 "$directory"
    tv_guard_secure_path "$directory" directory 0
  done
  for binary in /opt/tickvault/bin/tickvault /opt/tickvault/bin/smoke_test /opt/tickvault/bin/tv-wal-sequence-migrate; do
    if [ -e "$binary" ] || [ -L "$binary" ]; then
      [ -f "$binary" ] && [ ! -L "$binary" ] && [ "$(stat -c %h "$binary")" = 1 ] || die unsafe_installed_binary
      chown root:root "$binary"
      chmod 0755 "$binary"
      tv_guard_secure_path "$binary" executable 0
    fi
  done
  if [ ! -e "$STATE/start.lock" ] && [ ! -L "$STATE/start.lock" ]; then
    (set -o noclobber; : > "$STATE/start.lock")
    sync "$STATE/start.lock" "$STATE"
  fi
  tv_guard_secure_path "$STATE/start.lock" regular 0
  for destination in "$GUARD" "$DROPIN"; do
    if [ -e "$destination" ] || [ -L "$destination" ]; then
      tv_guard_secure_path "$destination" regular 0
    fi
  done
  temporary=$(mktemp /usr/local/libexec/.tickvault-start-guard.XXXXXX)
  install -o root -g root -m 0755 -- "$GUARD_SOURCE" "$temporary"
  digest_is "$temporary" "$GUARD_SHA"
  sync "$temporary"
  mv -- "$temporary" "$GUARD"
  sync "$GUARD" /usr/local/libexec
  temporary=$(mktemp /etc/systemd/system/tickvault.service.d/.sequence-guard.XXXXXX)
  install -o root -g root -m 0644 -- "$DROPIN_SOURCE" "$temporary"
  digest_is "$temporary" "$DROPIN_SHA"
  sync "$temporary"
  mv -- "$temporary" "$DROPIN"
  sync "$DROPIN" /etc/systemd/system/tickvault.service.d
  systemctl daemon-reload
  printf 'TV_START_GUARD_INSTALLED_FENCED guard_sha256=%s dropin_sha256=%s\n' "$GUARD_SHA" "$DROPIN_SHA"
  exit 0
fi

# Installed admission code and its ancestors must be root-owned and immutable
# to the runtime user. Verify before sourcing; no environment path overrides.
for directory in /usr /usr/local /usr/local/libexec; do
  [ -d "$directory" ] && [ ! -L "$directory" ] && [ "$(stat -c %u "$directory")" = 0 ] || die unsafe_installed_guard_directory
  mode=$(stat -c %a "$directory"); (( (8#$mode & 0022) == 0 )) || die writable_installed_guard_directory
done
[ -f "$GUARD" ] && [ ! -L "$GUARD" ] && [ "$(stat -c '%u:%h' "$GUARD")" = 0:1 ] || die unsafe_installed_guard
mode=$(stat -c %a "$GUARD"); (( (8#$mode & 0022) == 0 )) || die writable_installed_guard
source "$GUARD"
tv_guard_tools
tv_guard_secure_path "$STATE" directory 0

if [ "$MODE" = fence ]; then
  [ "$#" = 1 ] || die usage=fence_REASON_TOKEN
  tv_guard_fence "$STATE" 0 "$1"
  exit 0
fi

# Every mutation of permanent policy/permits requires both a durable fence and
# exclusive managed-start ownership. Legacy/external writers are additionally
# excluded by the stopped-unit check and the offline migration contract.
tv_guard_secure_path "$STATE/maintenance" regular 0
tv_guard_secure_path "$STATE/start.lock" regular 0
exec 8< "$STATE/start.lock"
flock --exclusive --timeout 5 8 || die managed_writer_or_start_still_owns_lock
[ "$(systemctl show -p ActiveState --value tickvault)" = inactive ] || die managed_unit_not_inactive
[ "$(systemctl show -p MainPID --value tickvault)" = 0 ] || die managed_pid_still_present

write_once() {
  local source=$1 destination=$2 expected=$3 temporary
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    tv_guard_secure_path "$destination" regular 0
    digest_is "$destination" "$expected"
    return
  fi
  tv_guard_secure_path "$(dirname "$destination")" directory 0
  temporary=$(mktemp "$(dirname "$destination")/.guard-record.XXXXXX")
  install -o root -g root -m 0644 -- "$source" "$temporary"
  digest_is "$temporary" "$expected"
  sync "$temporary"
  # noclobber publication preserves a pre-existing conflicting original. The
  # exclusive state lock plus active fence serialize all cooperating writers.
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    rm -- "$temporary"
    die concurrent_policy_publication
  fi
  mv -- "$temporary" "$destination"
  sync "$destination" "$(dirname "$destination")"
}

case "$MODE" in
  approve)
    [ "$#" = 9 ] && [ "$9" = --authority-aware-artifact-verified ] || die 'usage=approve_BINARY_SHA256_SOURCE_SHA_WAL_DIR_RECEIPT_COPY_VALIDATION_EVIDENCE_INSPECTOR_INSPECTOR_SHA256_--authority-aware-artifact-verified'
    BINARY=$1; BINARY_SHA=$2; SOURCE_SHA=$3; WAL_DIR=$4; RECEIPT=$5; EVIDENCE=$6; INSPECTOR=$7; INSPECTOR_SHA=$8
    [[ "$BINARY_SHA" =~ ^[0-9a-f]{64}$ && "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || die invalid_artifact_identity
    for source_file in "$BINARY" "$RECEIPT" "$EVIDENCE" "$INSPECTOR"; do tv_guard_secure_path "$source_file" regular 0; done
    tv_guard_secure_path "$BINARY" executable 0
    tv_guard_secure_path "$INSPECTOR" executable 0
    digest_is "$BINARY" "$BINARY_SHA"
    digest_is "$INSPECTOR" "$INSPECTOR_SHA"
    (( $(stat -c %s "$RECEIPT") <= 16384 && $(stat -c %s "$EVIDENCE") > 0 && $(stat -c %s "$EVIDENCE") <= 1048576 )) || die evidence_size
    # This is a fresh local structural probe, not discovery/certification of
    # the external database/history attestation or code compatibility.
    report=$(timeout 20s "$INSPECTOR" --inspect --wal-dir "$WAL_DIR") || die authority_inspection_refused
    printf '%s\n' "$report" | jq -e --arg wal "$WAL_DIR" '.mode == "inspect" and .namespace_state == "valid_durable_authority" and .authority_structurally_valid == true and .wal_directory == $wal and .complete_namespace_verified == false and .migration_applied == false and .deployment_authorized == false' >/dev/null || die authority_report_unverified
    # Required typed, namespace-bound receipt verification. In particular, jq
    # never parses, compares or stringifies the >2^53 capture identities.
    # The Rust verifier checks the exact retained receipt bytes, u64 bounds,
    # real manifest codec and existing-only exclusive namespace ownership.
    receipt_report=$(timeout 20s "$INSPECTOR" --verify-receipt --wal-dir "$WAL_DIR" --receipt-copy "$RECEIPT") || die receipt_verification_refused
    printf '%s\n' "$receipt_report" | jq -e --arg wal "$WAL_DIR" '.mode == "verify-receipt" and .receipt_verified == true and .wal_directory == $wal and .caller_attested_complete_namespace == true and .external_namespace_verified == false and .existing_owner_lock_acquired == true and .sequence_authority_written == false and .deployment_authorized == false' >/dev/null || die receipt_report_unverified
    tv_guard_namespace "$WAL_DIR"
    tv_guard_sha "$RECEIPT"; RECEIPT_SHA=$TV_DIGEST
    tv_guard_sha "$EVIDENCE"; EVIDENCE_SHA=$TV_DIGEST
    temporary=$(mktemp "$STATE/.policy-source.XXXXXX")
    printf 'TVSG1\n%s\ntvsq-v1\n%s\n' "$WAL_DIR" "$RECEIPT_SHA" > "$temporary"
    tv_guard_sha "$temporary"; POLICY_SHA=$TV_DIGEST
    write_once "$RECEIPT" "$STATE/migration-receipt.json" "$RECEIPT_SHA"
    write_once "$temporary" "$STATE/policy" "$POLICY_SHA"
    rm -- "$temporary"
    write_once "$EVIDENCE" "$STATE/evidence/$EVIDENCE_SHA.evidence" "$EVIDENCE_SHA"
    temporary=$(mktemp "$STATE/.permit-source.XXXXXX")
    printf 'TVSG1\n%s\n%s\n%s\n%s\n%s\n' "$WAL_DIR" "$SOURCE_SHA" "$BINARY_SHA" "$POLICY_SHA" "$EVIDENCE_SHA" > "$temporary"
    tv_guard_sha "$temporary"; PERMIT_SHA=$TV_DIGEST
    write_once "$temporary" "$STATE/approved/$BINARY_SHA.permit" "$PERMIT_SHA"
    rm -- "$temporary"
    tv_guard_verify "$STATE" "$BINARY" 0 "$BINARY_SHA" "$SOURCE_SHA" "$WAL_DIR" yes
    printf 'TV_START_GUARD_ARTIFACT_APPROVED_FENCED sha256=%s source_sha=%s\n' "$BINARY_SHA" "$SOURCE_SHA"
    ;;
  release)
    [ "$#" = 4 ] && [ "$4" = --offline-verification-complete ] || die 'usage=release_SHA256_SOURCE_SHA_WAL_DIR_--offline-verification-complete'
    tv_guard_verify "$STATE" "$TV_GUARD_BINARY" 0 "$1" "$2" "$3" yes
    # Retain the fence record as evidence. This is the only unfence operation;
    # it never starts the app, lowers authority or deletes a policy/permit.
    destination=$STATE/maintenance-released-$(date -u +%Y%m%dT%H%M%SZ)-$$
    [ ! -e "$destination" ] && [ ! -L "$destination" ] || die release_record_exists
    mv -- "$STATE/maintenance" "$destination"
    if ! sync "$destination" "$STATE"; then
      if ! tv_guard_fence "$STATE" 0 release_durability_unverified; then
        printf 'TV_START_GUARD_FENCE_RESTORE_UNVERIFIED keep_managed_service_stopped=true\n' >&2
      fi
      die release_durability_unverified
    fi
    printf 'TV_START_GUARD_RELEASED sha256=%s source_sha=%s service_started=false\n' "$TV_BINARY_SHA" "$TV_SOURCE_SHA"
    ;;
  *) die usage=install_or_fence_or_approve_or_release ;;
esac
