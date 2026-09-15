#!/bin/bash -p
# Persistent managed-start admission. Installed outside release/rollback paths.
# Sourceable functions are used by isolated filesystem tests; the executable
# interface always fixes the policy owner to root and accepts no environment
# override of its state, installed guard, drop-in or production executable.

TV_GUARD_STATE=/var/lib/tickvault/start-guard
TV_GUARD_BINARY=/opt/tickvault/bin/tickvault
TV_GUARD_INSTALLED=/usr/local/libexec/tickvault-start-guard
TV_GUARD_DROPIN=/etc/systemd/system/tickvault.service.d/90-sequence-authority-guard.conf

tv_guard_refuse() {
  printf 'TV_START_GUARD_REFUSED %s\n' "$*" >&2
  return 2
}

tv_guard_tools() {
  # GNU tools are required on the AWS host. A real GNU coreutils installation
  # also permits the same filesystem fixture to run on a developer Mac.
  TV_STAT=stat
  if ! stat --version >/dev/null 2>&1; then
    command -v gstat >/dev/null 2>&1 || { tv_guard_refuse gnu_stat_missing; return 2; }
    TV_STAT=gstat
  fi
  TV_REALPATH=realpath
  if ! realpath --version >/dev/null 2>&1; then
    command -v grealpath >/dev/null 2>&1 || { tv_guard_refuse gnu_realpath_missing; return 2; }
    TV_REALPATH=grealpath
  fi
  command -v sha256sum >/dev/null 2>&1 || { tv_guard_refuse sha256sum_missing; return 2; }
  command -v timeout >/dev/null 2>&1 || { tv_guard_refuse timeout_missing; return 2; }
}

# Reject textual aliases and every symlink, unsafe owner or writable prefix.
# Root-owned sticky directories such as /tmp are safe only because every
# descendant checked here must itself belong to the trusted owner or root.
tv_guard_secure_path() {
  local path=$1 kind=$2 owner=$3 canonical part cursor= uid mode links size
  [[ "$path" =~ ^/[A-Za-z0-9._/-]+$ ]] || { tv_guard_refuse invalid_absolute_path; return 2; }
  canonical=$("$TV_REALPATH" -e -- "$path") || { tv_guard_refuse "path_unreadable=$path"; return 2; }
  [ "$canonical" = "$path" ] || { tv_guard_refuse "path_alias_or_symlink=$path"; return 2; }
  local old_ifs=$IFS
  IFS=/ read -r -a TV_PATH_PARTS <<< "${path#/}"
  IFS=$old_ifs
  for part in "${TV_PATH_PARTS[@]}"; do
    cursor=$cursor/$part
    [ ! -L "$cursor" ] || { tv_guard_refuse "symlink=$cursor"; return 2; }
    read -r uid mode links size < <("$TV_STAT" -c '%u %a %h %s' -- "$cursor")
    [[ "$uid" =~ ^[0-9]+$ && "$mode" =~ ^[0-7]+$ && "$links" =~ ^[0-9]+$ && "$size" =~ ^[0-9]+$ ]] || { tv_guard_refuse metadata_unreadable; return 2; }
    if [ "$cursor" != "$path" ] || [ "$kind" = directory ]; then
      [ -d "$cursor" ] && { [ "$uid" = 0 ] || [ "$uid" = "$owner" ]; } || { tv_guard_refuse "unsafe_directory=$cursor"; return 2; }
      if (( (8#$mode & 0022) != 0 )); then
        [ "$uid" = 0 ] && (( (8#$mode & 01000) != 0 )) || { tv_guard_refuse "writable_directory=$cursor"; return 2; }
      fi
    else
      [ -f "$cursor" ] && [ "$uid" = "$owner" ] && [ "$links" = 1 ] && (( (8#$mode & 0022) == 0 )) || { tv_guard_refuse "unsafe_regular_file=$cursor"; return 2; }
      [ "$kind" != executable ] || [ -x "$cursor" ] || { tv_guard_refuse artifact_not_executable; return 2; }
    fi
  done
}

tv_guard_sha() {
  local result
  result=$(timeout 20s sha256sum -- "$1") || { tv_guard_refuse digest_unverified; return 2; }
  TV_DIGEST=${result%% *}
  [[ "$TV_DIGEST" =~ ^[0-9a-f]{64}$ ]] || { tv_guard_refuse digest_malformed; return 2; }
}

# Strict records, not executable shell and not permissive key/value parsing.
# Each record ends with a newline and has exactly the documented line count.
tv_guard_record() {
  local path=$1 count=$2 owner=$3 line bytes
  tv_guard_secure_path "$path" regular "$owner" || return 2
  bytes=$("$TV_STAT" -c %s -- "$path") || return 2
  (( bytes > 0 && bytes <= 16384 )) || { tv_guard_refuse record_size; return 2; }
  TV_RECORD=()
  while IFS= read -r line; do
    [[ "$line" =~ ^[A-Za-z0-9._/:-]+$ ]] || { tv_guard_refuse record_character; return 2; }
    TV_RECORD+=("$line")
    (( ${#TV_RECORD[@]} <= count )) || { tv_guard_refuse record_extra_lines; return 2; }
  done < "$path"
  [ "${#TV_RECORD[@]}" = "$count" ] && [ "${TV_RECORD[0]}" = TVSG1 ] || { tv_guard_refuse record_shape; return 2; }
  # read strips NUL bytes; comparing a normalized digest also rejects NULs,
  # a missing final newline and a trailing partial line.
  local raw normalized
  tv_guard_sha "$path" || return 2; raw=$TV_DIGEST
  normalized=$(printf '%s\n' "${TV_RECORD[@]}" | sha256sum) || return 2
  [ "${normalized%% *}" = "$raw" ] || { tv_guard_refuse noncanonical_record; return 2; }
}

tv_guard_namespace() {
  local root=$1 canonical marker expected
  [[ "$root" =~ ^/[A-Za-z0-9._/-]+$ ]] || { tv_guard_refuse invalid_namespace; return 2; }
  canonical=$("$TV_REALPATH" -e -- "$root") || { tv_guard_refuse namespace_unreadable; return 2; }
  [ "$canonical" = "$root" ] && [ -d "$root" ] && [ ! -L "$root" ] || { tv_guard_refuse namespace_alias; return 2; }
  # These are owned by the runtime writer, not by this root policy. This is
  # presence/directory-binding admission, not CRC/durability/history proof.
  local name wanted links bytes
  for name in sequence.tvsq sequence.initialized; do
    [ -f "$root/$name" ] && [ ! -L "$root/$name" ] || { tv_guard_refuse authority_missing_or_unsafe; return 2; }
    read -r links bytes < <("$TV_STAT" -c '%h %s' -- "$root/$name")
    wanted=52; [ "$name" != sequence.initialized ] || wanted=32
    [ "$links" = 1 ] && [ "$bytes" = "$wanted" ] || { tv_guard_refuse authority_incomplete; return 2; }
  done
  [ ! -e "$root/sequence.tvsq.tmp" ] && [ ! -L "$root/sequence.tvsq.tmp" ] || { tv_guard_refuse authority_refill_unverified; return 2; }
  marker=$(od -An -v -tx1 -- "$root/sequence.initialized" | tr -d ' \n') || return 2
  expected=$(printf '%s' "$root" | sha256sum) || return 2
  [ "$marker" = "${expected%% *}" ] || { tv_guard_refuse authority_directory_mismatch; return 2; }
}

# Owner is a library-test parameter only. The executable always supplies 0.
# An existing maintenance fence wins even when every other input is valid.
tv_guard_verify() {
  local state=$1 binary=$2 owner=$3 expected_sha=$4 expected_source=$5 expected_wal=$6 ignore_fence=${7:-no}
  local policy_sha receipt_sha evidence_sha permit_source permit_sha permit_policy permit_wal actual
  tv_guard_tools || return 2
  tv_guard_secure_path "$state" directory "$owner" || return 2
  if [ -e "$state/maintenance" ] || [ -L "$state/maintenance" ]; then
    tv_guard_secure_path "$state/maintenance" regular "$owner" || return 2
    [ "$ignore_fence" = yes ] || { tv_guard_refuse maintenance_fenced; return 2; }
  fi
  tv_guard_record "$state/policy" 4 "$owner" || return 2
  TV_WAL_DIR=${TV_RECORD[1]}; receipt_sha=${TV_RECORD[3]}
  [ "${TV_RECORD[2]}" = tvsq-v1 ] && [[ "$receipt_sha" =~ ^[0-9a-f]{64}$ ]] || { tv_guard_refuse policy_format; return 2; }
  [ -z "$expected_wal" ] || [ "$TV_WAL_DIR" = "$expected_wal" ] || { tv_guard_refuse candidate_namespace_mismatch; return 2; }
  tv_guard_sha "$state/policy" || return 2; policy_sha=$TV_DIGEST
  tv_guard_secure_path "$state/migration-receipt.json" regular "$owner" || return 2
  tv_guard_sha "$state/migration-receipt.json" || return 2
  [ "$TV_DIGEST" = "$receipt_sha" ] || { tv_guard_refuse migration_receipt_changed; return 2; }
  tv_guard_namespace "$TV_WAL_DIR" || return 2
  tv_guard_secure_path "$binary" executable "$owner" || return 2
  tv_guard_sha "$binary" || return 2; TV_BINARY_SHA=$TV_DIGEST
  [ -z "$expected_sha" ] || [ "$TV_BINARY_SHA" = "$expected_sha" ] || { tv_guard_refuse artifact_digest_mismatch; return 2; }
  tv_guard_record "$state/approved/$TV_BINARY_SHA.permit" 6 "$owner" || return 2
  permit_wal=${TV_RECORD[1]}; permit_source=${TV_RECORD[2]}; permit_sha=${TV_RECORD[3]}; permit_policy=${TV_RECORD[4]}; evidence_sha=${TV_RECORD[5]}
  [[ "$permit_source" =~ ^[0-9a-f]{40}$ && "$permit_sha" =~ ^[0-9a-f]{64}$ && "$permit_policy" =~ ^[0-9a-f]{64}$ && "$evidence_sha" =~ ^[0-9a-f]{64}$ ]] || { tv_guard_refuse permit_format; return 2; }
  [ "$permit_wal" = "$TV_WAL_DIR" ] && [ "$permit_sha" = "$TV_BINARY_SHA" ] && [ "$permit_policy" = "$policy_sha" ] || { tv_guard_refuse permit_binding_mismatch; return 2; }
  [ -z "$expected_source" ] || [ "$permit_source" = "$expected_source" ] || { tv_guard_refuse candidate_source_mismatch; return 2; }
  tv_guard_secure_path "$state/evidence/$evidence_sha.evidence" regular "$owner" || return 2
  tv_guard_sha "$state/evidence/$evidence_sha.evidence" || return 2
  [ "$TV_DIGEST" = "$evidence_sha" ] || { tv_guard_refuse artifact_evidence_changed; return 2; }
  TV_SOURCE_SHA=$permit_source
  # Recheck after the bounded reads; policy changes are permitted only under
  # maintenance. The run path additionally holds the shared start lock.
  if [ "$ignore_fence" != yes ] && { [ -e "$state/maintenance" ] || [ -L "$state/maintenance" ]; }; then
    tv_guard_refuse maintenance_fenced; return 2
  fi
  tv_guard_sha "$state/policy" || return 2
  [ "$TV_DIGEST" = "$policy_sha" ] || { tv_guard_refuse policy_changed; return 2; }
}

tv_guard_fence() {
  local state=$1 owner=$2 reason=$3
  [[ "$reason" =~ ^[A-Za-z0-9_.:-]{1,128}$ ]] || { tv_guard_refuse invalid_fence_reason; return 2; }
  tv_guard_tools || return 2
  tv_guard_secure_path "$state" directory "$owner" || return 2
  if [ -e "$state/maintenance" ] || [ -L "$state/maintenance" ]; then
    tv_guard_secure_path "$state/maintenance" regular "$owner" || return 2
  else
    (umask 022; set -o noclobber; printf 'TVSG1\n%s\n' "$reason" > "$state/maintenance") || { tv_guard_refuse fence_creation_unverified; return 2; }
  fi
  sync "$state/maintenance" "$state" || { tv_guard_refuse fence_durability_unverified; return 2; }
  printf 'TV_START_GUARD_FENCED reason=%s\n' "$reason"
}

tv_guard_main() {
  set -euo pipefail
  export LC_ALL=C
  export PATH=/usr/sbin:/usr/bin:/sbin:/bin
  local command=${1:-} binary=$TV_GUARD_BINARY expected_sha= expected_source= expected_wal= reason=
  [ "$#" -gt 0 ] && shift
  case "$command" in
    check|run) [ "$#" = 0 ] || { tv_guard_refuse unexpected_arguments; return 2; } ;;
    admit)
      [ "$#" = 8 ] || { tv_guard_refuse admit_requires_four_exact_flags; return 2; }
      [ "$1" = --binary ] && [ "$3" = --sha256 ] && [ "$5" = --source-sha ] && [ "$7" = --wal-dir ] || { tv_guard_refuse invalid_admit_flags; return 2; }
      binary=$2; expected_sha=$4; expected_source=$6; expected_wal=$8
      [[ "$expected_sha" =~ ^[0-9a-f]{64}$ && "$expected_source" =~ ^[0-9a-f]{40}$ ]] || { tv_guard_refuse invalid_candidate_identity; return 2; }
      ;;
    fence)
      [ "$EUID" = 0 ] && [ "$#" = 2 ] && [ "$1" = --reason ] || { tv_guard_refuse fence_requires_root_and_reason; return 2; }
      tv_guard_fence "$TV_GUARD_STATE" 0 "$2"; return
      ;;
    *) tv_guard_refuse 'usage=check|run|admit_--binary_PATH_--sha256_HASH_--source-sha_SHA_--wal-dir_PATH|fence_--reason_TOKEN'; return 2 ;;
  esac
  tv_guard_tools
  tv_guard_secure_path "$TV_GUARD_INSTALLED" executable 0
  tv_guard_secure_path "$TV_GUARD_DROPIN" regular 0
  tv_guard_secure_path "$TV_GUARD_STATE/start.lock" regular 0
  command -v flock >/dev/null 2>&1 || { tv_guard_refuse flock_missing; return 2; }
  exec 8< "$TV_GUARD_STATE/start.lock"
  flock --shared --timeout 5 8 || { tv_guard_refuse start_lock_unverified; return 2; }
  tv_guard_verify "$TV_GUARD_STATE" "$binary" 0 "$expected_sha" "$expected_source" "$expected_wal"
  printf 'TV_START_GUARD_ALLOWED sha256=%s source_sha=%s wal_dir=%s\n' "$TV_BINARY_SHA" "$TV_SOURCE_SHA" "$TV_WAL_DIR"
  if [ "$command" = run ]; then
    # Pin the approved inode, not a pathname another managed deploy can rename.
    # Root-owned non-writable prefixes and file permissions prevent the runtime
    # user from replacing/mutating it. The shared lock stays with this process.
    exec 9< "$binary"
    local opened_hash
    opened_hash=$(timeout 20s sha256sum /proc/self/fd/9)
    [ "${opened_hash%% *}" = "$TV_BINARY_SHA" ] || { tv_guard_refuse opened_artifact_changed; return 2; }
    [ ! -e "$TV_GUARD_STATE/maintenance" ] && [ ! -L "$TV_GUARD_STATE/maintenance" ] || { tv_guard_refuse maintenance_fenced; return 2; }
    export TV_WS_WAL_DIR=$TV_WAL_DIR
    exec -a "$TV_GUARD_BINARY" /proc/self/fd/9
  fi
}

if [[ "${BASH_SOURCE[0]}" = "$0" ]]; then
  tv_guard_main "$@"
fi
