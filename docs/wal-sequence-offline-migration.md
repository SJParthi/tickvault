# WAL sequence authority: inspection, offline migration and rollback

The durable sequence issuer is a forward-only identity migration. A legacy
writer can retain WAL without `sequence.tvsq` or `sequence.initialized`; it
does not become compatible merely because somebody creates those files.
Starting that unaware writer after migration can issue identities outside the
committed authority and invalidate the new writer's uniqueness assumptions.

This document specifies the required operator contract. The read-only CLI and
[persistent start guard](wal-sequence-start-guard.md) are implemented in source.
**Their presence in source does not prove installation or enforcement on the
actual host.** Do not apply the migration until the persistent fence and
compatibility gate have been installed and verified there.

## Read-only inspection while the old writer is running

Build the operator binary from the reviewed, tested source:

```sh
cargo build -p tickvault-storage --release --locked --bin tv-wal-sequence-migrate
timeout 15s ./target/release/tv-wal-sequence-migrate --inspect --wal-dir /actual/owned/wal
```

Use the actual namespace path; the example is a placeholder. Inspection needs
no evidence file and cannot be combined with `--apply` or `--evidence`.
It does not acquire or create `.wal-owner.lock`, initialize a manifest,
reserve a sequence, scan WAL payloads, rename files or query the database.

| JSON `namespace_state` | Observation | Exit |
|---|---|---|
| `fresh_empty` | Root is locally empty or holds only a regular directory-lock file | 0: local preflight only |
| `valid_durable_authority` | Manifest and marker match the canonical directory and real manifest codec, the lock file exists, and reservation headroom remains | 0: local preflight only |
| `migration_required` | Stable observed WAL or other local history without either authority file | 2 |
| `incomplete_authority` | Manifest/marker pair incomplete, lock file absent beside authority, or reservation temporary remains | 2 |
| `corrupt_authority` | Invalid size, magic/version, CRC, supported bound, directory digest or marker | 2 |
| `unsafe_path` | Observed symlink, special file, multiply-linked file or non-directory root | 2 |
| `unverified` | Missing/unreadable scope, unknown nested directory, entry/time budget reached, detected concurrent change, unsupported platform, or insufficient startup headroom | 2 |

The JSON includes root/replaying/archive/quarantine WAL counts, other observed
history entries, and the exact reserved bound as decimal text when validated.
It always states `writer_exclusion_verified=false`,
`complete_namespace_verified=false`, `deployment_authorized=false`,
`migration_applied=false`, and `retained_wal_validation="not_run"`.

Two metadata inventories bracket two bounded authority reads. They compare
inodes, device, type, length, link count, modification/change times and authority
bytes. Appends, replacement or partial reads observed during the pass are not
reported as a clean state. A stable observation does not establish quiescence,
fsync durability or a complete namespace; it cannot rule out changes between
observations or an ABA race. Each of the two inventories is capped at 4096
entries, including the root. Metadata ordering costs O(E log E), with a
cooperative five-second budget. An external timeout is still required for
a blocked kernel/filesystem call. A timeout is an unverified preflight, not a
successful inspection.

An empty local directory does not prove that a database/feed namespace is new:
older rows, rescue tiers, prior WAL roots or backups can still contain identities.
Likewise, a valid manifest does not prove that an old unaware process stopped
using another allocator. Both facts require separate evidence.

## Persistent maintenance and compatibility fence

Before stopping the legacy writer, establish a root-owned persistent fence
outside the checkout, release directory, ephemeral `/run`, and rollback-owned
paths. The implemented location is
`/var/lib/tickvault/start-guard/maintenance`; permanent `policy`, `approved/`
and `evidence/` records in the same directory bind the namespace, exact
artifacts, migration receipt and operator evidence, without credentials.
Sync files and parents before relying on them across restart. See the
start-guard contract for its exact installer/approval/release interfaces.

Every start route must enforce the same contract:

| Start route | Required behavior |
|---|---|
| systemd normal boot and `Restart=` | Refuse while maintenance fence exists, before any application process starts |
| deployment and smoke-test retry | Observe the persistent fence; cannot clear it as generic cleanup |
| rollback | Retain the fence and do not start an unaware previous binary |
| operator start/restart, AWS autopilot, host resize/reboot, timer/holiday paths | Start only through the guarded unit; no alternate unit or direct background executable |
| restore scripts/unit replacement | Preserve the guard/drop-in and permanent compatibility record outside rollback-owned paths |

A service condition or guard must be installed independently of the executable
that is being replaced. Otherwise restoring the old executable/service can
also restore the bypass. Test blocked manual start, restart, reboot/autostart,
deployment retry and rollback before calling the fence enforced. Merely
stopping or temporarily masking a unit does not prove a durable fence.

Maintain a separate **permanent compatibility record** after migration. Clearing
the temporary maintenance fence must not authorize an old binary later.
All subsequent starts must verify that the exact artifact belongs to the
approved authority-aware release set and uses the same owned namespace.
Do not infer compatibility from a claimed build string or from the presence
of `sequence.tvsq` alone. Directly launching an executable outside the guarded
service violates this operational contract.

## Offline migration sequence

1. Record the current process, artifact, configuration, unit/drop-ins and actual
   canonical namespace. Preserve a recoverable snapshot of WAL, staged/archive/
   quarantine, rescue data and the authoritative database/history scope.
2. Install and durably activate the persistent maintenance fence. Verify all
   automatic/manual starters refuse. Stop the old writer and confirm the unit
   is inactive and no writer/helper or alternative service still owns capture.
   Do not equate one PID disappearing with proof that every writer is gone.
3. Re-run the read-only inspection under an external timeout. Resolve unsafe,
   corrupt, partial or unverified states. Do not delete a marker or manifest,
   move history aside, or point at a fresh directory to manufacture a pass.
4. Establish a conservative capture bound across the complete database/feed
   identity namespace: relevant live and historical database tables, rescue/
   backup tiers, retained WAL and previous roots. Record scope and verifier.
   A MAX over only retained database rows is insufficient when history was
   pruned. If complete evidence is unavailable, leave the service fenced.
5. Prepare the strict evidence JSON required by `--evidence`. The
   `complete_namespace_verified: true` field is an operator attestation backed
   by step 4; the command does not discover or certify that external evidence.
   Run the no-`--apply` mode to validate arguments/evidence and exclusive
   ownership. It is **not** a retained-WAL validation or migration success.
6. With the fence still active, invoke `--apply`. The existing migration engine
   holds the directory guard, completely scans recognized retained WAL for
   integrity/sequence bounds, refuses insufficient evidence or an existing
   authority, and syncs its receipt, initialization marker and reservation
   manifest. Retain the receipt and source evidence. No retained source is
   deleted to complete this step.
7. Inspect and verify the resulting authority and receipt against the same
   canonical root and evidence. Install/validate the permanent compatibility
   start gate and exact approved candidate before allowing a new writer.
   Complete the separate candle-schema/recovery deployment prerequisites.
8. Clear the temporary fence only as an explicit, recorded release action;
   retain the permanent compatibility gate. Start the approved writer through
   systemd, verify its actual process/artifact, readiness, identity reservation
   behavior, pending recovery and applied storage. Leave conflicts visible.

This process requires a planned maintenance interval. A provider feed gap
while the application is stopped is not ruled out by local WAL preservation.
Do not claim a zero-tick-loss or full replay outcome from migration success.

## Failure and rollback rules

- Before authority application, restore only with the maintenance fence still
  controlling all starts. Release it only after choosing and validating one
  coherent writer/data/configuration state.
- After authority application, the identity floor is forward-only. Never
  delete, reset, replace with an older backup, or lower the manifest/marker to
  make a startup work. A valid receipt is not permission to reuse IDs.
- A prior binary that does not honor this authority is not an acceptable
  successful rollback. Restore code/configuration for inspection if necessary,
  but keep the service stopped and fenced; use a reviewed authority-aware
  forward fix or an explicitly planned compatible migration.
- Failure after new capture must preserve new WAL and reservations. Restoring
  an older database or WAL snapshot alone can rewind part of the namespace;
  reconcile the complete scope before any writer is allowed to resume.
- A changed namespace path changes the directory binding. Copying its manifest
  into another root is not a migration. The old and new namespaces cannot
  concurrently write the same capture identity domain.
- A lost connection, interrupted command or partial receipt is an unverified
  state. Retain the fence, inspect the durable result, and reconcile it; do not
  blindly rerun destructive cleanup or announce that rollback recovered service.

## Validation scope

The inspector tests real authority bytes created by the current issuer, holds
the real writer lock while inspecting, proves no reservation/manifest change,
and covers legacy tiers, partial/corrupt/copied authority, unsafe entries,
bounded scope and injected concurrent changes. These are local regression
tests. Systemd-fence behavior, complete external namespace evidence, stopped
production writers, actual offline migration and post-start readiness remain
separate operational verification requirements.
