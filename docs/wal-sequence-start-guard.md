# Persistent managed-start fence and artifact compatibility

The guard implementation is present in source. It has not thereby been
installed or activated on AWS. The legacy production writer must remain an
explicit migration blocker until the offline protocol and actual host checks
are complete. This mechanism governs managed systemd starts; it cannot stop
a privileged operator deliberately bypassing the unit or editing root-owned
policy. Direct/external writers remain outside the supported contract.

| Installed object | Purpose |
|---|---|
| `/usr/local/libexec/tickvault-start-guard` | Root-owned admission and pinned-inode execution, outside the repository and binary rollback paths |
| `/etc/systemd/system/tickvault.service.d/90-sequence-authority-guard.conf` | Persistent condition plus ExecStart override, retained when an old base unit is restored |
| `/var/lib/tickvault/start-guard/maintenance` | Durable temporary stop fence; its existence refuses admission |
| `start-guard/start.lock` | Shared across a managed start and the running process; exclusive ownership required for policy changes and release |
| `start-guard/policy` and `migration-receipt.json` | Permanent namespace/version binding and the preserved exact migration receipt |
| `start-guard/approved/<binary-sha256>.permit` | Exact artifact/source/namespace/policy/evidence binding; no implicit approval from a build label |
| `start-guard/evidence/<sha256>.evidence` | Preserved operator validation evidence without credentials |

The base unit and independent drop-in run `check` as an `ExecCondition`, before
QuestDB or any other `ExecStartPre`. The drop-in overrides the old `ExecStart`
with `run`, which verifies again, opens and hashes the executable inode, and
executes that same fd with the expected argv[0]. The shared start lock remains
open through exec. The approved namespace is exported as `TV_WS_WAL_DIR`.
The fixed `/bin/bash -p` interpreter ignores `BASH_ENV` and imported shell
functions before the guard sets its fixed command path.

All policy/artifact paths must be canonical, with no symlinks, special files,
multiple file links, unexpected owners or writable ancestors. Root-owned
sticky temporary directories are permitted only with trusted descendants.
State files are root-owned mode0644 and directories mode0755 so ec2-user can
read admission evidence and the lock, but cannot alter it. Runtime-owned WAL
files are a separate namespace: the guard checks authority presence and
directory binding; the Rust inspector and issuer provide CRC/bound checks.
The shell admission result is not a full WAL or database-history proof.

## Read-only deployment interfaces

```text
/usr/local/libexec/tickvault-start-guard check
/usr/local/libexec/tickvault-start-guard admit --binary ABS_PATH --sha256 HEX64 --source-sha HEX40 --wal-dir ABS_PATH
```

`check` admits the current `/opt/tickvault/bin/tickvault`. `admit` checks a
root-private staged candidate against an existing exact permit. Both refuse
maintenance and return exit2 on failure. Success prints one line beginning
`TV_START_GUARD_ALLOWED`, followed by the verified artifact hash, source SHA
and namespace. Neither operation creates or approves state.

Before invoking an installed helper, deployment verifies that its bytes and
the independent drop-in match the expected reviewed source hashes and are
root-owned/non-writable. It stages and SHA-verifies the candidate app and Rust
inspector before changing the repository, configuration, binary or service.
The Rust namespace result must be `valid_durable_authority`, followed by
successful candidate `admit`. Also run installed `check` as the unit's actual
ec2-user account so a root-only readable policy cannot masquerade as runtime
admission. `migration_required`, missing installation,
unapproved candidate, maintenance or an uncertain read stops normal deploy.

Normal deployment never calls `approve` or `release`. Restoring files does
not restore compatibility: rollback checks the restored binary before starting
it. Failed admission leaves/sets the persistent maintenance fence and reports
an incomplete rollback. The guard, drop-in, permanent policy and lock are
excluded from rollback-owned paths. Cleanup never removes them or unfences.

AWS Control, downsize/reboot, systemd auto-restart and manual managed starts
all enter the same unit condition. Autopilot also probes admission before
resetting a start limit; missing policy/helper is a refusing observation, and
maintenance is reported as intentional. A new fence racing that first probe
still wins at the unit condition and final `run` check.

## Explicit offline installation and approval

The operator tool is `scripts/tickvault-start-guard-admin.sh`. Stage its
reviewed sources privately as root and invoke it directly through its fixed
shebang. Do not source unreviewed code or use runtime-owned staging paths.
Argument placeholders below describe a protocol, not ready-made evidence.

1. Run `install GUARD_SOURCE GUARD_SHA256 DROPIN_SOURCE DROPIN_SHA256`.
   It verifies the source hashes/paths, creates and syncs the fence, installs
   the independent helper/drop-in, then reloads systemd. It does not stop or
   approve the old process. It makes `/opt/tickvault`, `bin`, and the three
   deployment binaries root-owned mode0755; data/config/repo child ownership
   stays intact. Test that managed starts refuse before proceeding.
2. Stop the managed service and all other writers. Confirm the unit is
   inactive, MainPID is zero, and no external writer survives. Preserve WAL,
   backups, rescue tiers and authoritative database history. Follow
   [the offline migration contract](wal-sequence-offline-migration.md).
3. Establish the complete external namespace bound and apply the reviewed
   Rust migration under exclusive existing namespace ownership. Retain its
   receipt and original evidence. Never create a manifest manually, reset an
   authority, move history aside, or attest completeness from a current MAX
   alone. If evidence is incomplete, remain fenced.
4. Review and install the exact compatible application/configuration/schema
   candidate while still fenced. Preserve its source/artifact SHA and the
   actual test and compatibility evidence. The first authority transition is
   an explicit offline cutover: normal deploy intentionally cannot pass an
   active fence or missing permanent policy.
5. Invoke `approve BINARY SHA256 SOURCE_SHA WAL_DIR RECEIPT_COPY
   VALIDATION_EVIDENCE INSPECTOR INSPECTOR_SHA256
   --authority-aware-artifact-verified`. The operator explicitly attests code
   compatibility; the command does not infer it from a source string. It
   requires maintenance, an inactive managed unit, exclusive start-lock
   ownership, exact artifact/inspector hashes and a fresh Rust authority
   inspection. The typed receipt verifier must bind the preserved copy to
   the real receipt in that namespace and validate exact integer bounds using
   `--verify-receipt --wal-dir WAL_DIR --receipt-copy RECEIPT_COPY`.
   This does not re-prove the receipt's external completeness attestation.
6. Approval publishes immutable root-owned receipt/policy/evidence/permit
   records and syncs their files and parents. Identical retries are accepted;
   conflicting originals remain and cause refusal. It never clears the fence.
7. After the actual offline deployment checks pass, run `release SHA256
   SOURCE_SHA WAL_DIR --offline-verification-complete`. It requires the
   installed candidate to pass admission, keeps the permanent gate, retains
   the old fence record as evidence and syncs the release. It does not start
   the app. Explicitly start through systemd and verify the actual executable,
   readiness, applied storage, namespace reservation and pending recovery.

Future compatible releases are possible without replacing the permanent
policy: under a planned fence and stopped managed writer, add a new exact
artifact permit using the same verified namespace/receipt. A currently
approved compatible binary can be released again before normal deployment
of the newly approved candidate. This trades a maintenance interval for
explicit compatibility approval. A new incompatible storage/authority
generation requires its own reviewed migration protocol.

## Strict record format and failure rules

The policy is exactly four newline-terminated lines: `TVSG1`, canonical WAL
directory, `tvsq-v1`, and the receipt SHA256. A permit is exactly six lines:
`TVSG1`, the same namespace, source SHA40, artifact SHA256, policy SHA256 and
validation-evidence SHA256. These are data records, never shell code. Unknown
lines, NULs, missing newlines, conflicting evidence or malformed digests refuse.

`fence --reason TOKEN` on the installed helper is root-only, creates or
preserves the marker and syncs it. It cannot release maintenance. Do not
delete `start.lock` to evade an inherited lock; a still-running child may
retain the managed claim and must be investigated. Do not delete policy to
permit a fresh bootstrap; missing state refuses startup. An old unaware
binary is never an acceptable post-migration rollback merely because it ran
before. Retain the fence and use an authority-aware forward fix.

Filesystem I/O or a sync failure is an unverified operation, not a migration
or rollback success. Preserve evidence and investigate. The guard cannot
promise physical durability after arbitrary hardware failure or prevent a
privileged operator bypass. Cold admission hashes artifact/evidence bytes and
checks paths; its work scales with those sizes and does not run per tick.

## Validation scope

`bash scripts/test-tickvault-start-guard.sh` passed 29 local Linux cases,
including filesystem and harmless-process checks. It runs
without AWS, systemctl or the real application. The Linux root fixture also
executes a copied harmless ELF through the pinned fd, checks argv[0] and the
ELF identity through its own fd, verifies shared-lock lifetime, and probes
`BASH_ENV` refusal. Portable fixtures test real filesystem metadata against
their fixture owner; the installed executable fixes the owner to root.
`cargo test -p tickvault-common --test start_guard_contract` runs the fixture
and the independent-unit wiring checks. Actual AWS installation, restart/
reboot behavior, offline migration, external completeness and final deployed
readiness still require their own evidence.
