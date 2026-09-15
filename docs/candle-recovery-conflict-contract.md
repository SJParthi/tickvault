# Candle recovery conflict contract

This source change replaces blind last-write-wins spill replay with bounded
boot reconciliation. It does not order revisions across process restarts:
`MultiTfAggregator` initializes each new instrument slot's `volume_revision`
to zero, and persisted `bucket_revision` inherits that counter.

| Observed state | Recovery action | Original file |
|---|---|---|
| Key absent after the WAL apply barrier | Insert, wait for WAL application, read back every persisted field | Archive only if every record is confirmed |
| Complete row already identical | Skip the write and count an identical confirmation | Archive only if every record is confirmed |
| Original 18 fields identical; incoming legacy provenance unknown; all 7 newer DB fields NULL | Skip without modifying any stored value; leave legacy provenance unknown | Archive only if every record is confirmed |
| Any persisted field differs | Refuse that batch; do not choose a winner by revision number | Retain unchanged in `replaying/` |
| Two differing values for one key inside a batch | Refuse before database reads or writes | Retain unchanged |
| HTTP flush fails or acknowledgement is ambiguous | Do not confirm; next boot reads the existing applied rows first | Retain unchanged |
| WAL suspended, not applied in time, missing fields, unreadable SQL response, or failed post-write readback | Refuse confirmation | Retain unchanged |
| Recovery attempted after a live drain or after its first boot attempt | Refuse before inspecting/staging files | Leave original pathname unchanged |
| Invalid framing or retired timeframe mixed into a file | Existing full-file preflight refuses all siblings | Retain unchanged |

The key includes table/timeframe, timestamp, security ID, segment and feed.
Full-row matching covers all 25 current persisted fields, including the
signed-volume basis, nullable legacy metadata, quality flags and revision.
It compares the writer's persisted projection, including its price rounding;
it never invents missing legacy metadata or relabels old tick-rule values.
The explicit legacy duplicate exception requires all original 18 fields to
match, all seven added database fields to remain SQL NULL, and the incoming
row to have no metadata/revision, an explicit legacy basis, and exactly the
decoder's unknown-metadata flags (plus unknown sign when applicable). Any
known metadata, extra quality flag, revision, current basis, or changed old
payload remains a conflict. Recognition issues no write: NULL provenance
stays NULL and Top Volume does not treat the row as current.

Each recovery batch contains at most 64 source records. Fixed input buffers,
query/response byte caps, and bounded WAL attempts limit memory and retries.
Each complete WAL-and-query phase has a ten-second deadline; the existing
ILP transport retains its separate request/reconnect bound. File discovery
and total replay work still grow with file count and bytes. No database read
is added to the tick or live candle-update path.

The application awaits recovery before releasing its managed candle
producers. `SealWriterRunner` consumes the recovery permission once, and a
live drain permanently closes it. This is a **single managed writer**
contract. Independent concurrent writers to the same candle tables remain
unsupported: a SQL read followed by ILP is not an atomic conditional insert.
Deploy/runbook verification must establish that exclusion before relying on
this recovery protocol.

QuestDB acknowledges WAL transactions before every table reader necessarily
sees them; `writerTxn` and `sequencerTxn` identify applied and sequenced
progress. Recovery requires a nonsuspended, caught-up table before checking
absence and again before post-write verification. See the official
[WAL documentation](https://questdb.com/docs/concepts/write-ahead-log/) and
[WAL metadata reference](https://questdb.com/docs/query/functions/meta/#wal_tables).
Readback proves equality at that observation under the stated writer
exclusion. It does not establish hardware power-loss durability; archived
original files remain retained as before.

The counters distinguish inserted-or-identical confirmations, identical rows
that required no write, conflicting files, and an unverified recovery scope.
A conflicting original can contain earlier batches already confirmed. Its
next retry skips those identical rows and again retains the conflicting
remainder; it does not erase the original or roll back the stored row.
Pending conflicts do not permanently block startup: the application waits
for the recovery pass to finish, receives its pending counts, and then starts
live producers. File scanning and bounded query/write attempts can delay that
completion. Unresolved historical gaps remain visible instead of being
silently overwritten or advertised as recovered.

Validation selectors:

```text
cargo test -p tickvault-storage seal_recovery_guard
cargo test -p tickvault-storage seal_writer_task
cargo test -p tickvault-storage recovery_scope_cannot_be_reopened
cargo test -p tickvault-storage --test seal_drain_recovery
```

The explicitly ignored release fixture is
`seal_recovery_guard::tests::isolated_questdb_recovery_inserts_skips_and_refuses_conflicts`.
It requires `TICKVAULT_ISOLATED_QUESTDB_EXEC_URL` on a throwaway loopback
QuestDB and refuses any preexisting `candles_5s` table. It creates that fresh
table, verifies real SQL/ILP insertion, exact retry, nontrivial prices and
percentages, conflicting volume, NULL historical basis, and unchanged staged
conflict files, then removes only its fixture table. Suspended/unapplied WAL
failures and partial acknowledgement prefixes are injected in unit tests;
the database fixture does not claim to suspend a real WAL worker.

These tests must actually pass on the candidate and the pinned QuestDB
version before release. Authored or ignored tests are not validation results.
