//! Bounded WAL unapplied RANGES (2026-10-20, plan item PR W) — the second
//! half of the applied-watermark's "what has NOT reached the database" record.
//!
//! # The defect this closes (MEASURED 2026-10-20, live box)
//!
//! The applied-watermark (`wal_applied_watermark`) records unapplied frames in
//! a fixed table of 128 buckets of ~4.6 minutes each. Two shapes overflowed it,
//! and an overflow was persisted as ONE BIT with no height:
//!
//! * the catch-up backlog guard (`guard_pending_backlog`) marked the WHOLE
//!   leftover range bucket by bucket; a backlog spanning days (3,176 buckets on
//!   20 Oct) can never fit in 128 slots;
//! * a shed on the next day landed on a slot still holding yesterday's id
//!   (the table is indexed modulo 128 and nothing ever cleared old ids).
//!
//! A reloaded overflow bit was seeded at `u64::MAX`, which no drain can pass,
//! so from that boot on EVERY segment read as unapplied: the replay could skip
//! nothing and the prune could delete nothing, including segments written and
//! fully acknowledged hours later. The WAL grew without bound.
//!
//! # What this module is
//!
//! * [`UnappliedRanges`]: at most [`UNAPPLIED_RANGE_SLOTS`] (32) disjoint
//!   inclusive `[lo, hi]` sequence ranges. Adding a range merges overlapping
//!   and adjacent ones; when a 33rd would be needed two NEIGHBOURS are
//!   merged instead, so coverage only grows and nothing is ever dropped.
//!   **⚠ CHANGED 2026-10-20 (PR W review round 3, M3):** the merge took the
//!   two CLOSEST ranges, which on a box whose old backlog never drains
//!   swallowed acked sessions between two boot tails and held them
//!   unprunable for good. It now takes the lowest gap that no waiting
//!   segment's span intersects (merging there covers nothing on disk), and
//!   only when every gap holds a waiting segment, or the spans are not
//!   known, the LOWEST pair (the oldest stretch, the likeliest to be
//!   archived or pruned already). Every operation is O(32) (a merge with
//!   spans: O(32 × log spans); `clip_to`: O(32 × log spans + spans)), cold
//!   paths only (boot, the end of a catch-up round, the drained reset, a
//!   QuestDB suspension episode, the once-a-second persist). The per-frame
//!   shed path never touches it.
//! * A sidecar file, [`UNAPPLIED_RANGES_FILE`], beside `applied.tvaw`: magic,
//!   version, a `migrated_overflow` flag, a generation, the ranges, CRC-32.
//!   Written tmp + fsync + rename + directory fsync.
//! * The boot fold, the backlog guard and the trim that use it; the trim
//!   clips the ranges to the spans the waiting segments can hold.
//!
//! # Compatibility and failure direction
//!
//! `applied.tvaw` stays version 1. The new binary trusts the ranges only when
//! the sidecar is valid AND its generation equals the one written into
//! `applied.tvaw`'s two pad bytes (which an older binary ignores and writes
//! as zero).
//!
//! **⚠ CORRECTED 2026-10-20 (PR W review, rollback).** The first draft kept
//! `applied.tvaw`'s overflow bit SET while any range existed. The boot tail
//! exists on nearly every boot, and an older binary seeds a loaded bit at
//! height `u64::MAX`, which it can never clear: rolling back would have
//! re-created the WAL-growth bug even on a clean box. Now every range is also
//! written into the version-1 bucket table as the bucket ids it spans
//! (`UNAPPLIED_BUCKET_RANGE_MARK`) whenever they land on free slots, so an
//! older binary reads exact buckets it can clear like any shed. The bit is set
//! only for what that table cannot represent: an overflow in RAM, an overflow
//! migrated at a boot (the box was already overflowed for an older binary),
//! or a range whose buckets do not fit (128 or more, or a slot holding a
//! different id). The span between the highest frame on disk at the boot and
//! `own_floor` holds no frame of an earlier process and is left out of that
//! encoding, so the overnight boot tail costs a few buckets, not hundreds.
//!
//! This binary, reading the file back: with the sidecar trusted, it drops the
//! range-encoded buckets (the sidecar holds the same ranges exactly); with it
//! missing, corrupt, stale or foreign, it keeps them as ordinary unapplied
//! buckets, which the boot fold folds into their exact bucket spans, and a set
//! bit means what it always meant: unbounded, replay everything. Every
//! failure direction is "replay more".
//!
//! **Honest limits:** this stops NEW growth. A backlog already on disk when
//! the fix ships is migrated into one bounded range and stays protected (and
//! unprunable) until a catch-up drain reads it; this module does not shrink
//! it. A rollback while a MIGRATED range is still held, or while a range does
//! not fit the table, still hands the older binary a set bit (its own
//! behaviour on that box before this change). The frameless span is RAM only,
//! so a tail carried from an EARLIER boot that no trim or drained reset has
//! cleared is encoded whole and can set the bit until one does. It is judged
//! from the boot's high-water probe, which reads record headers: a record
//! behind a corrupt header that only the replay's resync can reach is
//! covered by the sidecar's tail but not by the older binary's buckets (an
//! older binary never had a boot tail at all).
//!
//! **⚠ ADDED 2026-10-20 (PR W review round 3), honest limits found by the
//! third review:**
//!
//! * **A backlog that never drains still costs one kept segment per boot.**
//!   The replay reads the WAL oldest first, so when the oldest backlog never
//!   drains it never reaches the later segments. Each boot's tail
//!   `[boot_range_floor, own_floor]` (since round 6 one slack below
//!   `boot_applied_floor`) keeps the previous session's last
//!   segment (the frames past its last durable ack) for good, one island per
//!   boot. The guard no longer adds a second island (its top is clamped to
//!   `boot_frame_high`), and the merge no longer swallows acked sessions
//!   while a free gap exists (above), so everything else of those sessions
//!   prunes; pinned by `proptest_stuck_backlog_keeps_later_sessions_prunable`
//!   over 40+ boots. Past 32 islands with no free gap between any two (the
//!   sessions in between not yet pruned), the lowest pair merges and the
//!   sessions under it stay until the backlog drains. The real fix for the
//!   island per boot is the backlog draining; nothing here shrinks it.
//!   (Round 4: true only for a session that ends with clean probes; see
//!   the round-4 correction below. **⚠ CORRECTED again in round 5:**
//!   "everything else of those sessions prunes" holds only for a session
//!   with NO shed bucket and NO suspicion episode; see the round-5 section
//!   at the end of this header.)
//! * **A full disk** stops the sidecar write (its tmp file cannot be
//!   created), so the watermark names no sidecar and a range that does not
//!   fit the bucket table writes the unbounded bit; the prune then refuses
//!   everything. Not fixed: see `AppliedWatermark::write_ranges_locked`.
//! * **No or a rejected `applied.tvaw`** makes the boot tail start at the
//!   oldest waiting segment less the slack (at 1 when that is unknown),
//!   which rarely fits the bucket table, so the older-binary bit is set
//!   until a trim or a drained reset narrows it.
//! * **A shed-heavy day** fills bucket slots, so a range id is likely to
//!   land on a slot holding a different id and the older-binary bit is set
//!   on such days (see `AppliedSnapshot::v1_buckets`).
//! * **A crash between the two file writes.** Every range change writes the
//!   sidecar under a new generation, then `applied.tvaw` naming it (every
//!   caller now persists at once, the QuestDB probe included). A crash in
//!   between leaves `applied.tvaw` naming a generation no longer on disk:
//!   the sidecar is not trusted, and when the ranges did not fit the bucket
//!   table the bit loads unbounded and the next boot migrates (replays)
//!   every waiting segment. Replay more, never less; the window is two
//!   fsyncs wide.
//! * **The trim's span cut assumes the reorder bound** (a record at most the
//!   slack below one written before it); see [`waiting_spans`].
//!
//! **⚠ CORRECTED 2026-10-20 (PR W review round 4), what the limits above
//! get wrong or leave out:**
//!
//! * **"This stops NEW growth" and "one kept segment per boot" hold only
//!   for a session that ends with CLEAN QuestDB probes.** While a probe sees
//!   a suspended or lagging table (any latched apply lag counts), acks park
//!   and the watermark stops; the episode's range, and the next boot's tail
//!   from the frozen watermark, cover every ack of that stretch. Keeping
//!   them is correct (none was ever confirmed applied), but on a box whose
//!   backlog never drains nothing reads them, so each such session adds its
//!   whole unconfirmed stretch to the pinned set, not one segment. The real
//!   bound per session is "every segment reaching above its last CONFIRMED
//!   watermark less the slack". The backlog converges only if the drain
//!   outpaces that; how often the live box latches apply lag is Assumed.
//!   The pinned set is now visible on local `/metrics`:
//!   [`WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE`] and
//!   [`WAL_UNAPPLIED_PINNED_BYTES_GAUGE`], set after the boot fold, the backlog guard and
//!   every catch-up trim (a reading as of the last of those, never live). Not fixed
//!   here: confirming parked live acks once enough clean probes follow (as
//!   `settling/` does for replays) instead of pinning them for a full
//!   replay. Pinned by `proptest_stuck_backlog_keeps_later_sessions_prunable`,
//!   which now ends about a quarter of its sessions in suspicion.
//! * **An empty or torn-first-record segment no longer stops the trim.** A
//!   file shorter than any record holds no frame and is skipped; any other
//!   whose first record is unreadable is bounded by its readable neighbours
//!   in write order (checked against its last intact record), so only that
//!   stretch is kept. Before, one such file (the writer opens a segment as
//!   its thread starts, so a process stopped during the catch-up drain
//!   leaves an empty one) refused every trim and gave the fold no spans for
//!   as long as it stayed, and on a stuck box that is for ever. **Not
//!   fixed:** the prune in `ws_frame_spill` still judges the segment BEFORE
//!   an empty one with no upper bound (the empty one's first sequence reads
//!   as 0), so that one segment stays until the empty one is archived; one
//!   segment per empty file, not growth. `ws_frame_spill.rs` has no room
//!   under its size pin for that change.
//! * **An in-session overflow of the bucket table is persisted as a range.**
//!   A catch-up round re-appends earlier days' frames with their original
//!   sequence, so a failed rescue can collide with today's sheds or span a
//!   day boundary. An overflow of known height used to be written as the
//!   bare bit, which loads unbounded: every prune pass of the session
//!   refused every segment. The persist now writes it as
//!   `[lowest unrecorded, highest unrecorded]` (see
//!   `AppliedWatermark::bound_known_overflow_locked`).
//! * **A rollback's cost is larger than "exact buckets it can clear".** The
//!   version-1 table holds every range as bucket ids. With a backlog that
//!   never drains, ranges from many earlier days stay held, and an older
//!   binary (no boot fold) clears ids only on a drained reset, which never
//!   comes on that box. So the first shed of its session that lands on a
//!   slot holding a range id (or an island of an earlier day) overflows its
//!   table, and it reloads that as the unbounded bit: the original defect,
//!   made more likely by ids this binary planted. See
//!   `AppliedSnapshot::v1_buckets`.
//!
//! **⚠ ADDED 2026-10-20 (PR W review round 4):**
//!
//! * **A per-frame shed bucket mark carries no reorder slack.** A frame shed
//!   just below a `2^38` boundary marks only the bucket below it, while it
//!   can sit in a segment whose first record is just above the boundary.
//!   That segment read applied (skipped by replay, prunable). Fixed:
//!   `AppliedSnapshot::segment_is_applied` reads the bucket marks (and the
//!   in-RAM overflow height) from one slack below the first record, the
//!   boot fold gives every folded bucket one slack on its top, and the
//!   deferred-depth prune lookup and `segments_for_bucket` reach one slack
//!   below a segment's first record. The range lookup stays at `[first,
//!   hi]`. Cost: a segment starting within one second above a marked
//!   bucket's end is held. **Rollback limit:** an older binary reads the
//!   bucket marks from the first record, as it always has, so it can still
//!   skip such a segment; the file cannot carry the fix to code that does
//!   not read it (recorded in check O of the property test).
//! * **WAL growth can also continue through the deferred-depth table.**
//!   `wal_deferred_depth` has the same sticky-overflow shape this module
//!   removed from the unapplied table: while that table is overflowed,
//!   every segment reads deferred and both prunes keep it, whatever its
//!   height. A collision there needs a mark about 13 days old still in the
//!   table (see that module's header). Not changed here: plan item W2.
//!   Watch `tv_wal_deferred_depth_kept_segments` beside the pinned gauges.
//! * **Time spent suspect is now visible.** `tv_wal_suspect_episodes_total`
//!   and `tv_wal_suspect_seconds_total` (local `/metrics` only, registered
//!   at 0 at boot) count the QuestDB suspicion episodes the watermark
//!   entered and the whole seconds it spent in them. While suspect (any
//!   latched apply lag counts) every ack parks and that whole stretch stays
//!   pinned until a replay reaches it: on a box whose backlog never drains,
//!   about one session of WAL per lag-latched day. Not redesigned here; the
//!   follow-up is to confirm parked live acks once a full
//!   `WATERMARK_DURABILITY_LAG_SECS` (60 s) and two clean probes follow
//!   them, as `settling/` does for replays. "Growth stopped" is claimed only after
//!   pinned bytes, read together with suspect seconds, stay flat over
//!   several sessions.
//!
//! **⚠ CORRECTED 2026-10-20 (PR W review round 5), what the limits above
//! still get wrong:**
//!
//! * **"One kept segment per boot ... everything else of those sessions
//!   prunes" and "ends with clean probes" are both too narrow.** The bound
//!   per session on a box whose backlog never drains is: the tail (the
//!   segments past its last durable ack), PLUS every segment that overlaps
//!   one of its per-frame unapplied bucket marks (a ring shed, an append
//!   failure or a rescue failure: `note_unapplied` marks the frame's whole
//!   ~4.6-minute bucket, and the boot fold turns it into that bucket's range
//!   with one reorder slack on each side; at a 09:15 RingFull burst that is
//!   about one to three 128 MiB segments per marked bucket), PLUS, for a
//!   session with ANY suspicion episode, the episode's whole stretch (from
//!   its last clean watermark less the slack to its highest parked ack plus
//!   the slack), whether the episode closed with a clean probe mid-session
//!   or was still open at the stop. A session prunes down to the tail alone
//!   only when it has no shed bucket and no suspicion episode. Each shed
//!   island also takes a range slot, so the 32 slots fill sooner and, once
//!   no free gap is left, the lowest-pair merge pins the acked segments
//!   between two islands. Pinned by
//!   `proptest_stuck_backlog_keeps_later_sessions_prunable`, which now sheds
//!   in about a third of its sessions and closes half its episodes. To read
//!   a rise in pinned bytes on a day with no suspect seconds, read the ring
//!   shed and append-failure counters beside it (the post-deploy list in
//!   plan item W names them).
//! * **The tail starts at the DEPTH watermark.** The boot floor is
//!   `min(hwm_ticks, hwm_depth)` less the slack. Depth rows received after
//!   15:40 IST are refused (outside the depth arrival window) and never
//!   acked, so `hwm_depth` stops at the last in-window depth ack while
//!   main-feed frames keep arriving until the stop. On a box whose backlog
//!   never drains, the next boot's tail therefore keeps every segment from
//!   the last depth ack before 15:40 to the stop, not one segment. How many
//!   that is per day depends on the post-close frame volume (Assumed small;
//!   to be measured after deploy as `tv_wal_unapplied_pinned_segments` on a
//!   stuck box).
//! * **The upper side of a segment (the mirror of the round-4 shape).** A
//!   segment's last record can sit up to one slack ABOVE the next segment's
//!   first record (written just before the rotation, minted after a frame
//!   written later on another socket). A per-frame bucket mark on such a
//!   record could sit in a bucket above the segment's range, so the segment
//!   read applied. Fixed: `AppliedSnapshot::segment_is_applied` reads the
//!   bucket marks up to one slack above the range end, the boot fold and the
//!   in-session overflow bound give their ranges one slack on the bottom,
//!   and the deferred-depth prune lookup (`segment_is_deferred`) and
//!   `segments_for_bucket` reach one slack above a range end. Cost: a
//!   segment ending within one second below a marked bucket's start is held.
//!   The older-binary rollback limit recorded above covers this side too.
//! * **The pinned gauges are absent until a trim runs.** They are set only
//!   by a full or partial trim, and deliberately not registered at 0 (0
//!   would read "nothing pinned" before any trim). An absent gauge means the
//!   trim has not run or was refused: read
//!   `tv_wal_unapplied_trim_refused_total{reason}`, which is registered at 0
//!   for every reason at boot.
//!
//! **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2):**
//!
//! * **The migrated backlog drains only above the replay disk floor.**
//!   "Stays protected until a catch-up drain reads it" (above) assumed a
//!   drain runs. Every replay pass is refused while the WAL volume has less
//!   free than `ws_frame_spill::wal_replay_disk_floor_bytes` (40 GiB on the
//!   600 GB box), and the drain itself uses QuestDB disk (a session's
//!   backlog measured at 25–75 GB). Below the floor no replayed batch is
//!   acknowledged, so no trim runs and the migrated range never shrinks:
//!   the backlog and every later session's leftovers (tails, shed buckets,
//!   suspicion stretches) stay pinned until the operator frees space.
//!   Counted, local `/metrics` only: `tv_wal_replay_disk_floor_stops_total`
//!   (one per refused pass, registered at 0 on the first pass of every
//!   boot) and the catch-up summary line's `stop_reason` (`disk`,
//!   `frame_cap`, `ack_timeout`, `apply_lag`). The floor is unchanged.
//! * **The pinned gauges are published on every boot** (corrects the
//!   round-5 bullet just above). The boot fold and the backlog guard now
//!   publish them as well as the trim, whether or not a replayed batch is
//!   acknowledged, and a scan that finds nothing waiting publishes 0. An
//!   ABSENT gauge now means only that no scan could read the waiting set (a
//!   folder that cannot be listed, or a segment with an unreadable first
//!   record outside `settling/unreadable/` that its neighbours cannot
//!   bound); `tv_wal_unapplied_trim_refused_total{reason}` names which at
//!   the next trim. With an unbounded segment in `settling/unreadable/` the
//!   gauges count only the other segments. **Honest limit:** they are set at
//!   boot (fold, catch-up trims, guard) and not during the live session, and
//!   after a drained reset they keep the last value published before it
//!   until the next boot (that reset runs in the app crate, which has no
//!   room under its size pin for one more call). They measure with the
//!   trim's spans, the prune with `[first, next first − 1]`: allow one
//!   segment per session of difference when reading "flat across
//!   sessions". **⚠ CORRECTED 2026-10-21 (PR W review round 6):** this
//!   bullet said both staleness cases "err toward reading more pinned, not
//!   less", and that an absent gauge is the only doubt. Wrong on both. The
//!   gauge is the pinned set as of the LAST BOOT-TIME SCAN. Within a session
//!   it can read LOW (a suspicion episode or a shed burst adds ranges or
//!   bucket marks that pin segments, and `pinned_by_ranges` counts only the
//!   ranges, never this session's per-frame bucket marks) or HIGH (prunes,
//!   a drained reset). A PRESENT gauge can also be stale: the fold
//!   publishes, then a later trim refuses and publishes nothing. Read it
//!   after a boot, together with `tv_wal_unapplied_trim_refused_total`.
//! * **Suspension ranges start TWO slacks below their watermark**
//!   (`wal_applied_watermark::suspension_range_lo`). A frame acked after the
//!   last clean probe can sit one slack below that watermark and be the
//!   LAST record of a segment whose successor's first record sits up to one
//!   more slack below it; the segment, judged by `[first, successor first −
//!   1]`, ended below a range starting one slack under the watermark, so the
//!   replay skipped it and the prune could delete it. Ranges derived from
//!   an ACK watermark need both slacks on the bottom; the range lookup in
//!   `segment_is_applied` is not widened. Cost: up to one more second of
//!   frames per episode kept, so at most one more segment. **⚠ CORRECTED
//!   2026-10-21 (round 6):** the rule was applied only to the suspension
//!   ranges; the plain whole-segment skip and the boot tail are ack
//!   derived too and had the same hole (see the round-6 section below).
//! * **The trim's spans are computed in WRITE order** (segment names), not
//!   first-sequence order ([`waiting_spans`]). A tiny segment A followed by
//!   a long segment X whose first record sits just below A's (within the
//!   slack) sorted X first: X's span ended at A's first record, A's was cut
//!   to its last record plus the slack, and X's middle lay in no span, so
//!   the clip dropped a range over a frame shed there and the next replay
//!   skipped that frame as applied, uncounted. Each span now takes its top
//!   from the segment written after it, and starts at the highest first
//!   sequence written so far less the slack. If a name does not parse, or
//!   the firsts fall by more than the slack in write order, the order is not
//!   known: the spans are built in first-sequence order with no gap cut (one
//!   span from the lowest first up, which keeps more).
//!
//! **⚠ CHANGED 2026-10-21 (PR W review round 6):**
//!
//! * **Every whole-segment judgment against an ACK watermark has two
//!   slacks, not only the suspension ranges.** Acks follow write order
//!   within a sink, so a frame `f` just above `skip_below` (the watermark
//!   `H` less one slack) can be unacked while `H` is acked and durable: its
//!   flush stuck at a QuestDB stall when the process is killed, or acked but
//!   not durable at a power cut. `f` can be the LAST record of a segment
//!   whose successor's first record `g` sits up to one slack below it, so
//!   that segment, judged by `[first, g − 1]`, ended under `skip_below` and
//!   under the boot tail (which started at `skip_below + 1`): the replay
//!   archived it unread and the prune could delete it with `f` never
//!   confirmed. The `skip_below` half predates PR W; the tail repeated it.
//!   Now `AppliedSnapshot::segment_is_applied` requires the segment's end to
//!   sit more than one slack below `skip_below` for both sinks, and the boot
//!   tail and the backlog guard start at `AppliedWatermark::boot_range_floor`
//!   (`boot_applied_floor` less one more slack, at a frame base). The
//!   per-frame check keeps one slack (for one frame it is enough). Cost: at
//!   most one more segment per session read or kept. Pinned by
//!   `test_regression_plain_watermark_straddle_keeps_a_segment_whose_last_record_was_never_acked`
//!   and by the lifetime property test's plain-watermark straddle (`g` and
//!   `H` acked and persisted, `f` never, then an in-session prune and a
//!   crash or a clean stop): `test_regression_round6_plain_watermark_straddle_seeds`.
//!   **Rollback limit:** an older binary judges whole segments with one
//!   slack, so it can still skip such a segment (check O exempts exactly
//!   that shape: a frame above its segment's range end and above that
//!   binary's per-frame floor).
//! * **Suspension-episode marks no longer race the ack path.** The ack
//!   functions load the suspect flag and then update the watermark or the
//!   parked maximum, two separate atomics; an ack that read the flag on one
//!   side of a transition and wrote on the other was in neither episode
//!   range (at the entry it raised the watermark after the range read it,
//!   at the exit it parked into a slot already drained). Both now run under
//!   a small mutex (`AppliedWatermark::ack_gate`) that the two probe
//!   transitions also take around the flag flip and their reads. Per ack
//!   BATCH, never per tick; no allocation; never held across a file write.
//! * **The write-order check compares with the running maximum.** It
//!   compared each first with its neighbour's only, while the spans start at
//!   the running maximum of the firsts so far less the slack, so falls that
//!   add up (0.9 slack three times) passed and left a segment's own first
//!   record in no span; a range over it was clipped away and the replay
//!   skipped that record, uncounted. `order_by_name` and `bound_unreadable`
//!   now both check the running maximum.
//! * **The boot fold's log line is written after logging starts.** The fold
//!   runs inside `WsFrameSpill::new_with_guard`, before the tracing
//!   subscriber is installed, so its summary and the `WS-SPILL-01` warning
//!   of a failed fold persist were discarded (and the persist's failure
//!   latch then silenced the rest of that episode). The fold records a
//!   report; [`log_boot_fold_summary`] writes it once, from `main.rs`, right
//!   after the subscriber's `init`, with one coded warning when either the
//!   watermark file or the ranges sidecar was failing.
//! * **The migrated backlog shrinks only on a boot whose catch-up batches
//!   are PROMOTED to `archive/`.** Any suspicion episode between the drain's
//!   first acked batch and that promotion holds the WHOLE process's settled
//!   output for the next boot to read again (`wal_replay_settle`: one mark
//!   covers every settled segment, and once held, every later batch of the
//!   process is held too). That includes the drain's own apply-lag pause
//!   (the drain pauses on reported tables, which are a subset of the
//!   untrusted ones that start an episode) and a failed probe. So a boot
//!   that pauses even once makes ZERO net progress on the backlog: the next
//!   boot recovers those segments, reads them first (oldest first), causes
//!   lag again, and holds again. The trim keeps the migrated range, because
//!   `settling/` counts as waiting, so the prune frees nothing. **Flat
//!   pinned bytes do not mean growth stopped:** read per boot
//!   `tv_wal_replay_confirmed_segments_total` (must be above 0 for the
//!   backlog to shrink), `tv_wal_replay_settle_held_segments_total` and
//!   `tv_wal_replay_settle_recovered_segments_total`, with the catch-up
//!   summary's `stop_reason=apply_lag` and `tv_wal_catchup_lag_pause_total`.
//!   Held above 0 with confirmed at 0 means the backlog is NOT draining. Not
//!   fixed here (plan item W3): a mark per settled batch, and holding only
//!   the batches acked before an episode began, would turn all-or-nothing
//!   per boot into incremental progress.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::wal_applied_watermark::{
    AppliedWatermark, REPLAY_REORDER_SLACK_SEQ, UNAPPLIED_BUCKET_SHIFT, crc32_ieee, write_fresh,
};

/// The sidecar beside `applied.tvaw`. Not a `.wal`, so the segment glob and
/// the prune never see it.
pub const UNAPPLIED_RANGES_FILE: &str = "unapplied_ranges.tvur";
/// Written first, then renamed over the live file.
pub const UNAPPLIED_RANGES_TMP: &str = "unapplied_ranges.tvur.tmp";
/// Maximum disjoint ranges held. A 33rd merges two NEIGHBOURS: across the
/// lowest gap no waiting segment's span intersects, else the lowest pair
/// ([`UnappliedRanges::add_range_avoiding`]). Coverage only grows.
pub const UNAPPLIED_RANGE_SLOTS: usize = 32;

const RANGES_MAGIC: [u8; 4] = *b"TVUR";
/// Version 1: no migrated span. Read, never written since round 3.
const RANGES_VERSION_V1: u8 = 1;
/// Version 2 (2026-10-20, PR W review round 3, M4): the migrated span
/// `(lo, hi)` sits between the 32 pairs and the CRC.
const RANGES_VERSION: u8 = 2;
const FLAG_MIGRATED_OVERFLOW: u8 = 0b01;
/// magic | version | flags | generation | count | dir_tag
const RANGES_HEADER_LEN: usize = 4 + 1 + 1 + 2 + 4 + 8;
const RANGES_BODY_LEN: usize = UNAPPLIED_RANGE_SLOTS * 16;
/// A version-1 sidecar: header + 32 `(lo, hi)` pairs + crc32.
const UNAPPLIED_RANGES_LEN_V1: usize = RANGES_HEADER_LEN + RANGES_BODY_LEN + 4;
/// The whole sidecar as written: header + 32 `(lo, hi)` pairs + the migrated
/// span `(lo, hi)` + crc32.
pub const UNAPPLIED_RANGES_LEN: usize = RANGES_HEADER_LEN + RANGES_BODY_LEN + 16 + 4;

/// Counter: the trim kept every range because it could not tell what the
/// waiting segments hold (2026-10-20, PR W review round 3, L1). Labelled
/// `reason`: `listing_failed`, `unreadable_first_record`, `nothing_waiting`,
/// `unreadable_set_apart` (a partial trim below the lowest readable span
/// still ran). Local `/metrics` only: no CloudWatch filter, no EMF name, no
/// alarm. Each increment also writes one `warn!` naming the reason (cold, at
/// most once per boot refold or catch-up round).
pub const WAL_UNAPPLIED_TRIM_REFUSED_COUNTER: &str = "tv_wal_unapplied_trim_refused_total";

/// The `reason` a span scan gives when nothing is waiting: the pinned set is
/// then empty, and the gauges publish 0 (round 4, pass 2).
const NOTHING_WAITING: &str = "nothing_waiting";

/// At most [`UNAPPLIED_RANGE_SLOTS`] disjoint, non-adjacent, sorted inclusive
/// ranges of `capture_seq` values that may hold captured-but-unapplied frames.
/// `Copy`, ~520 bytes, fixed size: it never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnappliedRanges {
    slots: [(u64, u64); UNAPPLIED_RANGE_SLOTS],
    len: usize,
}

impl Default for UnappliedRanges {
    fn default() -> Self {
        Self::new()
    }
}

impl UnappliedRanges {
    /// No ranges.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [(0, 0); UNAPPLIED_RANGE_SLOTS],
            len: 0,
        }
    }

    /// Ranges held (≤ [`UNAPPLIED_RANGE_SLOTS`]).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` when no range is held.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The ranges, sorted by `lo`, disjoint and non-adjacent.
    #[must_use]
    pub fn as_slice(&self) -> &[(u64, u64)] {
        &self.slots[..self.len]
    }

    /// Adds `[lo, hi]` (inclusive; `lo` is raised to 1, a sequence of 0 is
    /// never skipped anyway). Overlapping and adjacent ranges merge. When the
    /// result would need more than [`UNAPPLIED_RANGE_SLOTS`] ranges, the
    /// LOWEST two neighbours merge, which only GROWS coverage (see
    /// [`Self::add_range_avoiding`] for why the lowest). Returns whether
    /// coverage changed. O(32), no allocation.
    pub fn add_range(&mut self, lo: u64, hi: u64) -> bool {
        self.add_range_avoiding(lo, hi, &[])
    }

    /// [`Self::add_range`] knowing the waiting segments' `spans` (sorted,
    /// disjoint, inclusive; the newest open-ended). When a merge is needed it
    /// joins the two neighbours across the LOWEST gap that no span
    /// intersects: nothing waiting on disk lies in such a gap, so covering it
    /// costs no segment. When every gap holds a waiting span (or `spans` is
    /// empty) it joins the lowest pair, the oldest stretch, which is the
    /// likeliest to have been archived or pruned already.
    ///
    /// **⚠ CHANGED 2026-10-20 (PR W review round 3, M3):** the merge took the
    /// two CLOSEST ranges. On a box whose old backlog never drains, one boot
    /// tail per boot fills the 32 slots, and joining the closest pair
    /// covered an acked session between two tails, which then never pruned
    /// and stayed waiting, so the cover was never undone.
    ///
    /// O(32 × log spans), no allocation.
    pub fn add_range_avoiding(&mut self, lo: u64, hi: u64, spans: &[(u64, u64)]) -> bool {
        let lo = lo.max(1);
        if lo > hi {
            return false;
        }
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), cold path
        if self.as_slice().iter().any(|&(a, b)| a <= lo && hi <= b) {
            return false;
        }
        let mut merged = [(0u64, 0u64); UNAPPLIED_RANGE_SLOTS + 1];
        let mut n = 0usize;
        let (mut new_lo, mut new_hi) = (lo, hi);
        let mut placed = false;
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), cold path
        for &(a, b) in self.as_slice() {
            if b.saturating_add(1) < new_lo {
                merged[n] = (a, b);
                n += 1;
            } else if new_hi.saturating_add(1) < a {
                if !placed {
                    merged[n] = (new_lo, new_hi);
                    n += 1;
                    placed = true;
                }
                merged[n] = (a, b);
                n += 1;
            } else {
                new_lo = new_lo.min(a);
                new_hi = new_hi.max(b);
            }
        }
        if !placed {
            merged[n] = (new_lo, new_hi);
            n += 1;
        }
        if n > UNAPPLIED_RANGE_SLOTS {
            // Full: merge across the lowest gap no waiting span intersects,
            // else the lowest pair. The ranges are sorted and non-adjacent,
            // so every gap holds at least one sequence and never underflows.
            let mut best = 0usize;
            // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), cold path
            for i in 0..n - 1 {
                if !spans_intersect(spans, merged[i].1 + 1, merged[i + 1].0 - 1) {
                    best = i;
                    break;
                }
            }
            merged[best].1 = merged[best + 1].1;
            merged.copy_within(best + 2..n, best + 1);
            n -= 1;
        }
        self.slots = [(0, 0); UNAPPLIED_RANGE_SLOTS];
        self.slots[..n].copy_from_slice(&merged[..n]);
        self.len = n;
        true
    }

    /// `true` when any held range shares a sequence with `[lo, hi]`.
    /// O(32), no allocation.
    #[must_use]
    pub fn intersects(&self, lo: u64, hi: u64) -> bool {
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), boot-time and prune decisions
        lo <= hi && self.as_slice().iter().any(|&(a, b)| a <= hi && lo <= b)
    }

    /// Removes every sequence below `seq`: a range wholly below it goes, a
    /// range straddling it starts at `seq`. Returns whether coverage changed.
    /// O(32), no allocation.
    pub fn trim_below(&mut self, seq: u64) -> bool {
        let mut out = [(0u64, 0u64); UNAPPLIED_RANGE_SLOTS];
        let mut n = 0usize;
        let mut changed = false;
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), cold path
        for &(a, b) in self.as_slice() {
            if b < seq {
                changed = true;
                continue;
            }
            if a < seq {
                out[n] = (seq, b);
                changed = true;
            } else {
                out[n] = (a, b);
            }
            n += 1;
        }
        self.slots = out;
        self.len = n;
        changed
    }

    /// Removes every sequence in `[lo, hi]` (inclusive): a range wholly
    /// inside goes, a range straddling one end is cut, a range around the
    /// whole span splits in two. Returns `Some(changed)`, or `None` when the
    /// split would need more than [`UNAPPLIED_RANGE_SLOTS`] ranges, in which
    /// case nothing is changed (the caller keeps the span, which only keeps
    /// more). Plan item W4 (2026-10-30): the confirmation of a lag-only
    /// suspicion episode's parked acks. O(32), no allocation.
    pub fn remove_span(&mut self, lo: u64, hi: u64) -> Option<bool> {
        if lo > hi {
            return Some(false);
        }
        let mut out = [(0u64, 0u64); UNAPPLIED_RANGE_SLOTS + 1];
        let mut n = 0usize;
        let mut changed = false;
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), cold path
        for &(a, b) in self.as_slice() {
            if b < lo || hi < a {
                out[n] = (a, b);
                n += 1;
                continue;
            }
            changed = true;
            if a < lo {
                out[n] = (a, lo - 1);
                n += 1;
            }
            if hi < b {
                out[n] = (hi + 1, b);
                n += 1;
            }
        }
        if n > UNAPPLIED_RANGE_SLOTS {
            return None;
        }
        self.slots = [(0, 0); UNAPPLIED_RANGE_SLOTS];
        self.slots[..n].copy_from_slice(&out[..n]);
        self.len = n;
        Some(changed)
    }

    /// `true` when every sequence covered by `other` is covered by `self`.
    /// O(32 × 32). Tests and the integration proptest only; no production
    /// caller (2026-10-20, PR W review round 3: this said "and the boot
    /// report", which never called it). Hidden from the docs (round 4):
    /// public only for `tests/wal_unapplied_ranges_properties.rs`.
    #[doc(hidden)]
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS squared, cold path
        other
            .as_slice()
            .iter()
            .all(|&(lo, hi)| self.as_slice().iter().any(|&(a, b)| a <= lo && hi <= b))
    }

    /// Keeps only the parts of the ranges inside `spans` (sorted, disjoint
    /// inclusive spans). A range over several spans splits into one piece per
    /// span; if the pieces need more than [`UNAPPLIED_RANGE_SLOTS`] slots
    /// they merge as in [`Self::add_range_avoiding`] with the same `spans`
    /// (across a gap no span intersects first), so a piece is never dropped.
    /// Returns whether the ranges changed. O(32 × log spans + spans), two
    /// sorted lists walked together; no allocation.
    pub fn clip_to(&mut self, spans: &[(u64, u64)]) -> bool {
        let mut out = Self::new();
        let mut j = 0usize;
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32) + spans, cold trim
        for &(a, b) in self.as_slice() {
            while j < spans.len() && spans[j].1 < a {
                j += 1;
            }
            let mut k = j;
            while k < spans.len() && spans[k].0 <= b {
                let (lo, hi) = (a.max(spans[k].0), b.min(spans[k].1));
                if lo <= hi {
                    out.add_range_avoiding(lo, hi, spans);
                }
                k += 1;
            }
        }
        let changed = out != *self;
        *self = out;
        changed
    }

    /// Builds from decoded pairs, refusing anything this module would never
    /// write: more than 32, `lo == 0`, `lo > hi`, unsorted, overlapping or
    /// adjacent. `None` means the sidecar is not trusted.
    fn from_pairs(pairs: &[(u64, u64)]) -> Option<Self> {
        if pairs.len() > UNAPPLIED_RANGE_SLOTS {
            return None;
        }
        let mut out = Self::new();
        let mut prev_hi: Option<u64> = None;
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), sidecar decode
        for (i, &(lo, hi)) in pairs.iter().enumerate() {
            if lo == 0 || lo > hi {
                return None;
            }
            if let Some(p) = prev_hi
                && p.saturating_add(1) >= lo
            {
                return None;
            }
            out.slots[i] = (lo, hi);
            prev_hi = Some(hi);
        }
        out.len = pairs.len();
        Some(out)
    }
}

/// `true` when any of the sorted, disjoint inclusive `spans` shares a
/// sequence with `[lo, hi]`. One binary search: O(log spans).
fn spans_intersect(spans: &[(u64, u64)], lo: u64, hi: u64) -> bool {
    let i = spans.partition_point(|&(_, b)| b < lo);
    spans.get(i).is_some_and(|&(a, _)| a <= hi)
}

/// The sidecar's decoded contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnappliedRangesFile {
    pub ranges: UnappliedRanges,
    /// The ranges include an overflow inherited from an earlier process and
    /// bounded at a boot. While set, the version-1 file's overflow bit stays
    /// set for an older binary (the box was already overflowed for it).
    /// **⚠ CORRECTED 2026-10-20 (PR W review round 3, L4):** this said
    /// "informational; the v1 bit stays set while any range exists, migrated
    /// or not", which was the first draft's rule. The bit now follows this
    /// flag (and an unencodable range), and the flag clears once no range
    /// intersects [`Self::migrated_span`].
    pub migrated_overflow: bool,
    /// The range the overflow was migrated into (round 3, M4). A version-1
    /// sidecar carries none and reads `(1, u64::MAX)` when the flag is set.
    pub migrated_span: Option<(u64, u64)>,
    /// Never 0. `applied.tvaw` carries the same value in its pad bytes when
    /// it vouches for these ranges.
    pub generation: u16,
    /// `dir_tag_of` the WAL directory it was written beside.
    pub dir_tag: u64,
}

impl UnappliedRangesFile {
    /// The on-disk form (version 2: the migrated span sits after the 32
    /// pairs). A set flag with no span is written as `(1, u64::MAX)`.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; UNAPPLIED_RANGES_LEN] {
        let mut out = [0u8; UNAPPLIED_RANGES_LEN];
        out[0..4].copy_from_slice(&RANGES_MAGIC);
        out[4] = RANGES_VERSION;
        let migrated = self.migrated_overflow || self.migrated_span.is_some();
        out[5] = if migrated { FLAG_MIGRATED_OVERFLOW } else { 0 };
        out[6..8].copy_from_slice(&self.generation.to_le_bytes());
        let count = u32::try_from(self.ranges.len()).unwrap_or(u32::MAX);
        out[8..12].copy_from_slice(&count.to_le_bytes());
        out[12..20].copy_from_slice(&self.dir_tag.to_le_bytes());
        // O(1) EXEMPT: fixed UNAPPLIED_RANGE_SLOTS iterations, cold persist
        for (i, (lo, hi)) in self.ranges.as_slice().iter().enumerate() {
            let at = RANGES_HEADER_LEN + i * 16;
            out[at..at + 8].copy_from_slice(&lo.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&hi.to_le_bytes());
        }
        let span_at = RANGES_HEADER_LEN + RANGES_BODY_LEN;
        if migrated {
            let (lo, hi) = self.migrated_span.unwrap_or((1, u64::MAX));
            out[span_at..span_at + 8].copy_from_slice(&lo.to_le_bytes());
            out[span_at + 8..span_at + 16].copy_from_slice(&hi.to_le_bytes());
        }
        let crc_at = span_at + 16;
        let crc = crc32_ieee(&out[..crc_at]);
        out[crc_at..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parses the on-disk form, version 2 or version 1 (the shape before
    /// round 3, which has no migrated span: a set flag then reads as the
    /// span `(1, u64::MAX)`, cleared only when no range is left, its old
    /// meaning). `None` on any shape, checksum or content failure; the
    /// caller then honours `applied.tvaw`'s overflow bit as unbounded, which
    /// replays everything.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 5 || bytes[0..4] != RANGES_MAGIC {
            return None;
        }
        let version = bytes[4];
        let span_len = match (version, bytes.len()) {
            (RANGES_VERSION, UNAPPLIED_RANGES_LEN) => 16,
            (RANGES_VERSION_V1, UNAPPLIED_RANGES_LEN_V1) => 0,
            _ => return None,
        };
        let crc_at = RANGES_HEADER_LEN + RANGES_BODY_LEN + span_len;
        let expected = u32::from_le_bytes(bytes[crc_at..].try_into().ok()?);
        if crc32_ieee(&bytes[..crc_at]) != expected {
            return None;
        }
        let generation = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
        if generation == 0 {
            return None;
        }
        let count = usize::try_from(u32::from_le_bytes(bytes[8..12].try_into().ok()?)).ok()?;
        if count > UNAPPLIED_RANGE_SLOTS {
            return None;
        }
        let dir_tag = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
        let mut pairs = [(0u64, 0u64); UNAPPLIED_RANGE_SLOTS];
        // O(1) EXEMPT: fixed UNAPPLIED_RANGE_SLOTS iterations, sidecar decode
        for (i, slot) in pairs.iter_mut().enumerate() {
            let at = RANGES_HEADER_LEN + i * 16;
            let lo = u64::from_le_bytes(bytes[at..at + 8].try_into().ok()?);
            let hi = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().ok()?);
            // Slots past `count` are written as zero; anything else is damage.
            if i >= count && (lo != 0 || hi != 0) {
                return None;
            }
            *slot = (lo, hi);
        }
        let migrated = bytes[5] & FLAG_MIGRATED_OVERFLOW != 0;
        let migrated_span = if span_len == 0 {
            migrated.then_some((1, u64::MAX))
        } else {
            let at = RANGES_HEADER_LEN + RANGES_BODY_LEN;
            let lo = u64::from_le_bytes(bytes[at..at + 8].try_into().ok()?);
            let hi = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().ok()?);
            match (migrated, lo, hi) {
                (false, 0, 0) => None,
                (true, lo, hi) if lo >= 1 && lo <= hi => Some((lo, hi)),
                // A span with no flag, or a flag with no valid span: damage.
                _ => return None,
            }
        };
        Some(Self {
            ranges: UnappliedRanges::from_pairs(&pairs[..count])?,
            migrated_overflow: migrated,
            migrated_span,
            generation,
            dir_tag,
        })
    }

    /// Reads and validates `<wal_dir>/unapplied_ranges.tvur`, refusing a file
    /// written beside another directory. Quiet: an absent or rejected sidecar
    /// is reported by its consequence (the overflow bit is honoured
    /// unbounded and the boot replays more), never as a separate alarm.
    #[must_use]
    pub fn load(wal_dir: &Path) -> Option<Self> {
        // APPROVED: boot-time / prune-pass load, cold path
        let bytes = std::fs::read(wal_dir.join(UNAPPLIED_RANGES_FILE)).ok()?;
        let file = Self::from_bytes(&bytes)?;
        (file.dir_tag == crate::wal_applied_watermark::dir_tag_of(wal_dir)).then_some(file)
    }

    /// Writes the sidecar: tmp (created fresh, never through a planted
    /// symlink) + fsync, rename over the live file, then fsync the directory
    /// so the rename itself survives a host crash. Cold: boot, the guard, a
    /// trim, a drained reset, or a retry from the once-a-second persist.
    pub(crate) fn persist(&self, path: &Path, tmp: &Path, dir: &Path) -> std::io::Result<()> {
        write_fresh(tmp, &self.to_bytes())?;
        std::fs::rename(tmp, path)?;
        // APPROVED: one directory fsync per sidecar write, cold path. A
        // platform that cannot open a directory for sync still has the
        // renamed file; only a host crash in the next moments could lose it,
        // and then the generation no longer matches and the bit reads as
        // unbounded (replay everything).
        if let Ok(d) = std::fs::File::open(dir)
            && let Err(err) = d.sync_all()
        {
            tracing::debug!(dir = ?dir, error = %err, "unapplied-ranges directory sync unsupported");
        }
        Ok(())
    }
}

/// What the WAL directory still holds that a replay could read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitingSegments {
    /// Lowest readable first `frame_seq` among the waiting segments.
    pub lowest: Option<u64>,
    /// A waiting segment whose first record is unreadable (or v1) and that
    /// could not be bounded by its neighbours in write order, or a folder
    /// that could not be listed: its frames could sit anywhere, so nothing
    /// below it may be trimmed. **⚠ CHANGED 2026-10-20 (PR W review round
    /// 4):** an unreadable segment that `bound_unreadable` bounds, or a
    /// file shorter than any record, no longer sets it.
    pub unknown: bool,
    /// 2026-10-20 (PR W review): a folder that exists could not be listed
    /// (any `read_dir` or entry error other than "not found"). `lowest` then
    /// says nothing about what that folder holds; the fold starts its
    /// migrated range at 1, the guard at the boot floor, and the trim
    /// refuses. Implies `unknown`.
    pub listing_failed: bool,
}

/// A gap wider than this between one waiting segment's first sequence and
/// the next one's is checked for where the lower segment really ENDS (its
/// last intact record, plus the reorder slack), so a range over a stretch
/// whose segments were archived or pruned is clipped (2026-10-20, PR W
/// review). One bucket (~4.6 min): a narrower gap is left whole, which only
/// keeps more.
pub const SPAN_SCAN_GAP_SEQ: u64 = 1 << UNAPPLIED_BUCKET_SHIFT;

/// How far back from a segment's end the tail probe looks for its last
/// intact record before trying the largest record the WAL writes.
const TAIL_PROBE_FIRST_WINDOW_BYTES: u64 = 64 * 1024;

/// The smallest record of any WAL version (a version-1 record's minimum,
/// `ws_frame_spill`'s `WAL_MIN_RECORD_V1`): a segment file shorter than this
/// holds no frame at all (2026-10-20, PR W review round 4). Pinned to the
/// writer's constant by `ws_frame_spill`'s `frame_base_tests`, since that
/// file has no room under its size pin to export it.
pub(crate) const WAL_SMALLEST_RECORD_BYTES: u64 = 13;

/// Every waiting segment's first `frame_seq` and path, read once.
struct WaitingScan {
    /// `(first frame_seq, path)` for every waiting segment with a readable
    /// first record.
    firsts: Vec<(u64, PathBuf)>,
    /// Every waiting segment whose first record is unreadable but that may
    /// hold frames, and whether it sits in `settling/unreadable/`
    /// (2026-10-20, PR W review round 4: an unknown segment no longer makes
    /// the whole scan unknown; [`bound_unreadable`] bounds it by its
    /// neighbours in write order).
    unreadable: Vec<(PathBuf, bool)>,
    listing_failed: bool,
}

/// What [`bound_unreadable`] could establish about the segments whose first
/// record is unreadable.
struct UnreadableBounds {
    /// `[lo, hi]` per unreadable segment it could bound, with its path.
    spans: Vec<(u64, u64, PathBuf)>,
    /// Some unreadable segment could not be bounded (or a folder could not
    /// be listed): its frames could sit anywhere.
    unbounded: bool,
    /// An unbounded one sits outside `settling/unreadable/` (or a folder
    /// could not be listed), so even a cut below the lowest span is unsafe.
    unbounded_outside_set_apart: bool,
}

/// Bounds each segment whose first record is unreadable by its neighbours
/// in WRITE order (2026-10-20, PR W review round 4).
///
/// Before, one such segment anywhere in the live directory or `replaying/`
/// made every trim refuse and gave the boot fold no oldest segment and no
/// spans, and nothing removes it: the writer opens a segment as soon as its
/// thread starts, so a process stopped during the catch-up drain leaves an
/// empty one, and a box whose backlog never drains never reads it. The
/// ranges then only accumulated, one island per boot, and from the 33rd
/// the lowest pair merged over acked sessions, which never pruned again.
///
/// A segment's name is `ws-frames-<nanos>.wal`, strictly rising in write
/// order (`ws_frame_spill::next_segment_name_nanos`). Every record of a
/// segment was written after the first record of the readable segment
/// before it (by name) and before the first record of the readable segment
/// after it, so by the reorder bound the slack stands for, its sequences lie
/// in `[previous first − slack, next first + slack]`; the span recorded is
/// `[previous first − slack, next first − 1]` (open-ended for the newest),
/// and the next segment's own span covers the rest. Each bound is CHECKED:
/// the readable segments' firsts must not fall by more than the slack in
/// name order, and the segment's last intact record (a backward search of
/// at most two windows) must lie inside its bound. A segment whose name does
/// not parse, that has no readable segment before it, or that fails either
/// check stays unbounded. A segment in which the whole-file search finds no
/// intact record at all holds no frame any replay can read (the replay's
/// resync accepts the same records) and is skipped.
///
/// O(waiting segments × log) plus one tail probe per unreadable segment,
/// cold.
fn bound_unreadable(scan: &WaitingScan) -> UnreadableBounds {
    let mut out = UnreadableBounds {
        spans: Vec::new(), // APPROVED: cold boot / catch-up scan
        unbounded: scan.listing_failed,
        unbounded_outside_set_apart: scan.listing_failed,
    };
    if scan.unreadable.is_empty() {
        return out;
    }
    let name_of = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(crate::ws_frame_spill::segment_name_nanos)
    };
    // `(name, first)` of every readable segment, in name order.
    let mut named: Vec<(u128, u64)> = Vec::with_capacity(scan.firsts.len()); // APPROVED: cold scan
    let mut names_ok = true;
    // O(1) EXEMPT: begin — bounded by the waiting segments, cold scan
    for (first, path) in &scan.firsts {
        match name_of(path) {
            Some(name) => named.push((name, *first)),
            None => names_ok = false,
        }
    }
    named.sort_unstable_by_key(|&(name, _)| name);
    // Round 6: against the running maximum, not the neighbour (see
    // `firsts_within_slack_of_running_max`).
    names_ok &= firsts_within_slack_of_running_max(named.iter().map(|&(_, first)| first));
    for (path, set_apart) in &scan.unreadable {
        let bound = name_of(path).filter(|_| names_ok).and_then(|name| {
            let at = named.partition_point(|&(n, _)| n < name);
            let prev = at.checked_sub(1).map(|i| named[i].1)?;
            let next = named.get(at).filter(|&&(n, _)| n > name).map(|&(_, f)| f);
            let lo = prev.saturating_sub(REPLAY_REORDER_SLACK_SEQ).max(1);
            let hi = next.map_or(u64::MAX, |n| n.saturating_sub(1).max(lo));
            let top = next.map_or(u64::MAX, |n| n.saturating_add(REPLAY_REORDER_SLACK_SEQ));
            let last = last_intact_frame_seq(path).ok().flatten()?;
            (lo <= last && last <= top).then_some((lo, hi))
        });
        match bound {
            Some((lo, hi)) => out.spans.push((lo, hi, path.clone())),
            None if holds_no_intact_record(path) => {}
            None => {
                out.unbounded = true;
                out.unbounded_outside_set_apart |= !*set_apart;
            }
        }
    }
    // O(1) EXEMPT: end
    out
}

/// `true` when the backward search reads the WHOLE of `seg` and finds no
/// intact record: no replay can read a frame from it (2026-10-20, PR W review
/// round 4). A file larger than the search window is never called empty, and
/// neither is one the search could not open or read (round 3: a read error
/// used to count as "no record", so a segment on a failing volume was
/// skipped and its frames lost their range). Cold.
fn holds_no_intact_record(seg: &Path) -> bool {
    let window = crate::ws_frame_spill::WAL_RESYNC_MAX_FRAME_BYTES as u64 + 64;
    std::fs::metadata(seg).is_ok_and(|m| m.len() <= window)
        && matches!(last_intact_frame_seq(seg), Ok(None))
}

/// `*.wal` files directly under `dir`, never the open segment. Unlike the
/// replay's lenient listing (`ws_frame_spill::wal_segments_in`, which reads
/// an unreadable folder as empty), a folder that exists and cannot be listed
/// is an ERROR here: a trim that took "unreadable" for "empty" would drop the
/// protection of every frame in it (2026-10-20, PR W review). A folder that
/// does not exist is empty. O(entries), cold.
fn segments_in_strict(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), // APPROVED: cold scan
        Err(err) => return Err(err),
    };
    let mut out = Vec::new(); // APPROVED: cold boot / catch-up scan
    // O(1) EXEMPT: bounded by the folder's entries, cold scan
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) == Some("wal")
            && !crate::ws_frame_spill::is_open_segment(&path)
        {
            out.push(path);
        }
    }
    Ok(out)
}

/// Scans the live WAL directory and every folder that holds segments a
/// replay may still read (`replaying/`, and when `include_settling`,
/// `settling/` and `settling/unreadable/`). O(waiting segments) header reads,
/// cold.
fn scan_waiting(wal_dir: &Path, include_settling: bool) -> WaitingScan {
    let settling = wal_dir.join(crate::wal_replay_settle::SETTLING_SUBDIR);
    // Listed in the order a segment moves (live -> replaying/ -> settling/ ->
    // settling/unreadable/), so one moved between two listings is seen in
    // the later folder rather than missed. The trims run on the catch-up
    // task, which makes those moves itself, so no move races them there.
    let unreadable = settling.join(crate::wal_replay_settle::SETTLING_UNREADABLE_SUBDIR);
    // `(folder, listed, is settling/unreadable/)`.
    let dirs: [(PathBuf, bool, bool); 4] = [
        (wal_dir.to_path_buf(), true, false),
        (
            wal_dir.join(crate::ws_frame_spill::REPLAYING_SUBDIR),
            true,
            false,
        ),
        (settling, include_settling, false),
        (unreadable, include_settling, true),
    ];
    let mut out = WaitingScan {
        firsts: Vec::new(),     // APPROVED: cold boot / catch-up scan
        unreadable: Vec::new(), // APPROVED: cold boot / catch-up scan
        listing_failed: false,
    };
    // O(1) EXEMPT: begin — cold boot / catch-up scan, one header read per waiting segment
    for (dir, wanted, set_apart) in &dirs {
        if !*wanted {
            continue;
        }
        let Ok(segments) = segments_in_strict(dir) else {
            out.listing_failed = true;
            continue;
        };
        for seg in segments {
            let seq = crate::ws_frame_spill::first_frame_seq_in_segment(&seg);
            if seq != 0 {
                out.firsts.push((seq, seg));
            } else if std::fs::metadata(&seg).is_ok_and(|m| m.len() < WAL_SMALLEST_RECORD_BYTES) {
                // Round 4: shorter than any record (an empty segment a
                // process left when it stopped before writing): no frame.
            } else {
                out.unreadable.push((seg, *set_apart));
            }
        }
    }
    // O(1) EXEMPT: end
    out
}

/// Scans the live WAL directory and every folder that holds segments a
/// replay may still read (`replaying/`, and when `include_settling`,
/// `settling/` and `settling/unreadable/`). O(waiting segments) header reads,
/// cold. A folder that exists and cannot be listed sets `listing_failed`
/// and `unknown` (2026-10-20, PR W review: it read as empty before).
#[must_use]
pub fn waiting_segments(wal_dir: &Path, include_settling: bool) -> WaitingSegments {
    let scan = scan_waiting(wal_dir, include_settling);
    let mut lowest: Option<u64> = None;
    // O(1) EXEMPT: bounded by the waiting segments, cold scan
    for &(seq, _) in &scan.firsts {
        lowest = Some(lowest.map_or(seq, |l| l.min(seq)));
    }
    // Round 4: a segment bounded by its neighbours starts no lower than
    // the readable one before it less the slack, so it never lowers
    // `lowest` below what the callers already take the slack off; only an
    // unbounded one makes the oldest unknown.
    WaitingSegments {
        lowest,
        unknown: bound_unreadable(&scan).unbounded,
        listing_failed: scan.listing_failed,
    }
}

/// The sequence of the LAST intact record in `seg`, found by searching
/// backwards from the end for a record header whose CRC matches (so a torn
/// tail, or a corrupt record the replay's resync skips, is stepped over).
/// Looks at the last 64 KiB first, then at the largest record the writer
/// produces. `Ok(None)` when the windows were read and hold no intact
/// record; `Err` when the file could not be opened, sized, sought or read.
/// 2026-10-20 (PR W review round 3): an I/O error used to read as `None`,
/// which [`holds_no_intact_record`] took for "no frame in it", so one failed
/// read on a degraded volume skipped a whole segment and dropped its
/// protection. Every caller now treats `Err` as unknown (keep more). Cold,
/// allocates the window once.
fn last_intact_frame_seq(seg: &Path) -> std::io::Result<Option<u64>> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(seg)?; // APPROVED: cold trim probe
    let len = file.metadata()?.len();
    let max_window = crate::ws_frame_spill::WAL_RESYNC_MAX_FRAME_BYTES as u64 + 64;
    // O(1) EXEMPT: at most two windows per segment, cold trim probe
    for window in [TAIL_PROBE_FIRST_WINDOW_BYTES, max_window] {
        let window = window.min(len);
        let base = len - window;
        file.seek(std::io::SeekFrom::Start(base))?;
        let size = usize::try_from(window)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        let mut buf = vec![0u8; size]; // APPROVED: cold trim probe
        file.read_exact(&mut buf)?;
        if buf.len() < 4 {
            return Ok(None);
        }
        let mut pos = buf.len() - 4;
        // O(1) EXEMPT: bounded by the window, cold trim probe
        loop {
            // `TVW2`..`TVW4` carry a sequence; `record_at` checks the magic,
            // the length and the CRC.
            if buf[pos..pos + 3] == *b"TVW" && (b'2'..=b'4').contains(&buf[pos + 3]) {
                let at = base + pos as u64;
                // 2026-10-20 (PR W review round 3, L9): a stray magic inside
                // a payload must not allocate past the largest frame the
                // writer produces, nor past the end of the file.
                let left = usize::try_from(len - at).unwrap_or(usize::MAX);
                let cap = crate::ws_frame_spill::WAL_RESYNC_MAX_FRAME_BYTES.min(left);
                if let crate::ws_frame_spill::RecordProbe::Intact {
                    frame_seq,
                    record_len,
                } = crate::ws_frame_spill::record_at_capped(&mut file, at, cap)
                    && at.saturating_add(record_len) <= len
                    && frame_seq > 0
                {
                    return Ok(Some(frame_seq));
                }
            }
            if pos == 0 {
                break;
            }
            pos -= 1;
        }
        if window == len {
            break;
        }
    }
    Ok(None)
}

/// The union of the `[lo, hi]` sequence spans every waiting segment (the
/// WAL directory, `replaying/`, `settling/`, `settling/unreadable/`) can
/// hold, sorted and merged (2026-10-20, PR W review). Segment `i` spans
/// `[first_i − slack, first_(i+1) − 1]`, the newest is open-ended (it covers
/// the writer's open segment and every frame still to come), and when the
/// next first is more than [`SPAN_SCAN_GAP_SEQ`] away the top is cut to the
/// segment's last intact record plus the slack. That cut relies on the
/// reorder bound the slack stands for: no record carries a sequence more
/// than the slack below a record written before it, so the segment's highest
/// sequence is at most its last record's plus the slack (2026-10-20, PR W
/// review round 3: the property test found that a record placed further
/// out of order than the slack would be cut off; the same bound is what
/// `skip_below`, the migrated range and the guard rely on). `None` when a folder
/// could not be listed, nothing is waiting, or a segment with an unreadable
/// first record, outside `settling/unreadable/`, cannot be bounded by its
/// neighbours (round 4: one they bound, or a file shorter than any record,
/// no longer refuses; round 3: nor does one whose whole-file read found no
/// intact record, but an I/O error while reading it does): the caller then
/// trims nothing (see
/// [`trim_to_waiting`] for the one partial trim).
///
/// **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2):** "segment `i`"
/// and "the next first" are in WRITE order (segment names), and a span
/// starts at the highest first sequence written up to it less the slack:
/// `[max(first_0..=first_i) − slack, first_(i+1) − 1]`, raised to
/// `first_i` when the next first sits below it. In first-sequence order a
/// long segment written after a tiny one, with its first record just below
/// the tiny one's, lost its middle from every span. See the module header.
///
/// O(waiting segments) header reads + a sort, plus one tail probe per gap
/// segment (at most two windows, ≤ 4 MiB), cold.
///
/// **⚠ CORRECTED 2026-10-21 (PR W review round 6):** this said it runs
/// "after the boot refold and each catch-up round". It has NO production
/// caller: it is the public test seam (unit tests and the lifetime property
/// test) over the private `scan_spans`, which production reaches through
/// [`fold_at_boot`], [`guard_pending_backlog`] and [`trim_to_waiting`]
/// (those run at the boot fold, after each catch-up round and after the
/// drain). The spans it returns are exactly the ones those clip and merge
/// against.
#[must_use]
pub fn waiting_spans(wal_dir: &Path) -> Option<Vec<(u64, u64)>> {
    match scan_spans(wal_dir) {
        SpanScan::Full(spans) => Some(spans.merged),
        SpanScan::ReadableOnly(_) | SpanScan::Refused(_) => None,
    }
}

/// The waiting segments' spans, merged and per segment.
struct Spans {
    /// Sorted, disjoint, merged (what the trim clips to).
    merged: Vec<(u64, u64)>,
    /// `(lo, hi, path)` per waiting segment, unmerged (2026-10-20, PR W
    /// review round 4: for the pinned-segment gauges).
    segments: Vec<(u64, u64, PathBuf)>,
}

/// What [`scan_spans`] could establish about the waiting segments.
enum SpanScan {
    /// Every waiting segment's span is known.
    Full(Spans),
    /// The spans of the readable (or neighbour-bounded) segments; every
    /// segment that could not be bounded sits in `settling/unreadable/`
    /// (2026-10-20, PR W review round 3, L1). Only a cut BELOW the lowest
    /// span is safe from it.
    ReadableOnly(Spans),
    /// Nothing usable; the `reason` label of
    /// [`WAL_UNAPPLIED_TRIM_REFUSED_COUNTER`].
    Refused(&'static str),
}

/// [`waiting_spans`] with the reason it could not answer. Cold.
///
/// **⚠ CHANGED 2026-10-20 (PR W review round 4):** a segment whose first
/// record is unreadable no longer refuses the whole scan. A file shorter
/// than any record holds no frame and is skipped ([`scan_waiting`]); any
/// other is bounded by its neighbours in write order ([`bound_unreadable`])
/// and its bounded span joins the others. Only one that cannot be bounded
/// still refuses (or, in `settling/unreadable/`, limits the trim to a cut
/// below the lowest span).
fn scan_spans(wal_dir: &Path) -> SpanScan {
    let mut scan = scan_waiting(wal_dir, true);
    if scan.listing_failed {
        return SpanScan::Refused("listing_failed");
    }
    let bounds = bound_unreadable(&scan);
    if bounds.unbounded_outside_set_apart {
        return SpanScan::Refused("unreadable_first_record");
    }
    if scan.firsts.is_empty() {
        return SpanScan::Refused(NOTHING_WAITING);
    }
    // Round 4, pass 2: WRITE order (the name), not first-sequence order.
    // Each segment's top comes from the segment written after it. Sorted by
    // first sequence, a tiny segment A followed by a long one X whose first
    // record sits just BELOW A's (minted earlier on another socket, within
    // the slack) put X first: X's span ended at A's first, A's was cut to
    // its last record plus the slack, and X's middle fell in no span, so the
    // clip dropped a range over a frame shed there. If a name does not parse
    // or the firsts fall by more than the slack in name order, the order is
    // not known: sort by first and cut no gap (one span from the lowest
    // first up, which keeps more).
    let gap_cuts = order_by_name(&mut scan.firsts);
    let n = scan.firsts.len();
    let mut segments: Vec<(u64, u64, PathBuf)> = Vec::with_capacity(n + bounds.spans.len()); // APPROVED: cold trim
    // O(1) EXEMPT: begin — bounded by the waiting segments, cold trim
    // Every record of a segment was written after the first record of every
    // segment before it in write order, so by the reorder bound it is at
    // least the highest of those firsts less the slack (round 4, pass 2:
    // a segment whose first sits just below an earlier one's starts its
    // span there, not one slack below its own first). In first-sequence
    // order (the fallback) the running maximum is the segment's own first.
    let mut written_floor = 0u64;
    for (i, (first, path)) in scan.firsts.iter().enumerate() {
        let first = *first;
        written_floor = written_floor.max(first);
        let lo = written_floor
            .saturating_sub(REPLAY_REORDER_SLACK_SEQ)
            .max(1);
        let hi = match scan.firsts.get(i + 1) {
            None => u64::MAX,
            Some(&(next, _)) => {
                // `next` can sit up to one slack below `first` (the
                // inversion above): then this span is `[first − slack,
                // first]` and `next`'s own span, from `first − slack` (the
                // running floor), covers the rest of this segment (every
                // record of it is at most one slack above `next`, which was
                // written after it, and `next`'s top reaches at least that).
                let coarse = next.saturating_sub(1).max(first);
                if gap_cuts && next.saturating_sub(first) > SPAN_SCAN_GAP_SEQ {
                    last_intact_frame_seq(path)
                        .ok()
                        .flatten()
                        .map_or(coarse, |last| {
                            coarse.min(last.max(first).saturating_add(REPLAY_REORDER_SLACK_SEQ))
                        })
                } else {
                    coarse
                }
            }
        };
        segments.push((lo, hi, path.clone()));
    }
    segments.extend(bounds.spans);
    segments.sort_unstable_by_key(|&(lo, _, _)| lo); // O(1) EXEMPT: cold trim, waiting segments only
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(segments.len()); // APPROVED: cold trim
    for &(lo, hi, _) in &segments {
        match merged.last_mut() {
            Some(prev) if prev.1.saturating_add(1) >= lo => prev.1 = prev.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    // O(1) EXEMPT: end
    let spans = Spans { merged, segments };
    if bounds.unbounded {
        SpanScan::ReadableOnly(spans)
    } else {
        SpanScan::Full(spans)
    }
}

/// Sorts `firsts` into WRITE order (by segment name, which rises strictly in
/// write order: `ws_frame_spill::next_segment_name_nanos`) and returns
/// `true`, when every name parses and no first sequence falls by more than
/// the reorder slack below the highest first written before it in that
/// order (2026-10-20, PR W review round 4, pass 2). Otherwise sorts by first
/// sequence and returns `false`: the caller must then cut no gap. O(n log
/// n), cold.
///
/// **⚠ CHANGED 2026-10-21 (PR W review round 6):** the check compared each
/// first with its NEIGHBOUR's only, while [`scan_spans`] starts each span at
/// the running MAXIMUM of the firsts so far less the slack. Falls of 0.9
/// slack each, three in a row, passed the check and left the third
/// segment's own first record (1.8 slacks below the maximum) in no span, so
/// a range over it was clipped away and the replay skipped that record as
/// applied, uncounted. Both checks now compare against the running maximum
/// ([`firsts_within_slack_of_running_max`]), the bound the span start
/// relies on.
fn order_by_name(firsts: &mut [(u64, PathBuf)]) -> bool {
    let name_of = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(crate::ws_frame_spill::segment_name_nanos)
    };
    // O(1) EXEMPT: begin — bounded by the waiting segments, cold trim
    if firsts.iter().all(|(_, p)| name_of(p).is_some()) {
        firsts.sort_unstable_by_key(|(_, p)| name_of(p));
        if firsts_within_slack_of_running_max(firsts.iter().map(|&(first, _)| first)) {
            return true;
        }
    }
    firsts.sort_unstable_by_key(|&(seq, _)| seq);
    // O(1) EXEMPT: end
    false
}

/// `true` when every first sequence, in the order given (write order), is
/// at least the highest first before it less [`REPLAY_REORDER_SLACK_SEQ`]
/// (2026-10-21, PR W review round 6): exactly the reorder bound
/// [`scan_spans`] relies on when it starts each span at that running
/// maximum less the slack, so each segment's own first record lies in its
/// own span. O(n), cold.
fn firsts_within_slack_of_running_max(firsts: impl Iterator<Item = u64>) -> bool {
    let mut highest = 0u64;
    // O(1) EXEMPT: bounded by the waiting segments, cold trim
    for first in firsts {
        if first.saturating_add(REPLAY_REORDER_SLACK_SEQ) < highest {
            return false;
        }
        highest = highest.max(first);
    }
    true
}

/// Gauge: waiting WAL segments a held unapplied range intersects, after the
/// last trim (2026-10-20, PR W review round 4). **⚠ CHANGED (round 4, pass
/// 2):** also after the boot fold and the backlog guard, so every boot sets
/// it; 0 when nothing is waiting. Local `/metrics` only: no
/// CloudWatch filter, no EMF name, no alarm. A count that keeps rising
/// across sessions on a box whose backlog never drains is the sign the
/// backlog is NOT converging.
pub const WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE: &str = "tv_wal_unapplied_pinned_segments";
/// Gauge: the bytes of those segments. Local `/metrics` only, as above.
pub const WAL_UNAPPLIED_PINNED_BYTES_GAUGE: &str = "tv_wal_unapplied_pinned_bytes";

/// `(segments, bytes)` of the waiting segments whose span intersects a held
/// range: what the ranges keep on disk (2026-10-20, PR W review round 4).
/// An approximation of what the prune refuses (it judges a segment by
/// `[first, next first − 1]`, this by the trim's span), close enough to
/// show whether the pinned set grows. O(segments × 32) plus one `metadata`
/// per pinned segment, cold: after a trim.
fn pinned_by_ranges(segments: &[(u64, u64, PathBuf)], ranges: &UnappliedRanges) -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    // O(1) EXEMPT: bounded by the waiting segments, cold trim
    for (lo, hi, path) in segments {
        if ranges.intersects(*lo, *hi) {
            count += 1;
            bytes = bytes.saturating_add(std::fs::metadata(path).map_or(0, |m| m.len()));
        }
    }
    (count, bytes)
}

/// What the boot fold did, for the boot log ([`log_boot_fold_summary`]
/// writes it after the tracing subscriber is installed, 2026-10-21, PR W
/// review round 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootFold {
    /// An overflow (inherited unbounded, or set before this point) was turned
    /// into the range `[oldest waiting − REPLAY_REORDER_SLACK_SEQ, own_floor]`
    /// (from 1 when the oldest waiting segment is unknown). **⚠ CORRECTED
    /// 2026-10-20 (PR W review round 3, L4):** this said `[oldest waiting,
    /// own_floor]`, without the slack the first review added.
    pub migrated_overflow: bool,
    /// Bucket-table slots from earlier processes folded into exact ranges.
    pub folded_buckets: u32,
    /// The boot tail `[max(boot_applied_floor + 1, oldest waiting − slack),
    /// own_floor]`, when non-empty. **⚠ CHANGED 2026-10-21 (PR W review
    /// round 6):** it starts at `AppliedWatermark::boot_range_floor`
    /// (`boot_applied_floor` less one more slack, at a frame base), not at
    /// `boot_applied_floor + 1`. **⚠ CORRECTED 2026-10-20 (PR W review
    /// round 4):** this is one segment's worth only when the previous
    /// session ended with clean QuestDB probes. A session that stopped
    /// inside a suspicion episode left its watermark frozen at the
    /// episode's start, so the tail covers every ack after it: on a box
    /// whose backlog never drains, all of those segments stay pinned (see
    /// the module header).
    pub tail: Option<(u64, u64)>,
    /// Ranges held after the fold.
    pub ranges: usize,
    /// The frozen floor the tail starts above.
    pub boot_applied_floor: u64,
    /// At or above every frame an earlier process wrote; the production
    /// wrapper rounds it up to a frame base and ratchets the frame counter to
    /// it, so every frame this process mints is above it (round 3, M1).
    pub own_floor: u64,
    /// The seeded frame sequence at the fold (every frame an earlier process
    /// wrote is at or below it): `[boot_frame_high + 1, own_floor]` is the
    /// frameless gap left out of the encoding for an older binary.
    pub boot_frame_high: u64,
}

/// The boot fold, run ONCE per process from `WsFrameSpill::new_with_guard`
/// right after the sequence is seeded past the disk and the watermark is
/// bound, BEFORE the writer thread spawns and before any sink or socket
/// exists. See [`AppliedWatermark::fold_at_boot_with_spans`] for the steps.
///
/// `own_floor = max(current frame seq, wall clock)` rounded UP to a frame
/// base: every frame an earlier process wrote is at or below it (the
/// sequence was just ratcheted past the disk). **⚠ CHANGED 2026-10-20 (PR W
/// review round 3, M1):** "every frame this process mints is above it" was
/// assumed, not enforced: the counter stayed at the seeded value and minted
/// `max(prev + 1, now)`, so a backward clock step after the fold minted
/// frames inside `(boot_frame_high, own_floor)`, which the drained reset
/// and the older-binary encoding both treat as frameless. The counter is now
/// ratcheted to `own_floor` here, before the fold and before anything can
/// mint, so every later mint is above it whatever the clock does.
///
/// A folder that could not be listed, or an unreadable first record, makes
/// the oldest waiting segment unknown, so a migrated overflow starts at 1.
/// The waiting spans, when known, steer a merge of a full range set across
/// a gap with nothing waiting (round 3, M3).
///
/// O(2 × waiting segments) header reads (one scan for the oldest, one for
/// the spans, which also feeds the pinned gauges) plus the fold, cold, once
/// per boot. **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2):** it
/// publishes the pinned gauges after the fold, so every boot sets them even
/// when no replayed batch is ever acknowledged.
///
/// Returns `None` when the watermark is bound to another directory (a test
/// binary sharing the global) or the fold already ran.
// TEST-EXEMPT: covered by test_fold_at_boot_migrates_overflow_and_registers_the_tail + test_migrated_range_covers_replaying_segments + test_fold_wrapper_ratchets_the_frame_counter_past_own_floor
pub fn fold_at_boot(wm: &AppliedWatermark, wal_dir: &Path) -> Option<BootFold> {
    if !wm.is_bound_to(wal_dir) {
        return None;
    }
    let boot_frame_high = crate::ws_frame_spill::current_frame_seq();
    let own_floor = crate::ws_frame_spill::round_up_to_frame_base(
        boot_frame_high.max(crate::wal_applied_watermark::wall_nanos()),
    );
    // M1: from here on every mint is above `own_floor`.
    let _counter = crate::ws_frame_spill::ratchet_frame_seq_to(own_floor);
    let waiting = waiting_segments(wal_dir, true);
    let oldest = if waiting.unknown {
        None
    } else {
        waiting.lowest
    };
    let scan = scan_spans(wal_dir);
    let spans: &[(u64, u64)] = match &scan {
        SpanScan::Full(spans) => &spans.merged,
        SpanScan::ReadableOnly(_) | SpanScan::Refused(_) => &[],
    };
    let fold = wm.fold_at_boot_with_spans(oldest, boot_frame_high, own_floor, spans)?;
    wm.persist_now();
    // Round 4, pass 2: every boot publishes the pinned set, whether or not a
    // replayed batch is ever acknowledged (a box under the replay disk floor
    // acknowledges none, so no trim runs).
    let pinned = publish_pinned_scan(wm, &scan);
    // Round 6: this runs before the tracing subscriber exists, so the line
    // is recorded here and written by `log_boot_fold_summary` after it.
    let (watermark_persist_failing, ranges_persist_failing) = wm.persist_failing_latched();
    let _first = BOOT_FOLD_REPORT.set(BootFoldReport {
        fold,
        pinned,
        listing_failed: waiting.listing_failed,
        watermark_persist_failing,
        ranges_persist_failing,
        wal_dir: wal_dir.to_path_buf(),
    });
    Some(fold)
}

/// What [`fold_at_boot`] recorded for the boot log (2026-10-21, PR W review
/// round 6). The fold runs inside `WsFrameSpill::new_with_guard`, before the
/// tracing subscriber is installed, so a line logged there was discarded:
/// the fold's summary (the only log record of the first-deploy migration)
/// and, on a failing disk, the `WS-SPILL-01` warning of the fold's own
/// persist, whose failure latch then silenced every later failure of that
/// episode.
#[derive(Debug, Clone)]
struct BootFoldReport {
    fold: BootFold,
    pinned: Option<(u64, u64)>,
    listing_failed: bool,
    watermark_persist_failing: bool,
    ranges_persist_failing: bool,
    wal_dir: PathBuf,
}

/// Set once by [`fold_at_boot`]; read by [`log_boot_fold_summary`].
static BOOT_FOLD_REPORT: std::sync::OnceLock<BootFoldReport> = std::sync::OnceLock::new();

/// Set once [`log_boot_fold_summary`] has written the report.
static BOOT_FOLD_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Writes what the boot fold recorded, ONCE, after the tracing subscriber is
/// installed (2026-10-21, PR W review round 6; `main.rs` calls it right after
/// the subscriber's `init`). The summary is an `info!`; when the fold's
/// persist of `applied.tvaw` or of the ranges sidecar failed, one coded
/// `WS-SPILL-01` `warn!` too, since that failure's own warning was emitted
/// with no subscriber and its latch keeps later failures of the episode
/// quiet (the counter `tv_wal_applied_persist_failed_total` still counts
/// each). Same level and code as the persist's own warning: the failure
/// direction is "replay more", nothing is skipped. Returns `false` when no
/// fold ran in this process or the report was already written. O(1), cold,
/// once per process.
pub fn log_boot_fold_summary() -> bool {
    let Some(report) = BOOT_FOLD_REPORT.get() else {
        return false;
    };
    if BOOT_FOLD_LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return false;
    }
    write_boot_fold_report(report);
    true
}

/// The lines [`log_boot_fold_summary`] writes for `report`. Cold.
fn write_boot_fold_report(report: &BootFoldReport) {
    let fold = report.fold;
    info!(
        pinned_segments = report.pinned.map_or(-1, |p| i64::try_from(p.0).unwrap_or(i64::MAX)),
        pinned_bytes = report.pinned.map_or(-1, |p| i64::try_from(p.1).unwrap_or(i64::MAX)),
        wal_dir = ?report.wal_dir,
        migrated_overflow = fold.migrated_overflow,
        folded_buckets = fold.folded_buckets,
        tail_from = fold.tail.map_or(0, |t| t.0),
        tail_to = fold.tail.map_or(0, |t| t.1),
        ranges = fold.ranges,
        boot_applied_floor = fold.boot_applied_floor,
        own_floor = fold.own_floor,
        boot_frame_high = fold.boot_frame_high,
        listing_failed = report.listing_failed,
        "WAL unapplied record folded at boot: earlier processes' marks are now bounded \
         ranges, so the bucket table starts this session empty and nothing written \
         after this boot inherits an unbounded overflow"
    );
    if report.watermark_persist_failing || report.ranges_persist_failing {
        warn!(
            code = tickvault_common::error_code::ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "unapplied_ranges_persist_failed_at_boot",
            wal_dir = ?report.wal_dir,
            watermark_file_failing = report.watermark_persist_failing,
            ranges_sidecar_failing = report.ranges_persist_failing,
            "WAL applied watermark or unapplied ranges could not be persisted at the boot \
             fold (logged now: the fold ran before logging started); until a persist \
             succeeds the next boot reads the ranges as whole buckets or as an overflow \
             and replays more; nothing is skipped"
        );
    }
}

/// Protects the WAL backlog a catch-up drain did NOT reach (2026-10-03,
/// bounded 2026-10-20).
///
/// A drain that stops early (clock, round cap, memory, apply lag, a sink that
/// stops answering) leaves segments as `*.wal` (or staged in `replaying/`)
/// for the next boot. The live lane then acks frames with HIGHER sequences,
/// which lifts the applied watermark past the leftover range; with nothing
/// marking it, the next boot's replay would read every leftover segment as
/// applied and archive it UNREAD.
///
/// This adds ONE range, `[max(lowest waiting first frame_seq −
/// REPLAY_REORDER_SLACK_SEQ, boot_applied_floor + 1), ceiling_seq − 1]`, to
/// the bounded range set and persists the sidecar. **⚠ CHANGED 2026-10-21
/// (PR W review round 6):** the floor is `AppliedWatermark::boot_range_floor`
/// (one more slack below `boot_applied_floor + 1`), as the boot tail. Below the frozen boot
/// floor everything is either applied or already covered by the ranges the
/// boot fold wrote. **⚠ CHANGED 2026-10-20:** it no longer marks the bucket
/// table (`note_unapplied_range`), which a multi-day backlog always
/// overflowed into an unbounded bit that no drain could ever clear.
/// **⚠ CORRECTED 2026-10-20 (PR W review):** the range started at the lowest
/// segment's FIRST sequence; a later record in that segment can sit below it
/// (sixteen sockets race to the writer), so the reorder slack is taken off.
/// A folder that could not be listed starts the range at the boot floor.
/// **⚠ CHANGED 2026-10-20 (PR W review round 3, M3):** the top is clamped
/// to the boot fold's `boot_frame_high` once the fold has run. The
/// leftover backlog is earlier processes' frames, all at or below it; this
/// process's own frames are covered by its own shed buckets and suspension
/// ranges. Up to `ceiling_seq − 1` the guard covered this session's first
/// frames too, a second island per boot that, with a backlog that never
/// drains, was never cleared and kept that session's first segment.
/// Returns the range, or `None` when nothing below the top is waiting.
/// The caller persists the watermark.
///
/// **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2):** publishes the
/// pinned gauges afterwards (when the watermark is bound to `wal_dir`),
/// whether or not the catch-up acknowledged a batch: a drain refused at the
/// replay disk floor runs no trim.
///
/// O(waiting segments) header reads, twice (the guard, then the gauges),
/// plus one O(32) range insert, cold: once per boot, after the drain.
// TEST-EXEMPT: covered by test_regression_leftover_backlog_is_replayed_after_live_acks_pass_it + test_guard_pending_backlog_ignores_segments_at_or_above_the_ceiling
pub fn guard_pending_backlog(
    wm: &AppliedWatermark,
    wal_dir: &Path,
    ceiling_seq: u64,
) -> Option<(u64, u64)> {
    let guarded = (|| {
        // `settling/` holds batches this process already acked; the settle
        // step protects them across a crash itself, so they are not
        // "waiting" here.
        let waiting = waiting_segments(wal_dir, false);
        // Round 6: one slack below `boot_applied_floor + 1`, as the boot
        // tail (`AppliedWatermark::boot_range_floor`).
        let floor_lo = wm.boot_range_floor();
        let lo = if waiting.listing_failed {
            floor_lo
        } else {
            waiting
                .lowest?
                .saturating_sub(REPLAY_REORDER_SLACK_SEQ)
                .max(floor_lo)
        };
        let mut hi = ceiling_seq.checked_sub(1)?;
        if let Some(boot_frame_high) = wm.boot_frame_high() {
            hi = hi.min(boot_frame_high);
        }
        if lo > hi {
            return None;
        }
        wm.add_unapplied_range(lo, hi);
        Some((lo, hi))
    })();
    // Round 4, pass 2: the pinned set after the catch-up, whether or not it
    // acknowledged a batch (a drain refused at the replay disk floor runs
    // no trim). One more O(waiting segments) scan, once per boot.
    if wm.is_bound_to(wal_dir) {
        let _pinned = publish_pinned_scan(wm, &scan_spans(wal_dir));
    }
    guarded
}

/// Clips the ranges to what the waiting segments can hold
/// ([`waiting_spans`]): nothing outside those spans is on disk except
/// archived segments, which are applied by construction, and pruned ones,
/// which were applied and copied. Persists when anything changed. Run after
/// the boot refold and after each catch-up round (from
/// `settle_confirm_replayed`). Refuses while a folder cannot be listed,
/// nothing is waiting, a segment with an unreadable first record outside
/// `settling/unreadable/` cannot be bounded by its neighbours (one they
/// bound, or a file shorter than any record, no longer refuses), or the
/// watermark is bound elsewhere. Returns the lowest span start (the
/// old trim point: the oldest waiting first sequence less the slack).
///
/// **⚠ CHANGED 2026-10-20 (PR W review):** this cut only BELOW the oldest
/// waiting segment. A backlog that never drains then held every later range
/// for ever: a boot tail or guard per boot over segments since archived or
/// pruned, until 32 slots merged them and the merged ranges tainted acked,
/// shed-free sessions in between, which the prune could no longer delete.
/// Now a range over a stretch with no waiting segment goes too.
///
/// **⚠ CHANGED 2026-10-20 (PR W review round 3, L1):** a segment whose
/// first record is unreadable and that sits in `settling/unreadable/` made
/// every trim refuse for as long as it stayed there, and such a segment is
/// never pruned or archived. Now, when every unreadable segment sits there,
/// the ranges are still cut strictly BELOW the lowest readable span (never
/// clipped above it). That is safe for the unreadable segment: it is never
/// read by the replay nor pruned while it stays unreadable, and the boot
/// recovery (`wal_replay_settle`) marks its intact range unapplied anew
/// before it ever returns to the replay, so it never relied on these ranges.
/// Every refusal, and the partial trim, is counted on
/// [`WAL_UNAPPLIED_TRIM_REFUSED_COUNTER`] with its `reason` (local
/// `/metrics` only).
///
/// O(waiting segments) header reads + a sort + one tail probe per gap
/// segment + O(32 × log spans + spans), cold.
// TEST-EXEMPT: covered by test_trim_to_waiting_keeps_ranges_over_waiting_segments + test_trim_to_waiting_refuses_an_unknown_segment + test_trim_to_waiting_cuts_below_a_segment_set_apart_as_unreadable
pub fn trim_to_waiting(wm: &AppliedWatermark, wal_dir: &Path) -> Option<u64> {
    if !wm.is_bound_to(wal_dir) {
        return None;
    }
    let refused = |reason: &'static str| {
        metrics::counter!(WAL_UNAPPLIED_TRIM_REFUSED_COUNTER, "reason" => reason).increment(1);
        // At most once per boot refold or catch-up round, cold. Not a loss:
        // every range the trim could not check is kept, so the WAL keeps
        // more, never less; the line says why it is not shrinking.
        warn!(
            reason,
            wal_dir = %wal_dir.display(),
            "WAL unapplied-range trim refused or partial: the ranges it could not \
             check are kept, so the WAL keeps more until a later trim succeeds"
        );
    };
    let scan = scan_spans(wal_dir);
    let below = match &scan {
        SpanScan::Full(spans) => {
            let below = spans.merged.first().map(|s| s.0);
            if below.is_some() && wm.clip_unapplied_ranges_to(&spans.merged) {
                wm.persist_now();
            }
            below
        }
        SpanScan::ReadableOnly(spans) => {
            refused("unreadable_set_apart");
            let below = spans.merged.first().map(|s| s.0);
            if let Some(below) = below
                && wm.trim_unapplied_ranges_below(below)
            {
                wm.persist_now();
            }
            below
        }
        SpanScan::Refused(reason) => {
            refused(reason);
            None
        }
    };
    // Round 4, pass 2: "nothing waiting" now publishes 0 too, so an absent
    // gauge means only that the waiting set could not be read.
    let _pinned = publish_pinned_scan(wm, &scan);
    below
}

/// Publishes [`WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE`] and
/// [`WAL_UNAPPLIED_PINNED_BYTES_GAUGE`] for the ranges held now, and returns
/// `(segments, bytes)`. Cold.
fn publish_pinned(wm: &AppliedWatermark, segments: &[(u64, u64, PathBuf)]) -> (u64, u64) {
    let (count, bytes) = pinned_by_ranges(segments, &wm.snapshot().ranges);
    metrics::gauge!(WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE).set(count as f64);
    metrics::gauge!(WAL_UNAPPLIED_PINNED_BYTES_GAUGE).set(bytes as f64);
    (count, bytes)
}

/// [`publish_pinned`] from a span scan (2026-10-20, PR W review round 4,
/// pass 2). Run after the boot fold, after a trim and after the backlog
/// guard, so every boot publishes the pinned set whether or not a replayed
/// batch is ever acknowledged. A scan that found nothing waiting publishes
/// `(0, 0)`; one that saw only the readable segments (an unbounded one sits
/// in `settling/unreadable/`) publishes those, which undercounts by the
/// unbounded ones; one that could not read the waiting set (a folder that
/// cannot be listed, or an unbounded unreadable segment outside
/// `settling/unreadable/`) publishes nothing and returns `None`, so the
/// gauges keep their last value or stay absent. Cold.
fn publish_pinned_scan(wm: &AppliedWatermark, scan: &SpanScan) -> Option<(u64, u64)> {
    match scan {
        SpanScan::Full(spans) | SpanScan::ReadableOnly(spans) => {
            Some(publish_pinned(wm, &spans.segments))
        }
        SpanScan::Refused(NOTHING_WAITING) => Some(publish_pinned(wm, &[])),
        SpanScan::Refused(_) => None,
    }
}

/// The waiting segments the held unapplied ranges pin: what a boot's catch-up
/// would still replay (operator Quote 31's evening drain, 2026-10-21). `None`
/// when the waiting set cannot be read. The same span scan and count as the
/// pinned gauges, without publishing them. O(waiting segments) header reads
/// plus O(segments × 32), cold: once a minute after the close.
#[must_use]
pub fn backlog_segments(wal_dir: &Path) -> Option<u64> {
    backlog_segments_with(crate::wal_applied_watermark::applied_watermark(), wal_dir)
}

/// [`backlog_segments`] against `wm`. Cold.
fn backlog_segments_with(wm: &AppliedWatermark, wal_dir: &Path) -> Option<u64> {
    let ranges = wm.snapshot().ranges;
    match scan_spans(wal_dir) {
        SpanScan::Full(spans) | SpanScan::ReadableOnly(spans) => {
            Some(pinned_by_ranges(&spans.segments, &ranges).0)
        }
        SpanScan::Refused(NOTHING_WAITING) => Some(0),
        SpanScan::Refused(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal_applied_watermark::{
        APPLIED_WATERMARK_FILE, AppliedSink, AppliedSnapshot, UNAPPLIED_BUCKET_RANGE_MARK,
        UNAPPLIED_BUCKETS, WATERMARK_DURABILITY_LAG_SECS,
    };
    use crate::ws_frame_spill::{WalEndpoint, WsType};
    use std::time::{Duration, SystemTime};

    const LAG_NANOS: u64 = WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000;

    /// A 2026-era sequence: nanos with the 17 reserved low bits zeroed.
    fn seq(secs: u64) -> u64 {
        ((1_780_000_000 + secs) * 1_000_000_000) >> 17 << 17
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-ranges-{tag}-{}-{}",
            std::process::id(),
            crate::wal_applied_watermark::wall_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// One TVW4 record, byte for byte what the writer produces.
    fn record(frame_seq: u64) -> Vec<u8> {
        let ws = WsType::LiveFeed.as_u8();
        let ep = WalEndpoint::MainFeed.as_u8();
        let frame = [0xABu8; 32];
        let seq_le = frame_seq.to_le_bytes();
        let recv = 7i64.to_le_bytes();
        let len = 32u32.to_le_bytes();
        let crc = crate::ws_frame_spill::crc32_ieee_of(&[
            &[ws],
            &seq_le[..],
            &recv[..],
            &[ep],
            &len[..],
            &frame[..],
        ]);
        let mut v = Vec::new();
        v.extend_from_slice(b"TVW4");
        v.push(ws);
        v.extend_from_slice(&seq_le);
        v.extend_from_slice(&recv);
        v.push(ep);
        v.extend_from_slice(&len);
        v.extend_from_slice(&frame);
        v.extend_from_slice(&crc.to_le_bytes());
        v
    }

    /// A segment of `frames` records one second apart from `first_secs`.
    fn segment(dir: &Path, first_secs: u64, frames: u64) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let mut bytes = Vec::new();
        for i in 0..frames {
            bytes.extend_from_slice(&record(seq(first_secs + i)));
        }
        let path = dir.join(format!("ws-frames-{:020}.wal", seq(first_secs)));
        std::fs::write(&path, bytes).expect("write segment");
        path
    }

    /// A segment body whose FIRST record is torn (its CRC broken), followed
    /// by one intact record for frame `later`: the header probe reads the
    /// segment as unknown, the replay's resync still reads `later`.
    fn torn_then(later: u64) -> Vec<u8> {
        let mut bytes = record(later);
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        bytes.extend_from_slice(&record(later));
        bytes
    }

    /// `applied.tvaw` as an OLDER binary leaves it after an overflow: the
    /// bit set, the pad bytes zero, no sidecar, both sinks acked to `hwm`.
    fn write_v1_overflow_file(dir: &Path, hwm: u64) {
        // Round 4: this binary's persist bounds an overflow of known height
        // into a range, so the older binary's file is written directly.
        crate::wal_applied_watermark::write_v1_overflow_file_for_test(
            dir,
            hwm,
            [(0, 0); UNAPPLIED_BUCKETS],
        );
        let snap = AppliedSnapshot::load(dir).expect("file");
        assert!(snap.overflowed, "the older-binary shape: bit set");
        assert_eq!(snap.overflow_high_seq, u64::MAX, "no height on disk");
        assert!(!dir.join(UNAPPLIED_RANGES_FILE).exists(), "no sidecar");
    }

    fn persist_durably(wm: &AppliedWatermark, t: u64) {
        wm.persist_at(t);
        wm.persist_at(t + LAG_NANOS + 1);
    }

    fn backdate(path: &Path, now: SystemTime, age_secs: u64) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open");
        f.set_times(std::fs::FileTimes::new().set_modified(now - Duration::from_secs(age_secs)))
            .expect("set mtime");
    }

    fn mark_uploaded(dir: &Path, seg: &Path) {
        let len = std::fs::metadata(seg).expect("meta").len();
        crate::raw_frame_upload::write_marker_for_test(
            &crate::raw_frame_upload::markers_dir(dir),
            seg,
            len,
        );
    }

    fn prune_with_copies(
        dir: &Path,
        snap: &AppliedSnapshot,
    ) -> crate::ws_frame_spill::ArchivePruneOutcome {
        let markers = crate::raw_frame_upload::markers_dir(dir);
        crate::ws_frame_spill::prune_active_segments_deferred_at(
            dir,
            172_800,
            u64::MAX,
            SystemTime::now(),
            Some(snap),
            None,
            false,
            crate::ws_frame_spill::UploadGate::Required {
                markers_dir: &markers,
            },
        )
    }

    // ---------------------------------------------------------------------
    // UnappliedRanges
    // ---------------------------------------------------------------------

    #[test]
    fn test_unapplied_ranges_len_is_empty_and_as_slice() {
        let mut r = UnappliedRanges::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.as_slice().is_empty());
        assert!(r.add_range(10, 20));
        assert_eq!(r.len(), 1);
        assert!(!r.is_empty());
        assert_eq!(r.as_slice(), &[(10, 20)]);
        assert_eq!(UnappliedRanges::default(), UnappliedRanges::new());
    }

    #[test]
    fn test_remove_span_cuts_splits_and_refuses_past_the_slots() {
        let mut r = UnappliedRanges::new();
        r.add_range(10, 20);
        r.add_range(30, 40);
        r.add_range(50, 60);
        assert_eq!(r.remove_span(5, 8), Some(false), "a span over no range");
        assert_eq!(r.remove_span(9, 8), Some(false), "an empty span");
        assert_eq!(r.remove_span(15, 35), Some(true));
        assert_eq!(r.as_slice(), &[(10, 14), (36, 40), (50, 60)]);
        assert_eq!(r.remove_span(53, 55), Some(true), "a split in two");
        assert_eq!(r.as_slice(), &[(10, 14), (36, 40), (50, 52), (56, 60)]);
        assert_eq!(r.remove_span(36, 40), Some(true), "a whole range goes");
        assert_eq!(r.as_slice(), &[(10, 14), (50, 52), (56, 60)]);
        assert_eq!(r.remove_span(1, u64::MAX), Some(true));
        assert!(r.is_empty());
        // A full set: a split of one range would need a 33rd slot.
        let mut full = UnappliedRanges::new();
        for i in 0..UNAPPLIED_RANGE_SLOTS as u64 {
            full.add_range(i * 100 + 1, i * 100 + 50);
        }
        assert_eq!(full.len(), UNAPPLIED_RANGE_SLOTS);
        let before = full;
        assert_eq!(full.remove_span(10, 20), None, "refused, nothing changed");
        assert_eq!(full, before);
        assert_eq!(full.remove_span(1, 50), Some(true), "no split needed");
        assert_eq!(full.len(), UNAPPLIED_RANGE_SLOTS - 1);
    }

    #[test]
    fn test_add_range_merges_overlapping_and_adjacent() {
        let mut r = UnappliedRanges::new();
        assert!(r.add_range(10, 20));
        assert!(r.add_range(30, 40));
        assert!(!r.add_range(12, 18), "already covered");
        assert!(
            r.add_range(21, 29),
            "adjacent on both sides joins all three"
        );
        assert_eq!(r.as_slice(), &[(10, 40)]);
        assert!(r.add_range(0, 5), "lo 0 is raised to 1");
        assert_eq!(r.as_slice(), &[(1, 5), (10, 40)]);
        assert!(!r.add_range(9, 8), "an empty range adds nothing");
        assert!(r.add_range(3, 50));
        assert_eq!(r.as_slice(), &[(1, 50)]);
        assert!(r.add_range(u64::MAX - 1, u64::MAX));
        assert_eq!(r.as_slice(), &[(1, 50), (u64::MAX - 1, u64::MAX)]);
    }

    #[test]
    fn test_unapplied_ranges_full_merges_lowest_pair_and_never_shrinks_coverage() {
        let mut r = UnappliedRanges::new();
        // 32 ranges, gaps of 100 except one gap of 3 between #10 and #11.
        let mut lo = 1_000u64;
        for i in 0..UNAPPLIED_RANGE_SLOTS as u64 {
            let gap = if i == 10 { 3 } else { 100 };
            assert!(r.add_range(lo, lo + 5));
            lo += 5 + gap;
        }
        assert_eq!(r.len(), UNAPPLIED_RANGE_SLOTS);
        let before = r;
        // A 33rd disjoint range with no spans known: the LOWEST pair merges
        // (2026-10-20, round 3, M3: it was the closest pair, #10 and #11).
        assert!(r.add_range(1_000_000, 1_000_010));
        assert_eq!(r.len(), UNAPPLIED_RANGE_SLOTS);
        assert!(r.covers(&before), "coverage only grows");
        assert!(r.intersects(1_000_000, 1_000_010));
        let merged = r.as_slice()[0];
        assert_eq!(merged, (before.as_slice()[0].0, before.as_slice()[1].1));
        assert_eq!(
            r.as_slice()[10],
            before.as_slice()[11],
            "the closest pair is left alone"
        );
        // Hammer it: coverage of every range ever added is kept.
        let mut all = Vec::new();
        for k in 0..500u64 {
            let a = (k * 7_919) % 1_000_000 + 1;
            let b = a + (k % 13);
            r.add_range(a, b);
            all.push((a, b));
            assert!(r.len() <= UNAPPLIED_RANGE_SLOTS);
        }
        for (a, b) in all {
            assert!(r.intersects(a, a) && r.intersects(b, b), "{a}..{b} lost");
        }
        // Sorted, disjoint, non-adjacent.
        for w in r.as_slice().windows(2) {
            assert!(w[0].1.saturating_add(1) < w[1].0);
        }
    }

    #[test]
    fn test_remove_range_cuts_splits_and_merges_when_full() {
        let mut r = UnappliedRanges::new();
        r.add_range(10, 20);
        r.add_range(30, 40);
        assert!(!r.remove_range(21, 29), "a gap: nothing to remove");
        assert!(!r.remove_range(5, 1), "an empty range removes nothing");
        assert!(r.remove_range(15, 32));
        assert_eq!(r.as_slice(), &[(10, 14), (33, 40)]);
        assert!(r.remove_range(35, 36), "a split");
        assert_eq!(r.as_slice(), &[(10, 14), (33, 34), (37, 40)]);
        assert!(r.remove_range(1, 100));
        assert!(r.is_empty());
        // Full: a split merges the lowest pair, so coverage only grows
        // outside the hole.
        let mut full = UnappliedRanges::new();
        // O(1) EXEMPT: test loop
        for k in 0..UNAPPLIED_RANGE_SLOTS as u64 {
            full.add_range(k * 100 + 1, k * 100 + 50);
        }
        assert!(full.remove_range(3_120, 3_130));
        assert_eq!(full.len(), UNAPPLIED_RANGE_SLOTS);
        assert_eq!(full.as_slice()[0], (1, 150), "the lowest pair merged");
        assert!(!full.intersects(3_120, 3_130), "the hole stays open");
        assert!(full.intersects(3_101, 3_119) && full.intersects(3_131, 3_150));
    }

    #[test]
    fn test_trim_below_drops_and_cuts() {
        let mut r = UnappliedRanges::new();
        r.add_range(10, 20);
        r.add_range(30, 40);
        r.add_range(50, 60);
        assert!(!r.trim_below(5), "nothing below 5");
        assert!(r.trim_below(35));
        assert_eq!(r.as_slice(), &[(35, 40), (50, 60)]);
        assert!(r.trim_below(41));
        assert_eq!(r.as_slice(), &[(50, 60)]);
        assert!(r.trim_below(61));
        assert!(r.is_empty());
    }

    #[test]
    fn test_intersects_and_covers() {
        let mut r = UnappliedRanges::new();
        r.add_range(10, 20);
        assert!(r.intersects(20, 30));
        assert!(r.intersects(0, 10));
        assert!(!r.intersects(21, 30));
        assert!(!r.intersects(30, 20), "an empty query intersects nothing");
        let mut sub = UnappliedRanges::new();
        sub.add_range(12, 15);
        assert!(r.covers(&sub));
        assert!(!sub.covers(&r));
        assert!(r.covers(&UnappliedRanges::new()));
    }

    // ---------------------------------------------------------------------
    // Sidecar
    // ---------------------------------------------------------------------

    fn sample_file(dir_tag: u64) -> UnappliedRangesFile {
        let mut ranges = UnappliedRanges::new();
        ranges.add_range(seq(1), seq(2));
        ranges.add_range(seq(100), seq(200));
        UnappliedRangesFile {
            ranges,
            migrated_overflow: true,
            migrated_span: Some((seq(1), seq(150))),
            generation: 42,
            dir_tag,
        }
    }

    #[test]
    fn test_sidecar_to_bytes_from_bytes_round_trip() {
        let f = sample_file(9);
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), UNAPPLIED_RANGES_LEN);
        assert_eq!(UnappliedRangesFile::from_bytes(&bytes), Some(f));
    }

    #[test]
    fn test_sidecar_bad_crc_magic_short_or_malformed_is_refused() {
        let bytes = sample_file(9).to_bytes();
        let mut flipped = bytes;
        flipped[RANGES_HEADER_LEN + 3] ^= 1;
        assert_eq!(UnappliedRangesFile::from_bytes(&flipped), None, "crc");
        let mut magic = bytes;
        magic[0] = b'X';
        assert_eq!(UnappliedRangesFile::from_bytes(&magic), None, "magic");
        assert_eq!(UnappliedRangesFile::from_bytes(&bytes[..20]), None, "short");
        let mut version = bytes;
        version[4] = 9;
        assert_eq!(UnappliedRangesFile::from_bytes(&version), None, "version");
        // Malformed content under a VALID crc is refused too.
        let resign = |mut b: [u8; UNAPPLIED_RANGES_LEN]| {
            let end = RANGES_HEADER_LEN + RANGES_BODY_LEN + 16;
            let crc = crc32_ieee(&b[..end]);
            b[end..].copy_from_slice(&crc.to_le_bytes());
            b
        };
        let mut gen0 = bytes;
        gen0[6..8].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(gen0)),
            None,
            "gen 0"
        );
        let mut count = bytes;
        count[8..12].copy_from_slice(&33u32.to_le_bytes());
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(count)),
            None,
            "count"
        );
        let mut unsorted = bytes;
        let first = RANGES_HEADER_LEN;
        let second = RANGES_HEADER_LEN + 16;
        let a: [u8; 16] = unsorted[first..first + 16].try_into().expect("16");
        let b: [u8; 16] = unsorted[second..second + 16].try_into().expect("16");
        unsorted[first..first + 16].copy_from_slice(&b);
        unsorted[second..second + 16].copy_from_slice(&a);
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(unsorted)),
            None,
            "order"
        );
        let mut garbage = bytes;
        garbage[RANGES_HEADER_LEN + 5 * 16] = 1;
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(garbage)),
            None,
            "tail"
        );
        // Round 3 (M4): the migrated span must agree with the flag.
        let span_at = RANGES_HEADER_LEN + RANGES_BODY_LEN;
        let mut no_flag = bytes;
        no_flag[5] = 0;
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(no_flag)),
            None,
            "a span with no flag"
        );
        let mut inverted = bytes;
        inverted[span_at..span_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(inverted)),
            None,
            "lo above hi"
        );
        let mut zero_span = bytes;
        zero_span[span_at..span_at + 16].fill(0);
        assert_eq!(
            UnappliedRangesFile::from_bytes(&resign(zero_span)),
            None,
            "a flag with no span"
        );
    }

    /// The version-1 sidecar (no migrated span) still reads (2026-10-20, PR
    /// W review round 3, M4): a set flag reads as the span `(1, u64::MAX)`,
    /// which clears only once no range is left, its old meaning.
    #[test]
    fn test_sidecar_from_bytes_reads_a_version_1_file() {
        let v2 = sample_file(9).to_bytes();
        let body_end = RANGES_HEADER_LEN + RANGES_BODY_LEN;
        let mut v1 = Vec::with_capacity(UNAPPLIED_RANGES_LEN_V1);
        v1.extend_from_slice(&v2[..body_end]);
        v1[4] = RANGES_VERSION_V1;
        let crc = crc32_ieee(&v1);
        v1.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(v1.len(), UNAPPLIED_RANGES_LEN_V1);
        let got = UnappliedRangesFile::from_bytes(&v1).expect("a v1 sidecar reads");
        assert!(got.migrated_overflow);
        assert_eq!(got.migrated_span, Some((1, u64::MAX)), "unknown span");
        assert_eq!(got.ranges, sample_file(9).ranges);
        // A v1 header on a v2-length file, or the reverse, is refused.
        let mut wrong = v2;
        wrong[4] = RANGES_VERSION_V1;
        assert_eq!(UnappliedRangesFile::from_bytes(&wrong), None);
        v1[4] = RANGES_VERSION;
        assert_eq!(UnappliedRangesFile::from_bytes(&v1), None);
        // No flag: no span.
        let mut plain = UnappliedRangesFile {
            migrated_overflow: false,
            migrated_span: None,
            ..sample_file(9)
        }
        .to_bytes()[..body_end]
            .to_vec();
        plain[4] = RANGES_VERSION_V1;
        let crc = crc32_ieee(&plain);
        plain.extend_from_slice(&crc.to_le_bytes());
        let got = UnappliedRangesFile::from_bytes(&plain).expect("v1");
        assert!(!got.migrated_overflow && got.migrated_span.is_none());
    }

    #[test]
    fn test_sidecar_load_and_persist_refuse_another_directory() {
        let a = scratch("side-a");
        let b = scratch("side-b");
        let f = sample_file(crate::wal_applied_watermark::dir_tag_of(&a));
        f.persist(
            &a.join(UNAPPLIED_RANGES_FILE),
            &a.join(UNAPPLIED_RANGES_TMP),
            &a,
        )
        .expect("persist");
        assert!(!a.join(UNAPPLIED_RANGES_TMP).exists(), "tmp renamed away");
        assert_eq!(UnappliedRangesFile::load(&a), Some(f));
        std::fs::copy(a.join(UNAPPLIED_RANGES_FILE), b.join(UNAPPLIED_RANGES_FILE)).expect("copy");
        assert_eq!(UnappliedRangesFile::load(&b), None, "foreign sidecar");
        assert_eq!(
            UnappliedRangesFile::load(&scratch("side-none")),
            None,
            "absent"
        );
    }

    /// A damaged, missing or stale sidecar falls back to what the version-1
    /// file says on its own: the range's whole buckets when they fit (no
    /// bit), an UNBOUNDED overflow when they do not.
    #[test]
    fn test_corrupt_or_missing_sidecar_falls_back_to_buckets_or_an_unbounded_overflow() {
        // A range that fits the bucket table.
        let dir = scratch("side-fallback");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(5_000));
        wm.note_depth_acked(seq(5_000));
        assert!(wm.add_unapplied_range(seq(10), seq(20)));
        persist_durably(&wm, 1);
        let good = AppliedSnapshot::load(&dir).expect("file");
        assert!(!good.overflowed, "a narrow range never sets the bit");
        assert!(good.range_has_unapplied(seq(15), seq(15)));
        assert!(!good.range_has_unapplied(seq(1_000), seq(1_001)));
        assert!(
            good.buckets.iter().all(|&(id, _)| id == 0),
            "a trusted sidecar replaces the range-encoded buckets"
        );
        let side = dir.join(UNAPPLIED_RANGES_FILE);
        let mut bytes = std::fs::read(&side).expect("read");
        bytes[RANGES_HEADER_LEN] ^= 0xFF;
        std::fs::write(&side, &bytes).expect("write");
        let bad = AppliedSnapshot::load(&dir).expect("file");
        assert!(!bad.overflowed, "no bit was written");
        assert!(
            bad.range_has_unapplied(seq(10), seq(20)),
            "the range's buckets still protect it"
        );
        assert!(
            bad.buckets
                .iter()
                .all(|&(id, mark)| id == 0 || mark != UNAPPLIED_BUCKET_RANGE_MARK),
            "an untrusted load turns them into ordinary buckets"
        );
        std::fs::remove_file(&side).expect("rm");
        let none = AppliedSnapshot::load(&dir).expect("file");
        assert!(!none.overflowed && none.range_has_unapplied(seq(15), seq(15)));

        // A range wider than the bucket table: the bit, explained only by a
        // trusted sidecar.
        let dir = scratch("side-fallback-wide");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(90_000));
        wm.note_depth_acked(seq(90_000));
        let wide_hi = seq(10) + ((UNAPPLIED_BUCKETS as u64 + 4) << UNAPPLIED_BUCKET_SHIFT);
        assert!(wm.add_unapplied_range(seq(10), wide_hi));
        persist_durably(&wm, 1);
        let good = AppliedSnapshot::load(&dir).expect("file");
        assert!(!good.overflowed, "a matched sidecar bounds the bit");
        assert!(!good.range_has_unapplied(wide_hi + 1, wide_hi + 2));
        let bytes = std::fs::read(dir.join(APPLIED_WATERMARK_FILE)).expect("read");
        assert!(
            AppliedSnapshot::from_bytes(&bytes, seq(99_999))
                .expect("v1")
                .overflowed,
            "the bit is on disk"
        );

        // Corrupt: the bit is honoured unbounded.
        let side = dir.join(UNAPPLIED_RANGES_FILE);
        let mut bytes = std::fs::read(&side).expect("read");
        bytes[RANGES_HEADER_LEN] ^= 0xFF;
        std::fs::write(&side, &bytes).expect("write");
        let bad = AppliedSnapshot::load(&dir).expect("file");
        assert!(bad.overflowed);
        assert!(
            bad.range_has_unapplied(wide_hi + 1, wide_hi + 2),
            "replay everything"
        );

        // Missing: same.
        std::fs::remove_file(&side).expect("rm");
        let none = AppliedSnapshot::load(&dir).expect("file");
        assert!(none.overflowed && none.range_has_unapplied(seq(80_000), seq(80_001)));

        // Stale: a valid sidecar whose generation the file does not name.
        let stale = UnappliedRangesFile {
            ranges: good.ranges,
            migrated_overflow: false,
            migrated_span: None,
            generation: good.ranges_gen.wrapping_add(7).max(1),
            dir_tag: crate::wal_applied_watermark::dir_tag_of(&dir),
        };
        stale
            .persist(&side, &dir.join(UNAPPLIED_RANGES_TMP), &dir)
            .expect("persist");
        let mismatched = AppliedSnapshot::load(&dir).expect("file");
        assert!(
            mismatched.overflowed,
            "a stale generation never bounds the bit"
        );
    }

    // ---------------------------------------------------------------------
    // Overflow height
    // ---------------------------------------------------------------------

    #[test]
    fn overflow_taints_only_ranges_starting_at_or_below_its_height() {
        let snap = AppliedSnapshot {
            hwm_ticks: seq(9_000),
            hwm_depth: seq(9_000),
            overflowed: true,
            overflow_high_seq: seq(100),
            ..AppliedSnapshot::default()
        };
        assert!(snap.range_has_unapplied(seq(50), seq(60)));
        assert!(
            snap.range_has_unapplied(seq(100), seq(200)),
            "starts AT the height"
        );
        assert!(
            !snap.range_has_unapplied(seq(101), seq(200)),
            "wholly above"
        );
        assert!(snap.range_is_applied(AppliedSink::Ticks, seq(101), seq(200)));
        // An unknown height (0 or MAX) still taints everything.
        for unknown in [0, u64::MAX] {
            let s = AppliedSnapshot {
                overflow_high_seq: unknown,
                ..snap.clone()
            };
            assert!(s.range_has_unapplied(seq(5_000), seq(5_001)));
        }
        // The RAM path: a collision records its bucket top as the height.
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(999_999));
        wm.note_depth_acked(seq(999_999));
        wm.note_unapplied(seq(10));
        let wrapped = seq(10) + ((UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied(wrapped);
        let ram = wm.snapshot();
        assert!(ram.overflowed);
        assert!(ram.range_has_unapplied(wrapped, wrapped));
        let above = wrapped + (2 << UNAPPLIED_BUCKET_SHIFT);
        assert!(
            !ram.range_has_unapplied(above, above + 10),
            "above the bucket top"
        );
    }

    // ---------------------------------------------------------------------
    // Boot fold
    // ---------------------------------------------------------------------

    #[test]
    fn test_v1_overflow_bit_migrates_to_range_bounded_by_own_floor() {
        let dir = scratch("migrate");
        segment(&dir, 0, 3);
        segment(&dir, 1_000, 3);
        write_v1_overflow_file(&dir, seq(1_500));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        assert!(wm.snapshot().overflowed, "loaded unbounded");
        let oldest = waiting_segments(&dir, true).lowest;
        assert_eq!(oldest, Some(seq(0)));
        let own_floor = seq(3_000);
        let fold = wm
            .fold_at_boot(oldest, seq(1_002), own_floor)
            .expect("first fold");
        assert!(fold.migrated_overflow);
        let snap = wm.snapshot();
        assert!(!snap.overflowed, "the bit is now bounded");
        assert!(snap.migrated_overflow);
        assert_eq!(
            snap.ranges.as_slice()[0].0,
            seq(0) - REPLAY_REORDER_SLACK_SEQ,
            "the migrated range starts a reorder slack below the oldest segment"
        );
        assert!(snap.range_has_unapplied(seq(0), seq(2)));
        assert!(snap.range_has_unapplied(own_floor, own_floor));
        assert!(
            !snap.range_has_unapplied(own_floor + 1, own_floor + 1_000),
            "frames this process mints are above it"
        );
        assert!(
            wm.fold_at_boot(oldest, seq(1_002), own_floor).is_none(),
            "once per process"
        );
    }

    #[test]
    fn test_migrated_range_covers_replaying_segments() {
        let dir = scratch("migrate-replaying");
        let settling = dir.join(crate::wal_replay_settle::SETTLING_SUBDIR);
        let replaying = dir.join(crate::ws_frame_spill::REPLAYING_SUBDIR);
        segment(&settling, 0, 3);
        segment(&replaying, 500, 3);
        segment(&dir, 1_000, 3);
        write_v1_overflow_file(&dir, seq(1_500));
        let waiting = waiting_segments(&dir, true);
        assert_eq!(waiting.lowest, Some(seq(0)), "settling/ is waiting too");
        assert!(!waiting.unknown);
        assert_eq!(
            waiting_segments(&dir, false).lowest,
            Some(seq(500)),
            "replaying/ without settling/"
        );
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.fold_at_boot(waiting.lowest, seq(1_002), seq(3_000))
            .expect("fold");
        let snap = wm.snapshot();
        for first in [0, 500, 1_000] {
            assert!(snap.range_has_unapplied(seq(first), seq(first + 2)));
        }
    }

    #[test]
    fn test_waiting_segments_flags_an_unreadable_first_record() {
        let dir = scratch("waiting-unknown");
        segment(&dir, 100, 2);
        std::fs::write(
            dir.join("ws-frames-00000000000000000001.wal"),
            torn_then(seq(1)),
        )
        .expect("w");
        let w = waiting_segments(&dir, true);
        assert!(w.unknown);
        assert!(!w.listing_failed, "the folder itself was listed");
        assert_eq!(w.lowest, Some(seq(100)));
        assert_eq!(
            waiting_segments(&scratch("waiting-empty"), true),
            WaitingSegments::default(),
            "folders that do not exist are empty, not failed"
        );
    }

    /// 2026-10-20 (PR W review): a folder that exists and cannot be listed
    /// read as EMPTY, so the fold could start a migrated range above frames
    /// it never saw and the trim could cut them. It now fails closed.
    #[test]
    fn test_waiting_segments_fail_closed_when_a_folder_cannot_be_listed() {
        let dir = scratch("listing-failed");
        segment(&dir, 500, 3);
        // `replaying` is a FILE: `read_dir` fails with "not a directory".
        std::fs::write(dir.join(crate::ws_frame_spill::REPLAYING_SUBDIR), b"x").expect("w");
        let w = waiting_segments(&dir, true);
        assert!(w.listing_failed && w.unknown);
        assert_eq!(w.lowest, Some(seq(500)), "the readable folders still count");
        assert_eq!(waiting_spans(&dir), None, "no spans without a full listing");

        // The fold starts a migrated overflow at 1, not at the segment seen.
        write_v1_overflow_file(&dir, seq(400));
        // The fold reads `boot_frame_high` from the process frame counter;
        // mint one so it stands at the wall clock (far above seq(900)) as in
        // production. Run alone in a fresh process (CI's nextest) it was 0,
        // which clamped the guard's top below its floor.
        let _minted = crate::ws_frame_spill::next_frame_seq();
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let fold = fold_at_boot(&wm, &dir).expect("fold");
        assert!(fold.migrated_overflow);
        assert_eq!(wm.snapshot().ranges.as_slice()[0].0, 1, "lo = 1");

        // The trim refuses and keeps every range.
        let before = wm.snapshot().ranges;
        assert_eq!(trim_to_waiting(&wm, &dir), None);
        assert_eq!(wm.snapshot().ranges, before);

        // The guard starts at the boot floor (less the second slack, round
        // 6), not at the segment seen.
        let floor = wm.boot_range_floor();
        assert_eq!(
            guard_pending_backlog(&wm, &dir, seq(900)),
            Some((floor, seq(900) - 1))
        );
    }

    #[test]
    fn test_fold_at_boot_migrates_overflow_and_registers_the_tail() {
        let dir = scratch("fold-free");
        segment(&dir, 0, 3);
        write_v1_overflow_file(&dir, seq(1_000));
        let wm = AppliedWatermark::new();
        // Unbound: refused.
        assert!(fold_at_boot(&wm, &dir).is_none());
        wm.bind(&dir);
        assert!(
            fold_at_boot(&wm, &scratch("fold-other")).is_none(),
            "another dir"
        );
        let fold = fold_at_boot(&wm, &dir).expect("fold");
        assert!(fold.migrated_overflow);
        assert!(fold.own_floor >= crate::wal_applied_watermark::wall_nanos() - 60_000_000_000);
        let tail = fold.tail.expect("a tail above the frozen floor");
        // Round 6: one more slack below the frozen floor, at a frame base.
        assert_eq!(tail.0, wm.boot_range_floor());
        assert!(tail.0 <= wm.boot_applied_floor() + 1 - REPLAY_REORDER_SLACK_SEQ);
        assert_eq!(tail.1, fold.own_floor);
        // Persisted: the next boot reads the bounded ranges, not the bit.
        let loaded = AppliedSnapshot::load(&dir).expect("file");
        assert!(!loaded.overflowed);
        assert!(loaded.range_has_unapplied(seq(0), seq(2)));
        assert!(fold_at_boot(&wm, &dir).is_none(), "once");
    }

    #[test]
    fn test_boot_applied_floor_frozen_at_bind_and_ignores_later_acks() {
        let dir = scratch("floor");
        crate::wal_applied_watermark::write_file_for_test(&dir, seq(100), seq(90));
        let wm = AppliedWatermark::new();
        assert_eq!(wm.boot_applied_floor(), 0, "unbound");
        wm.bind(&dir);
        let floor = seq(90) - crate::wal_applied_watermark::REPLAY_REORDER_SLACK_SEQ;
        assert_eq!(
            wm.boot_applied_floor(),
            floor,
            "min of both sinks, less slack"
        );
        wm.note_ticks_acked(seq(5_000));
        wm.note_depth_acked(seq(5_000));
        wm.seed(&AppliedSnapshot {
            hwm_ticks: seq(6_000),
            hwm_depth: seq(6_000),
            ..AppliedSnapshot::default()
        });
        wm.bind(&dir);
        assert_eq!(wm.boot_applied_floor(), floor, "frozen");
        // The guard never marks below the floor less the second slack
        // (round 6: `boot_range_floor`), at a frame base.
        segment(&dir, 10, 2);
        let got = guard_pending_backlog(&wm, &dir, seq(200)).expect("guarded");
        let range_floor = (floor - crate::wal_applied_watermark::REPLAY_REORDER_SLACK_SEQ)
            & !crate::ws_frame_spill::MAX_PACKET_INDEX;
        assert_eq!(wm.boot_range_floor(), range_floor);
        assert_eq!(got, (range_floor, seq(200) - 1));
        // No file: the floor is 0 and the guard starts at the segment.
        let fresh = scratch("floor-none");
        let wm2 = AppliedWatermark::new();
        wm2.bind(&fresh);
        assert_eq!(wm2.boot_applied_floor(), 0);
    }

    #[test]
    fn test_regression_next_day_shed_no_longer_overflows_after_boot_fold() {
        let dir = scratch("next-day");
        let day1 = seq(100);
        // Day 1: a shed, persisted with the file.
        let wm1 = AppliedWatermark::new();
        wm1.bind(&dir);
        wm1.note_unapplied(day1);
        persist_durably(&wm1, 1);
        // Next day, a shed whose bucket id is congruent mod 128 to day 1's.
        let next_day = day1 + 3 * ((UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT);
        let own_floor = day1 + 86_400 * 1_000_000_000;
        assert!(next_day > own_floor);

        // The bug, reproduced: without the fold the stale id collides.
        let unfolded = AppliedWatermark::new();
        unfolded.seed(&AppliedSnapshot::load(&dir).expect("file"));
        unfolded.note_unapplied(next_day);
        assert!(unfolded.snapshot().overflowed, "the 2026-10-20 collision");

        // The fix: the fold moves day 1's bucket into a range first.
        let wm2 = AppliedWatermark::new();
        wm2.bind(&dir);
        let fold = wm2.fold_at_boot(None, day1, own_floor).expect("fold");
        assert_eq!(fold.folded_buckets, 1);
        wm2.note_unapplied(next_day);
        let snap = wm2.snapshot();
        assert!(!snap.overflowed, "no collision after the fold");
        assert!(
            snap.range_has_unapplied(day1, day1),
            "day 1 still protected"
        );
        assert!(snap.range_has_unapplied(next_day, next_day));
    }

    #[test]
    fn test_regression_reloaded_overflow_clears_after_a_drain_past_its_height() {
        let dir = scratch("reloaded-clears");
        segment(&dir, 0, 3);
        write_v1_overflow_file(&dir, seq(500));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        // Before the fold a reloaded flag cannot be cleared (u64::MAX).
        wm.reset_unapplied_below(seq(4_000));
        assert!(wm.snapshot().overflowed);
        wm.fold_at_boot(waiting_segments(&dir, true).lowest, seq(2), seq(3_000))
            .expect("fold");
        assert!(!wm.snapshot().overflowed);
        // A completed drain whose ceiling is own_floor (the counter after the
        // fold's ratchet, nothing minted since) clears the pre-boot ranges.
        wm.reset_unapplied_below(seq(3_000));
        let snap = wm.snapshot();
        assert!(snap.ranges.is_empty(), "{:?}", snap.ranges);
        assert!(!snap.migrated_overflow);
        wm.note_ticks_acked(seq(6_000));
        wm.note_depth_acked(seq(6_000));
        persist_durably(&wm, 1);
        let loaded = AppliedSnapshot::load(&dir).expect("file");
        assert!(!loaded.overflowed, "the bit is gone from disk");
        assert!(loaded.range_is_applied(AppliedSink::Ticks, seq(4_000), seq(4_500)));
    }

    #[test]
    fn test_rollback_older_binary_decode_keeps_overflow_bit_while_migrated_ranges_exist() {
        let dir = scratch("rollback");
        segment(&dir, 0, 3);
        write_v1_overflow_file(&dir, seq(500));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.fold_at_boot(Some(seq(0)), seq(2), seq(3_000))
            .expect("fold");
        wm.note_ticks_acked(seq(9_000));
        wm.note_depth_acked(seq(9_000));
        persist_durably(&wm, 1);
        let bytes = std::fs::read(dir.join(APPLIED_WATERMARK_FILE)).expect("read");
        // `from_bytes` is exactly the version-1 decode an older binary runs.
        // A MIGRATED overflow is the one shape that still sets the bit (the
        // box was already overflowed for an older binary before this boot);
        // a clean box is pinned by
        // `test_rollback_old_decoder_on_a_clean_box_sees_buckets_not_overflow`.
        let older = AppliedSnapshot::from_bytes(&bytes, seq(9_999)).expect("v1 file");
        assert!(older.overflowed, "an older binary still replays everything");
        assert!(older.range_has_unapplied(seq(5_000), seq(5_001)));
        assert_ne!(older.ranges_gen, 0, "the pad names the sidecar");
        assert!(
            older
                .buckets
                .iter()
                .all(|&(_, mark)| mark != UNAPPLIED_BUCKET_RANGE_MARK),
            "with the bit set the real table is written unchanged"
        );
        let newer = AppliedSnapshot::load(&dir).expect("file");
        assert!(!newer.overflowed && newer.migrated_overflow);
        assert!(!newer.range_has_unapplied(seq(5_000), seq(5_001)));
        // Once no range is left the bit clears for both.
        wm.reset_unapplied_below(seq(4_000));
        persist_durably(&wm, 10 * LAG_NANOS);
        let bytes = std::fs::read(dir.join(APPLIED_WATERMARK_FILE)).expect("read");
        assert!(
            !AppliedSnapshot::from_bytes(&bytes, seq(9_999))
                .expect("v1")
                .overflowed
        );
    }

    #[test]
    fn test_add_unapplied_range_writes_the_sidecar_before_returning() {
        let dir = scratch("add-writes");
        let wm = AppliedWatermark::new();
        assert!(wm.add_unapplied_range(seq(1), seq(2)), "unbound: RAM only");
        assert!(!dir.join(UNAPPLIED_RANGES_FILE).exists());
        wm.bind(&dir);
        assert!(wm.add_unapplied_range(seq(10), seq(20)));
        let side = UnappliedRangesFile::load(&dir).expect("written at once");
        assert_eq!(side.ranges, wm.snapshot().ranges);
        assert_eq!(side.generation, wm.snapshot().ranges_gen);
        assert!(!wm.add_unapplied_range(seq(12), seq(15)), "already covered");
    }

    /// 2026-10-20 (PR W review, rollback): the live shape of nearly every
    /// boot. A clean previous session, an overnight gap, and the boot tail
    /// that spans it, wider than the whole bucket table. The first draft set
    /// the overflow bit for it, which an older binary seeds at `u64::MAX`
    /// and never clears: a rollback would have stopped every prune again.
    #[test]
    fn test_rollback_old_decoder_on_a_clean_box_sees_buckets_not_overflow() {
        let dir = scratch("rollback-clean");
        segment(&dir, 0, 3);
        // Yesterday: a clean stop, every frame acked.
        let yesterday = AppliedWatermark::new();
        yesterday.bind(&dir);
        yesterday.note_ticks_acked(seq(2));
        yesterday.note_depth_acked(seq(2));
        persist_durably(&yesterday, 1);
        // This binary boots the next morning.
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let own_floor = seq(16 * 3_600);
        let fold = wm
            .fold_at_boot(Some(seq(0)), seq(2), own_floor)
            .expect("fold");
        assert!(!fold.migrated_overflow);
        let tail = fold.tail.expect("the boot tail");
        assert!(
            tail.1 - tail.0 > (UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT,
            "the tail spans the night, wider than the whole bucket table"
        );
        assert_eq!(
            wm.snapshot().frameless_gap,
            Some((seq(2) + 1, own_floor)),
            "round 3: own_floor itself is never minted (the fold ratchets the counter to it)"
        );
        // Today's session acks well past the boot.
        wm.note_ticks_acked(seq(60_000));
        wm.note_depth_acked(seq(60_000));
        persist_durably(&wm, 1);
        let bytes = std::fs::read(dir.join(APPLIED_WATERMARK_FILE)).expect("read");

        // The older binary's decode: no bit, exact buckets.
        let older = AppliedSnapshot::from_bytes(&bytes, seq(99_999)).expect("v1 file");
        assert!(
            !older.overflowed,
            "a clean box hands an older binary no overflow"
        );
        assert!(
            older
                .buckets
                .iter()
                .any(|&(id, mark)| id != 0 && mark == UNAPPLIED_BUCKET_RANGE_MARK),
            "the ranges are written as buckets"
        );
        assert!(
            older.range_has_unapplied(tail.0, tail.0) && older.range_has_unapplied(seq(2), seq(2)),
            "yesterday's frames the file never vouched for stay protected"
        );
        assert!(
            older.range_is_applied(AppliedSink::Ticks, seq(58_500), seq(58_600)),
            "today's acked frames read as applied, so the older binary can prune"
        );
        // And it clears them like any shed bucket once a drain passes them.
        let old_binary = AppliedWatermark::new();
        old_binary.seed(&older);
        assert!(!old_binary.snapshot().overflowed);
        old_binary.reset_unapplied_below(seq(59_000));
        let drained = old_binary.snapshot();
        assert!(!drained.overflowed);
        assert!(drained.buckets.iter().all(|&(id, _)| id == 0));

        // This binary, reading the same file with its sidecar: the exact
        // ranges, and no range-encoded bucket left in the table.
        let newer = AppliedSnapshot::load(&dir).expect("file");
        assert!(!newer.overflowed);
        assert!(newer.buckets.iter().all(|&(id, _)| id == 0));
        assert!(newer.ranges.intersects(tail.0, tail.1));
        assert_eq!(newer.ranges_gen, wm.snapshot().ranges_gen);
    }

    /// Without a trusted sidecar the range-encoded buckets are ordinary
    /// unapplied buckets, and the next boot fold folds each into its exact
    /// bucket span: nothing the ranges protected is lost.
    #[test]
    fn test_range_buckets_without_a_trusted_sidecar_fold_back_into_their_spans() {
        let dir = scratch("range-buckets-untrusted");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(9_000));
        wm.note_depth_acked(seq(9_000));
        assert!(wm.add_unapplied_range(seq(100), seq(700)));
        persist_durably(&wm, 1);
        std::fs::remove_file(dir.join(UNAPPLIED_RANGES_FILE)).expect("rm sidecar");

        let next = AppliedWatermark::new();
        next.bind(&dir);
        let seeded = next.snapshot();
        assert!(!seeded.overflowed);
        assert!(seeded.ranges.is_empty(), "no sidecar to read");
        assert!(
            seeded
                .buckets
                .iter()
                .all(|&(id, mark)| id == 0 || mark == 1),
            "seeded as ordinary buckets, never the range mark"
        );
        let fold = next
            .fold_at_boot(None, seq(9_000), seq(20_000))
            .expect("fold");
        let first = seq(100) >> UNAPPLIED_BUCKET_SHIFT;
        let last = seq(700) >> UNAPPLIED_BUCKET_SHIFT;
        assert_eq!(u64::from(fold.folded_buckets), last - first + 1);
        let snap = next.snapshot();
        assert!(snap.buckets.iter().all(|&(id, _)| id == 0), "table empty");
        let bottom = first << UNAPPLIED_BUCKET_SHIFT;
        let top = ((last + 1) << UNAPPLIED_BUCKET_SHIFT) - 1;
        assert!(
            snap.ranges.intersects(bottom, bottom) && snap.ranges.intersects(top, top),
            "each bucket folds to its exact span"
        );
        let mut original = UnappliedRanges::new();
        original.add_range(seq(100), seq(700));
        assert!(snap.ranges.covers(&original), "coverage never shrinks");
    }

    #[test]
    fn test_clip_unapplied_ranges_to_clears_migrated_when_empty() {
        let dir = scratch("clip-migrated");
        segment(&dir, 0, 2);
        write_v1_overflow_file(&dir, seq(500));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.fold_at_boot(Some(seq(0)), seq(1), seq(3_000))
            .expect("fold");
        assert!(wm.snapshot().migrated_overflow);
        assert!(
            !wm.clip_unapplied_ranges_to(&[(1, u64::MAX)]),
            "a span over everything changes nothing"
        );
        assert!(wm.clip_unapplied_ranges_to(&[(seq(3_000) + 1, u64::MAX)]));
        let snap = wm.snapshot();
        assert!(snap.ranges.is_empty());
        assert!(!snap.migrated_overflow);
        let side = UnappliedRangesFile::load(&dir).expect("sidecar");
        assert!(side.ranges.is_empty() && !side.migrated_overflow);
    }

    /// The partial trim drops only sequences below its cut, keeps a range
    /// above it whole, cuts a straddling range at the cut, and writes the
    /// sidecar so a restart reloads the same set. A second identical call
    /// changes nothing.
    #[test]
    fn test_trim_unapplied_ranges_below_cuts_only_below_and_writes_the_sidecar() {
        let dir = scratch("trim-below");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        assert!(wm.add_unapplied_range(seq(10), seq(20)));
        assert!(wm.add_unapplied_range(seq(40), seq(60)));
        assert!(wm.add_unapplied_range(seq(100), seq(120)));
        let cut = seq(50);
        assert!(wm.trim_unapplied_ranges_below(cut));
        let snap = wm.snapshot();
        assert!(
            !snap.ranges.intersects(seq(10), cut - 1),
            "below the cut is gone"
        );
        assert!(
            snap.ranges.intersects(cut, cut),
            "a straddling range starts at the cut"
        );
        assert!(snap.ranges.intersects(seq(60), seq(60)));
        assert!(
            snap.ranges.intersects(seq(100), seq(120)),
            "above the cut is kept"
        );
        let side = UnappliedRangesFile::load(&dir).expect("sidecar");
        assert_eq!(
            side.ranges, snap.ranges,
            "the sidecar holds the trimmed set"
        );
        assert!(
            !wm.trim_unapplied_ranges_below(cut),
            "a repeat changes nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-10-20 (PR W review): sixteen sockets race to the writer, so a
    /// later record in a segment can carry a sequence BELOW the segment's
    /// first. A migrated range that started exactly at the oldest segment's
    /// first sequence let the replay skip that frame once acks passed it.
    #[test]
    fn test_migrated_range_keeps_a_record_below_its_segment_s_first_sequence() {
        let dir = scratch("migrate-reorder");
        let first = seq(100);
        // ~0.52 s earlier: inside the reorder slack, low bits zero.
        let earlier = first - (4_000 << 17);
        let mut bytes = record(first);
        bytes.extend_from_slice(&record(earlier));
        std::fs::write(dir.join(format!("ws-frames-{first:020}.wal")), bytes).expect("w");
        segment(&dir, 1_000, 3);
        // An older binary's unbounded overflow, its watermark past both.
        write_v1_overflow_file(&dir, seq(500));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let oldest = waiting_segments(&dir, true).lowest;
        assert_eq!(oldest, Some(first), "the probe reads the FIRST record");
        let fold = wm
            .fold_at_boot(oldest, seq(1_002), seq(3_000))
            .expect("fold");
        assert!(fold.migrated_overflow);
        wm.note_ticks_acked(seq(5_000));
        wm.note_depth_acked(seq(5_000));
        persist_durably(&wm, 1);
        let batch = crate::ws_frame_spill::replay_all_with_report_guarded(
            &dir,
            usize::MAX,
            || None,
            None,
            crate::ws_frame_spill::WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay");
        assert!(
            batch.frames.iter().any(|f| f.frame_seq == earlier),
            "the earlier record comes back"
        );
        assert!(batch.frames.iter().any(|f| f.frame_seq == first));
    }

    /// A known overflow (a wide mark made in this process before the fold)
    /// migrates as `[oldest - slack, its height]`, not as everything up to
    /// `own_floor`. (Round 3, L5: the low end is no longer tracked.)
    #[test]
    fn test_fold_migrates_a_known_overflow_up_to_its_height() {
        let dir = scratch("migrate-known");
        crate::wal_applied_watermark::write_file_for_test(&dir, seq(99_000), seq(99_000));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let height = seq(1_000) + ((UNAPPLIED_BUCKETS as u64 + 72) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied_range(seq(1_000), height);
        assert!(wm.snapshot().overflowed);
        let fold = wm
            .fold_at_boot(Some(seq(0)), seq(99_000), seq(100_000))
            .expect("fold");
        assert!(fold.migrated_overflow);
        let snap = wm.snapshot();
        assert!(!snap.overflowed);
        assert_eq!(
            snap.ranges.as_slice()[0],
            (seq(0) - REPLAY_REORDER_SLACK_SEQ, height)
        );
        assert_eq!(
            snap.migrated_span,
            Some((seq(0) - REPLAY_REORDER_SLACK_SEQ, height))
        );
        assert!(
            !snap.range_has_unapplied(height + 1, height + 2),
            "nothing above the height is migrated"
        );
    }

    #[test]
    fn test_is_bound_to_only_the_bound_directory() {
        let a = scratch("bound-a");
        let wm = AppliedWatermark::new();
        assert!(!wm.is_bound_to(&a), "unbound");
        wm.bind(&a);
        assert!(wm.is_bound_to(&a));
        assert!(!wm.is_bound_to(&scratch("bound-b")));
    }

    #[test]
    fn test_reset_unapplied_below_keeps_ranges_above_the_ceiling() {
        let wm = AppliedWatermark::new();
        wm.add_unapplied_range(seq(10), seq(20));
        wm.add_unapplied_range(seq(100), seq(200));
        wm.reset_unapplied_below(seq(50));
        let snap = wm.snapshot();
        assert_eq!(snap.ranges.as_slice(), &[(seq(100), seq(200))]);
        wm.reset_unapplied();
        assert!(wm.snapshot().ranges.is_empty());
    }

    // ---------------------------------------------------------------------
    // Guard and trim
    // ---------------------------------------------------------------------

    #[test]
    fn test_regression_multi_day_backlog_never_overflows_the_bucket_table() {
        let dir = scratch("multi-day");
        segment(&dir, 0, 3);
        // 3,176 buckets, the span measured on 2026-10-20.
        let ceiling = seq(0) + (3_176u64 << UNAPPLIED_BUCKET_SHIFT);
        // The old mark: one bucket per id, overflowed at once.
        let old = AppliedWatermark::new();
        old.note_unapplied_range(seq(0), ceiling - 1);
        assert!(old.snapshot().overflowed, "the bug the guard used to cause");
        // The guard now writes one bounded range.
        let wm = AppliedWatermark::new();
        assert_eq!(
            guard_pending_backlog(&wm, &dir, ceiling),
            Some((seq(0) - REPLAY_REORDER_SLACK_SEQ, ceiling - 1))
        );
        let snap = wm.snapshot();
        assert!(!snap.overflowed);
        assert_eq!(snap.ranges.len(), 1);
        assert!(snap.range_has_unapplied(seq(0), seq(2)));
        assert!(snap.range_has_unapplied(ceiling - 1, ceiling - 1));
        assert!(!snap.range_has_unapplied(ceiling, ceiling + 10));
        assert!(
            snap.buckets.iter().all(|&(id, _)| id == 0),
            "the table is untouched"
        );
    }

    /// The guard must never mark the bucket table again. Source scan: the
    /// defect would be a CALL, and a behavioural test of one span can miss a
    /// branch that only a wider one reaches.
    #[test]
    fn test_guard_never_calls_note_unapplied_range() {
        let src = include_str!("wal_unapplied_ranges.rs");
        assert!(guard_body_is_bounded(src));
        let bitten = "pub fn guard_pending_backlog(\n    wm: &W,\n) {\n    wm.note_unapplied_range(lo, hi);\n}\n";
        assert!(!guard_body_is_bounded(bitten), "the scan bites");
    }

    fn guard_body_is_bounded(src: &str) -> bool {
        let Some(start) = src.find("pub fn guard_pending_backlog(") else {
            return false;
        };
        let body = &src[start..];
        // The closing brace is written as the escape u7d so every line of this
        // test module carries balanced braces: the banned-pattern scanner finds
        // the end of `mod tests` by counting them, and one unmatched closing
        // brace in a string ended the module early and scanned the rest of the
        // tests as production code. The same holds at the two sites below.
        let end = body.find("\n\u{7d}\n").map_or(body.len(), |i| i + 3);
        let code: String = body[..end]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        !code.contains("note_unapplied_range") && code.contains("add_unapplied_range(")
    }

    /// The fold runs after the sequence is seeded and the watermark bound,
    /// and before either WAL thread spawns.
    #[test]
    fn test_boot_tail_registered_before_writer_spawn() {
        let src = include_str!("ws_frame_spill.rs");
        assert!(fold_order_ok(src));
        let wrong = "fn new_with_guard<P>() {\n    let d = seed_frame_seq_from_disk(&wal_dir);\n    applied_watermark().bind(&wal_dir);\n    let s = spawn_wal_syncer(x);\n    crate::wal_unapplied_ranges::fold_at_boot(w, &wal_dir);\n    let t = thread::Builder::new();\n}\n";
        assert!(
            !fold_order_ok(wrong),
            "the scan bites on a fold after the syncer"
        );
    }

    fn fold_order_ok(src: &str) -> bool {
        let Some(start) = src.find("fn new_with_guard<") else {
            return false;
        };
        let body = &src[start..];
        let find = |needle: &str| body.find(needle);
        match (
            find("seed_frame_seq_from_disk(&wal_dir)"),
            find("applied_watermark().bind(&wal_dir)"),
            find("wal_unapplied_ranges::fold_at_boot("),
            find("spawn_wal_syncer("),
            find("thread::Builder::new()"),
        ) {
            (Some(seed), Some(bind), Some(fold), Some(syncer), Some(writer)) => {
                seed < bind && bind < fold && fold < syncer && fold < writer
            }
            _ => false,
        }
    }

    #[test]
    fn test_trim_to_waiting_keeps_ranges_over_waiting_segments() {
        let dir = scratch("trim");
        let wm = AppliedWatermark::new();
        assert_eq!(trim_to_waiting(&wm, &dir), None, "unbound");
        wm.bind(&dir);
        wm.add_unapplied_range(seq(0), seq(100));
        wm.add_unapplied_range(seq(1_000), seq(2_000));
        assert_eq!(
            trim_to_waiting(&wm, &dir),
            None,
            "nothing waiting: keep all"
        );
        assert_eq!(wm.snapshot().ranges.len(), 2);
        segment(&dir, 1_500, 3);
        let below = trim_to_waiting(&wm, &dir).expect("trimmed");
        assert_eq!(
            below,
            seq(1_500) - crate::wal_applied_watermark::REPLAY_REORDER_SLACK_SEQ
        );
        let snap = wm.snapshot();
        assert_eq!(snap.ranges.as_slice(), &[(below, seq(2_000))]);
        assert!(snap.range_has_unapplied(seq(1_500), seq(1_502)));
        let loaded = AppliedSnapshot::load(&dir).expect("persisted");
        assert_eq!(loaded.ranges, snap.ranges);
    }

    #[test]
    fn test_trim_to_waiting_refuses_an_unknown_segment() {
        let dir = scratch("trim-unknown");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.add_unapplied_range(seq(0), seq(100));
        segment(&dir, 1_500, 3);
        std::fs::write(
            dir.join("ws-frames-00000000000000000002.wal"),
            torn_then(seq(1)),
        )
        .expect("w");
        assert_eq!(trim_to_waiting(&wm, &dir), None);
        assert_eq!(wm.snapshot().ranges.len(), 1, "nothing trimmed");
    }

    /// 2026-10-20 (PR W review): where a waiting segment is followed by a
    /// gap, its span ends at its last intact record plus the slack, found by
    /// a backward search that steps over a torn tail.
    #[test]
    fn test_waiting_spans_clip_a_gap_to_the_segment_s_last_record() {
        let dir = scratch("spans-gap");
        assert_eq!(waiting_spans(&dir), None, "nothing waiting");
        let old = segment(&dir, 0, 5);
        segment(&dir, 10_000, 3);
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let expected = vec![
            (seq(0) - slack, seq(4) + slack),
            (seq(10_000) - slack, u64::MAX),
        ];
        assert_eq!(waiting_spans(&dir), Some(expected.clone()));
        // A torn record at the end is stepped over.
        let mut bytes = std::fs::read(&old).expect("read");
        bytes.extend_from_slice(b"TVW4\x01\x02\x03");
        std::fs::write(&old, bytes).expect("write");
        assert_eq!(waiting_spans(&dir), Some(expected));
        // Segments closer than the gap threshold keep the coarse span.
        let near = scratch("spans-near");
        segment(&near, 0, 2);
        segment(&near, 60, 2);
        assert_eq!(waiting_spans(&near), Some(vec![(seq(0) - slack, u64::MAX)]));
    }

    /// 2026-10-20 (PR W review): a range over a stretch whose segments were
    /// archived or pruned is clipped, even above a backlog that never drains.
    /// Before, only what lay below the oldest waiting segment was cut, so
    /// one boot tail per day piled up until the 32 slots merged them over
    /// acked sessions.
    #[test]
    fn test_trim_to_waiting_clips_a_range_over_a_stretch_with_no_waiting_segment() {
        let dir = scratch("trim-clip");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        // An old backlog segment that never drains, and today's newest one.
        segment(&dir, 0, 5);
        segment(&dir, 50_000, 3);
        // Ranges from the days in between, their segments since archived.
        wm.add_unapplied_range(seq(0), seq(20_000));
        wm.add_unapplied_range(seq(30_000), seq(30_500));
        let slack = REPLAY_REORDER_SLACK_SEQ;
        assert_eq!(trim_to_waiting(&wm, &dir), Some(seq(0) - slack));
        let snap = wm.snapshot();
        assert_eq!(snap.ranges.as_slice(), &[(seq(0), seq(4) + slack)]);
        assert!(
            !snap.range_has_unapplied(seq(5_000), seq(30_600)),
            "the stretch with no waiting segment is free"
        );
        assert!(
            snap.range_has_unapplied(seq(0), seq(4)),
            "the backlog stays protected"
        );
        assert_eq!(
            AppliedSnapshot::load(&dir).expect("persisted").ranges,
            snap.ranges
        );
    }

    #[test]
    fn test_unapplied_ranges_clip_to_splits_keeps_and_drops() {
        let mut r = UnappliedRanges::new();
        r.add_range(10, 100);
        r.add_range(200, 300);
        let mut whole = r;
        assert!(!whole.clip_to(&[(1, 1_000)]), "inside one span: unchanged");
        assert_eq!(whole, r);
        assert!(r.clip_to(&[(20, 30), (50, 60), (250, 260), (400, 500)]));
        assert_eq!(r.as_slice(), &[(20, 30), (50, 60), (250, 260)]);
        assert!(r.clip_to(&[]), "no span: nothing left");
        assert!(r.is_empty());
    }

    /// 2026-10-20 (PR W review): a QuestDB suspension that runs across an
    /// overnight gap (longer than the bucket table's ~9.8 h) is a bounded
    /// range now, never an unbounded overflow that taints every later
    /// segment.
    #[test]
    fn test_suspicion_across_an_overnight_gap_is_a_range_not_an_overflow() {
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(100));
        wm.note_depth_acked(seq(100));
        wm.note_questdb_probe(true);
        wm.note_ticks_acked(seq(400));
        wm.note_depth_acked(seq(400));
        wm.note_questdb_probe(false);
        let overnight = seq(400 + 16 * 3_600);
        wm.note_ticks_acked(overnight);
        wm.note_depth_acked(overnight);
        wm.note_questdb_probe(true);
        let snap = wm.snapshot();
        assert!(!snap.overflowed, "no unbounded bit");
        assert!(
            snap.buckets.iter().all(|&(id, _)| id == 0),
            "the bucket table is untouched"
        );
        assert!(
            snap.range_has_unapplied(seq(150), seq(150)),
            "the detection window"
        );
        assert!(
            snap.range_has_unapplied(seq(30_000), seq(30_000)),
            "the parked night"
        );
        let above = overnight + 2 * REPLAY_REORDER_SLACK_SEQ;
        assert!(!snap.range_has_unapplied(above, above + 1));
    }

    // ---------------------------------------------------------------------
    // Replay and prune honour the ranges
    // ---------------------------------------------------------------------

    #[test]
    fn test_per_record_skip_honours_ranges_after_live_acks_pass_them() {
        let dir = scratch("per-record");
        segment(&dir, 0, 5);
        segment(&dir, 1_000, 5);
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.add_unapplied_range(seq(2), seq(3));
        wm.note_ticks_acked(seq(5_000));
        wm.note_depth_acked(seq(5_000));
        persist_durably(&wm, 1);
        let batch = crate::ws_frame_spill::replay_all_with_report_guarded(
            &dir,
            usize::MAX,
            || None,
            None,
            crate::ws_frame_spill::WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay");
        let seqs: Vec<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
        assert_eq!(
            seqs,
            vec![seq(2), seq(3)],
            "only the ranged frames come back"
        );
    }

    #[test]
    fn test_prune_never_deletes_a_segment_under_a_range_or_an_unbounded_overflow() {
        // Under a range, with a verified copy: kept.
        let dir = scratch("prune-range");
        let a = segment(&dir, 0, 3);
        let b = segment(&dir, 1_000, 3);
        let c = segment(&dir, 2_000, 3);
        let now = SystemTime::now();
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
            mark_uploaded(&dir, p);
        }
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.add_unapplied_range(seq(1_001), seq(1_001));
        wm.note_ticks_acked(seq(9_000));
        wm.note_depth_acked(seq(9_000));
        persist_durably(&wm, 1);
        let snap = AppliedSnapshot::load(&dir).expect("file");
        let out = prune_with_copies(&dir, &snap);
        assert!(!a.exists(), "applied and copied: aged out");
        assert!(b.exists(), "under a range: kept despite its copy");
        assert!(c.exists(), "the newest segment is never judged");
        assert_eq!(out.deleted, 1);

        // An unbounded v1 overflow: nothing goes, copies or not.
        let dir = scratch("prune-v1");
        let segs = [
            segment(&dir, 0, 3),
            segment(&dir, 1_000, 3),
            segment(&dir, 2_000, 3),
        ];
        for p in &segs {
            backdate(p, now, 400_000);
            mark_uploaded(&dir, p);
        }
        write_v1_overflow_file(&dir, seq(9_000));
        let snap = AppliedSnapshot::load(&dir).expect("file");
        let out = prune_with_copies(&dir, &snap);
        assert_eq!(out.deleted, 0);
        assert!(segs.iter().all(|p| p.exists()));
    }

    /// The live shape of 2026-10-20: a box booting onto an older binary's
    /// overflow bit. Before this change every segment, including the ones
    /// written and acknowledged after this boot, stayed unprunable forever.
    #[test]
    fn test_regression_20_oct_shape() {
        let dir = scratch("20-oct");
        let pre: Vec<PathBuf> = [0, 1_000, 2_000]
            .iter()
            .map(|&s| segment(&dir, s, 3))
            .collect();
        write_v1_overflow_file(&dir, seq(2_500));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let own_floor = seq(3_000);
        wm.fold_at_boot(waiting_segments(&dir, true).lowest, seq(2_002), own_floor)
            .expect("fold");
        persist_durably(&wm, 1);
        // This session: three segments, every frame acked past the 60 s lag.
        let new: Vec<PathBuf> = [4_000, 5_000, 6_000]
            .iter()
            .map(|&s| segment(&dir, s, 3))
            .collect();
        wm.note_ticks_acked(seq(6_100));
        wm.note_depth_acked(seq(6_100));
        persist_durably(&wm, 10 * LAG_NANOS);
        let now = SystemTime::now();
        for p in pre.iter().chain(new.iter()) {
            backdate(p, now, 400_000);
            mark_uploaded(&dir, p);
        }
        let snap = AppliedSnapshot::load(&dir).expect("file");
        assert!(!snap.overflowed, "the bit is bounded by the sidecar");
        let out = prune_with_copies(&dir, &snap);
        assert!(
            pre.iter().all(|p| p.exists()),
            "every pre-boot segment refused"
        );
        assert!(
            !new[0].exists() && !new[1].exists(),
            "acked + uploaded: prunable"
        );
        assert!(new[2].exists(), "the newest segment is never judged");
        assert_eq!(out.deleted, 2);
        assert!(out.age_kept_unapplied >= 3);
    }

    // ---------------------------------------------------------------------
    // 2026-10-20, PR W review round 3
    // ---------------------------------------------------------------------

    /// M3: a full set merges across the lowest gap no waiting span
    /// intersects; with every gap holding a span, the lowest pair.
    #[test]
    fn test_add_range_avoiding_merges_across_a_gap_no_span_intersects() {
        let mut r = UnappliedRanges::new();
        for k in 0..UNAPPLIED_RANGE_SLOTS as u64 {
            assert!(r.add_range(k * 100 + 1, k * 100 + 11));
        }
        let before = r;
        // Spans cover every range and every gap except the one between #5
        // and #6, which lies over nothing waiting.
        let spans = [(1, 511), (601, u64::MAX)];
        assert!(r.add_range_avoiding(1_000_000, 1_000_010, &spans));
        assert_eq!(r.len(), UNAPPLIED_RANGE_SLOTS);
        assert!(r.covers(&before));
        assert_eq!(r.as_slice()[5], (501, 611), "merged across the free gap");
        assert_eq!(r.as_slice()[0], (1, 11), "the lowest pair is left alone");
        // Every gap holds a span: the lowest pair.
        let mut all = before;
        assert!(all.add_range_avoiding(1_000_000, 1_000_010, &[(1, u64::MAX)]));
        assert_eq!(all.as_slice()[0], (1, 111));
        // `spans_intersect` itself.
        assert!(spans_intersect(&spans, 500, 520));
        assert!(!spans_intersect(&spans, 512, 600));
        assert!(spans_intersect(&spans, 600, 601));
        assert!(!spans_intersect(&[], 1, u64::MAX));
    }

    /// M3: the boot fold steers a merge with the waiting spans.
    #[test]
    fn test_fold_at_boot_with_spans_merges_across_a_free_gap() {
        let wm = AppliedWatermark::new();
        for k in 0..UNAPPLIED_RANGE_SLOTS as u64 {
            wm.add_unapplied_range(seq(k * 100), seq(k * 100 + 10));
        }
        let spans = [
            (seq(0), seq(510)),
            (seq(600), seq(3_200)),
            (seq(5_000) - REPLAY_REORDER_SLACK_SEQ, u64::MAX),
        ];
        let fold = wm
            .fold_at_boot_with_spans(Some(seq(5_000)), seq(10_000), seq(20_000), &spans)
            .expect("fold");
        assert_eq!(
            fold.tail,
            Some((seq(5_000) - REPLAY_REORDER_SLACK_SEQ, seq(20_000)))
        );
        let snap = wm.snapshot();
        assert_eq!(snap.ranges.len(), UNAPPLIED_RANGE_SLOTS);
        assert_eq!(snap.ranges.as_slice()[0], (seq(0), seq(10)));
        assert_eq!(snap.ranges.as_slice()[5], (seq(500), seq(610)));
    }

    /// M1: the wrapper rounds `own_floor` up to a frame base and ratchets
    /// the frame counter to it, so every later mint is above it whatever
    /// the wall clock reads.
    #[test]
    fn test_fold_wrapper_ratchets_the_frame_counter_past_own_floor() {
        let dir = scratch("fold-ratchet");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let fold = fold_at_boot(&wm, &dir).expect("fold");
        let mask = crate::ws_frame_spill::MAX_PACKET_INDEX;
        assert_eq!(fold.own_floor & mask, 0, "a frame base");
        assert!(fold.own_floor >= fold.boot_frame_high);
        assert!(crate::ws_frame_spill::current_frame_seq() >= fold.own_floor);
        let minted = crate::ws_frame_spill::next_frame_seq();
        assert!(minted > fold.own_floor, "{minted} <= {}", fold.own_floor);
        assert_eq!(wm.boot_frame_high(), Some(fold.boot_frame_high));
    }

    /// M1: a range over frames inside `(boot_frame_high, own_floor)` (minted
    /// there by a watermark folded without the ratchet) survives a drained
    /// reset whose ceiling sits below `own_floor`. The first draft cut below
    /// `own_floor + 1` and cleared it.
    #[test]
    fn test_drained_reset_keeps_a_suspension_range_above_the_ceiling() {
        let wm = AppliedWatermark::new();
        wm.fold_at_boot(None, seq(1_000), seq(3_000)).expect("fold");
        wm.reset_unapplied_below(seq(3_000) + 1);
        assert!(wm.snapshot().ranges.is_empty(), "the tail clears");
        wm.add_unapplied_range(seq(2_000), seq(2_100));
        wm.reset_unapplied_below(seq(1_500));
        let snap = wm.snapshot();
        assert!(
            snap.ranges.intersects(seq(2_000), seq(2_100)),
            "{:?}",
            snap.ranges
        );
        // A ceiling at `own_floor` (the ratchet's value, never minted) clears
        // what lies below it, as before.
        wm.reset_unapplied_below(seq(3_000));
        assert!(wm.snapshot().ranges.is_empty());
        // No fold in this process: the ceiling alone.
        let unfolded = AppliedWatermark::new();
        unfolded.add_unapplied_range(seq(2_000), seq(2_100));
        unfolded.reset_unapplied_below(seq(2_050));
        assert_eq!(
            unfolded.snapshot().ranges.as_slice(),
            &[(seq(2_050), seq(2_100))]
        );
        assert_eq!(unfolded.boot_frame_high(), None);
    }

    /// M2: the last session ran with no depth sink, so its file says
    /// `depth_untracked` and `hwm_depth < hwm_ticks`. This boot has a depth
    /// sink and its catch-up stops early. The boot floor took `hwm_ticks`
    /// alone, so the tail and the guard started above the depth frames in
    /// `(hwm_depth, hwm_ticks]`; once live depth acks passed them, the next
    /// replay skipped them.
    #[test]
    fn test_regression_depth_untracked_file_keeps_depth_frames_between_the_watermarks() {
        let dir = scratch("depth-untracked");
        segment(&dir, 0, 3);
        segment(&dir, 600, 3);
        segment(&dir, 2_000, 3);
        let old = AppliedWatermark::new();
        old.bind(&dir);
        old.mark_depth_untracked();
        old.note_ticks_acked(seq(1_000));
        old.note_depth_acked(seq(500));
        persist_durably(&old, 1);
        assert!(AppliedSnapshot::load(&dir).expect("file").depth_untracked);

        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        assert_eq!(
            wm.boot_applied_floor(),
            seq(500) - REPLAY_REORDER_SLACK_SEQ,
            "the lower watermark, whatever the flag says"
        );
        wm.fold_at_boot(waiting_segments(&dir, true).lowest, seq(2_002), seq(3_000))
            .expect("fold");
        wm.mark_depth_tracked();
        let _guarded = guard_pending_backlog(&wm, &dir, seq(3_000));
        wm.note_ticks_acked(seq(9_000));
        wm.note_depth_acked(seq(9_000));
        persist_durably(&wm, 1);
        let batch = crate::ws_frame_spill::replay_all_with_report_guarded(
            &dir,
            usize::MAX,
            || None,
            None,
            crate::ws_frame_spill::WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay");
        for s in [600, 601, 602] {
            assert!(
                batch.frames.iter().any(|f| f.frame_seq == seq(s)),
                "frame {s} is replayed"
            );
        }
    }

    /// M3: the guard's top is clamped to the boot frame high once the fold
    /// has run; this process's own frames are not the leftover backlog.
    #[test]
    fn test_guard_pending_backlog_is_clamped_to_the_boot_frame_high() {
        let dir = scratch("guard-clamp");
        segment(&dir, 0, 3);
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.fold_at_boot(Some(seq(0)), seq(2), seq(3_000))
            .expect("fold");
        let got = guard_pending_backlog(&wm, &dir, seq(5_000)).expect("guarded");
        assert_eq!(got.1, seq(2), "not seq(5_000) - 1");
    }

    /// L3: with no `applied.tvaw`, the tail starts at the oldest waiting
    /// segment less the slack, not at 1.
    #[test]
    fn test_boot_tail_starts_at_the_oldest_waiting_segment_without_a_file() {
        let dir = scratch("tail-no-file");
        segment(&dir, 1_000, 3);
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        assert_eq!(wm.boot_applied_floor(), 0);
        let fold = wm
            .fold_at_boot(Some(seq(1_000)), seq(1_002), seq(3_000))
            .expect("fold");
        assert_eq!(
            fold.tail,
            Some((seq(1_000) - REPLAY_REORDER_SLACK_SEQ, seq(3_000)))
        );
        // Unknown oldest: from the floor, as before.
        let wm2 = AppliedWatermark::new();
        let fold2 = wm2
            .fold_at_boot(None, seq(1_002), seq(3_000))
            .expect("fold");
        assert_eq!(fold2.tail, Some((1, seq(3_000))));
    }

    /// M4: the migrated flag (and the older binary's overflow bit) clears
    /// once no range intersects the migrated span, not only once no range
    /// at all is left. Before, one later boot tail kept it set for ever.
    #[test]
    fn test_migrated_flag_clears_once_no_range_intersects_the_migrated_span() {
        let dir = scratch("migrated-span");
        segment(&dir, 0, 3);
        write_v1_overflow_file(&dir, seq(500));
        let wm1 = AppliedWatermark::new();
        wm1.bind(&dir);
        wm1.fold_at_boot(Some(seq(0)), seq(2), seq(3_000))
            .expect("fold");
        let span = (seq(0) - REPLAY_REORDER_SLACK_SEQ, seq(3_000));
        assert_eq!(wm1.snapshot().migrated_span, Some(span));
        ack_both(&wm1, seq(6_000));
        persist_durably(&wm1, 1);
        assert_eq!(
            UnappliedRangesFile::load(&dir)
                .expect("sidecar")
                .migrated_span,
            Some(span)
        );

        // The next boot: the span is read back, and a new tail lies above it.
        let wm2 = AppliedWatermark::new();
        wm2.bind(&dir);
        assert_eq!(wm2.snapshot().migrated_span, Some(span), "seeded");
        let fold = wm2
            .fold_at_boot(Some(seq(0)), seq(6_002), seq(9_000))
            .expect("fold");
        assert!(fold.tail.is_some());
        // The backlog is gone (archived): the clip removes the migrated range.
        assert!(wm2.clip_unapplied_ranges_to(&[(seq(5_000), u64::MAX)]));
        let snap = wm2.snapshot();
        assert!(!snap.ranges.is_empty(), "the new tail stays");
        assert!(!snap.migrated_overflow && snap.migrated_span.is_none());
        ack_both(&wm2, seq(12_000));
        persist_durably(&wm2, 1);
        let bytes = std::fs::read(dir.join(APPLIED_WATERMARK_FILE)).expect("read");
        let older = AppliedSnapshot::from_bytes(&bytes, u64::MAX).expect("v1");
        assert!(!older.overflowed, "an older binary sees no overflow bit");
        assert!(
            older.range_has_unapplied(seq(6_000), seq(6_000)),
            "the tail is written as buckets"
        );
    }

    /// L1: a segment whose first record is unreadable and that sits in
    /// `settling/unreadable/` no longer stops every trim: the ranges are cut
    /// below the lowest readable span (never clipped above it).
    #[test]
    fn test_trim_to_waiting_cuts_below_a_segment_set_apart_as_unreadable() {
        let dir = scratch("trim-unreadable");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let apart = dir
            .join(crate::wal_replay_settle::SETTLING_SUBDIR)
            .join(crate::wal_replay_settle::SETTLING_UNREADABLE_SUBDIR);
        std::fs::create_dir_all(&apart).expect("dir");
        std::fs::write(
            apart.join("ws-frames-00000000000000000003.wal"),
            torn_then(seq(1)),
        )
        .expect("w");
        segment(&dir, 1_500, 3);
        segment(&dir, 50_000, 3);
        wm.add_unapplied_range(seq(0), seq(100));
        wm.add_unapplied_range(seq(20_000), seq(20_100));
        assert_eq!(waiting_spans(&dir), None, "the full spans stay unknown");
        let below = trim_to_waiting(&wm, &dir).expect("a partial trim");
        assert_eq!(below, seq(1_500) - REPLAY_REORDER_SLACK_SEQ);
        assert_eq!(
            wm.snapshot().ranges.as_slice(),
            &[(seq(20_000), seq(20_100))],
            "cut below, never clipped above"
        );
        // An unreadable segment anywhere else still refuses.
        std::fs::write(
            dir.join("ws-frames-00000000000000000004.wal"),
            torn_then(seq(1)),
        )
        .expect("w");
        wm.add_unapplied_range(seq(0), seq(100));
        assert_eq!(trim_to_waiting(&wm, &dir), None);
        assert_eq!(wm.snapshot().ranges.len(), 2);
    }

    /// L8: the probe's range writes run through `off_worker`, so a
    /// multi-thread runtime's worker is moved aside for the fsync.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_note_questdb_probe_writes_ranges_off_the_runtime_worker() {
        let dir = scratch("probe-off-worker");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        ack_both(&wm, seq(100));
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(false);
        assert!(UnappliedRangesFile::load(&dir).is_some(), "written");
        ack_both(&wm, seq(200));
        wm.note_questdb_probe(true);
        let src = include_str!("wal_applied_watermark.rs");
        // Round 4 (C): the body moved into `note_questdb_probe_at`; round 7
        // moved it again, into `note_questdb_probe_full` (it takes the basis).
        let needle = concat!("fn ", "note_questdb_probe_full(");
        let start = src.find(needle).unwrap_or(src.len());
        assert!(start < src.len(), "the probe is in the source");
        let body = &src[start..];
        let end = body.find("\n    \u{7d}\n").unwrap_or(body.len());
        let body = &body[..end];
        // Round 4: the calls must sit INSIDE the closures, not merely be
        // counted; moving one add out of its closure kept both counts.
        assert_eq!(
            off_worker_nesting(body),
            // W4 (2026-10-30): the exit adds its ranges through one
            // `add_exit_ranges` call (both sinks, one sidecar write).
            Some((3, 3, 2)),
            "every range add and a persist inside one of the two off_worker closures"
        );
        let bitten = "pub fn note_questdb_probe(&self) {\n    self.add_unapplied_range(a, b);\n    \
                      crate::off_worker::off_worker(|| {\n        self.add_unapplied_range(c, d);\n        \
                      self.persist_now();\n    });\n}\n";
        assert_eq!(
            off_worker_nesting(bitten),
            Some((1, 2, 1)),
            "the check sees an add moved out of its closure"
        );
    }

    /// `(adds inside an off_worker closure, adds in total, closures that
    /// also persist)` for a function body, or `None` when a closure's braces
    /// do not balance. Source scan, test-only.
    fn off_worker_nesting(body: &str) -> Option<(usize, usize, usize)> {
        let needle = "off_worker::off_worker(|| \u{7b}";
        let mut inside = 0;
        let mut persisting = 0;
        let mut rest = body;
        while let Some(at) = rest.find(needle) {
            let open = at + needle.len() - 1;
            let mut depth = 0usize;
            let mut close = None;
            for (i, b) in rest.bytes().enumerate().skip(open) {
                if b == b'{' {
                    depth += 1;
                } else if b == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
            }
            let close = close?;
            let closure = &rest[open..=close];
            inside += closure.matches("add_unapplied_range(").count()
                + closure.matches("add_exit_ranges(").count();
            if closure.contains("persist_now()") {
                persisting += 1;
            }
            rest = &rest[close..];
        }
        Some((
            inside,
            body.matches("add_unapplied_range(").count() + body.matches("add_exit_ranges(").count(),
            persisting,
        ))
    }

    /// Round 4 (finding 1): the ring reorders frames from sixteen sockets, so
    /// a frame minted just before the one that set the last clean watermark
    /// can be acked AFTER that probe. If a suspension episode then starts,
    /// that frame may never have been applied. The episode's range used to
    /// start AT the watermark, so after the live acks moved past it the next
    /// boot dropped the frame as applied. It now starts one slack lower.
    #[test]
    fn test_regression_suspension_range_keeps_a_reordered_ack_below_the_clean_watermark() {
        let dir = scratch("suspend-reorder");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let mask = crate::ws_frame_spill::MAX_PACKET_INDEX;
        let b2 = seq(1_000);
        let b0 = (b2 - REPLAY_REORDER_SLACK_SEQ / 2) & !mask;
        ack_both(&wm, b2);
        wm.note_questdb_probe(true);
        // Minted before b2, acked after the clean probe.
        ack_both(&wm, b0);
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(true);
        ack_both(&wm, b2 + 20 * REPLAY_REORDER_SLACK_SEQ);
        persist_durably(&wm, 1);
        let snap = AppliedSnapshot::load(&dir).expect("file");
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, b0),
            "the reordered frame replays"
        );
        assert!(!snap.frame_is_applied(AppliedSink::Depth, b0));
        assert!(
            snap.frame_is_applied(AppliedSink::Ticks, b2 - 3 * REPLAY_REORDER_SLACK_SEQ),
            "a frame more than the slack below stays applied"
        );
        // A range start that falls INSIDE a frame starts at its base.
        let lo = snap.ranges.as_slice()[0].0;
        assert_eq!(lo & mask, 0, "{lo} is a frame base");
    }

    /// Round 4 (finding 3): an overflow of the bucket table in RAM whose
    /// height is KNOWN was written as the bare bit, which loads at height
    /// `u64::MAX`: from then on every prune pass refused every segment and
    /// the next boot migrated the bit over the whole session. A catch-up
    /// round re-appends earlier days' frames with their original sequence,
    /// so a failed rescue can span a day boundary (130 buckets) or collide
    /// with a shed of today on one slot. It is now persisted as the bounded
    /// range `[low, height]`.
    #[test]
    fn test_regression_in_session_overflow_of_known_height_persists_as_a_bounded_range() {
        let dir = scratch("overflow-bounded");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let bucket = 1u64 << UNAPPLIED_BUCKET_SHIFT;
        let lo = seq(1_000);
        let hi = lo + (UNAPPLIED_BUCKETS as u64 + 2) * bucket;
        wm.note_unapplied_range(lo, hi);
        let shed = hi + 10 * bucket;
        let collide = shed + UNAPPLIED_BUCKETS as u64 * bucket;
        wm.note_unapplied(shed);
        wm.note_unapplied(collide);
        assert!(wm.snapshot().overflowed, "both shapes overflow the table");
        let top = collide + 20 * bucket;
        ack_both(&wm, top);
        persist_durably(&wm, 1);
        assert!(
            !wm.snapshot().overflowed,
            "the RAM flag is bounded and cleared"
        );
        let snap = AppliedSnapshot::load(&dir).expect("file");
        assert!(
            !(snap.overflowed && snap.overflow_high_seq == u64::MAX),
            "a known height is never written as the unbounded bit"
        );
        assert!(!snap.overflowed);
        // O(1) EXEMPT: four fixed probes, test only
        for s in [lo, hi, shed, collide] {
            assert!(!snap.frame_is_applied(AppliedSink::Ticks, s), "{s} replays");
        }
        let above = collide + 3 * bucket;
        assert!(
            snap.range_is_applied(AppliedSink::Ticks, above, above + bucket),
            "a segment above the height, acked, can be pruned"
        );
    }

    /// Review round 3 (second pass): the in-session bound cleared the flag
    /// but kept the overflow bounds, which the overflow arms only ever widen
    /// (`fetch_min` / `fetch_max`). A second overflow hours later was then
    /// persisted as one range from the FIRST overflow's low end to the
    /// second one's top, pinning every acked segment written in between.
    /// The bounds are now reset after each bound.
    #[test]
    fn test_regression_a_second_overflow_after_a_persist_is_bounded_on_its_own() {
        let dir = scratch("overflow-twice");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let bucket = 1u64 << UNAPPLIED_BUCKET_SHIFT;
        let first_lo = seq(1_000);
        let first_hi = first_lo + (UNAPPLIED_BUCKETS as u64 + 2) * bucket;
        wm.note_unapplied_range(first_lo, first_hi);
        ack_both(&wm, first_hi + 5 * bucket);
        persist_durably(&wm, 1);
        assert!(!wm.snapshot().overflowed, "the first overflow is bounded");
        // Hours later, far above the first overflow, a second one.
        let second_lo = first_hi + 1_000 * bucket;
        let second_hi = second_lo + (UNAPPLIED_BUCKETS as u64 + 2) * bucket;
        wm.note_unapplied_range(second_lo, second_hi);
        ack_both(&wm, second_hi + 5 * bucket);
        persist_durably(&wm, 3 * LAG_NANOS);
        assert!(!wm.snapshot().overflowed, "the second overflow is bounded");
        let snap = AppliedSnapshot::load(&dir).expect("file");
        // O(1) EXEMPT: four fixed probes, test only
        for s in [first_lo, first_hi, second_lo, second_hi] {
            assert!(!snap.frame_is_applied(AppliedSink::Ticks, s), "{s} replays");
        }
        let between = first_hi + 500 * bucket;
        assert!(
            snap.range_is_applied(AppliedSink::Ticks, between, between + bucket),
            "an acked segment between the two overflows can be pruned"
        );
    }

    /// Round 4 (finding 2): an EMPTY segment (a process stopped during the
    /// catch-up drain leaves one; nothing removes it) or one whose first
    /// record is torn made every trim refuse and gave the fold no spans, so
    /// on a box whose backlog never drains the ranges only accumulated. The
    /// empty one is skipped; the torn one is bounded by its neighbours in
    /// write order, and the frames in it stay protected.
    #[test]
    fn test_regression_an_empty_or_torn_live_segment_no_longer_refuses_the_trim() {
        let dir = scratch("trim-empty-torn");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        segment(&dir, 100, 3);
        std::fs::write(dir.join(format!("ws-frames-{:020}.wal", seq(500))), b"").expect("w");
        let torn = dir.join(format!("ws-frames-{:020}.wal", seq(1_200)));
        std::fs::write(&torn, torn_then(seq(1_200))).expect("w");
        segment(&dir, 50_000, 3);
        wm.add_unapplied_range(seq(10), seq(20));
        wm.add_unapplied_range(seq(1_200), seq(1_200));
        let w = waiting_segments(&dir, true);
        assert!(!w.unknown, "the empty one is skipped, the torn one bounded");
        assert_eq!(w.lowest, Some(seq(100)));
        assert!(waiting_spans(&dir).is_some());
        assert!(trim_to_waiting(&wm, &dir).is_some(), "the trim runs");
        let snap = wm.snapshot();
        assert!(
            snap.range_has_unapplied(seq(1_200), seq(1_200)),
            "the torn segment's intact frame stays protected"
        );
        assert!(
            !snap.range_has_unapplied(seq(10), seq(20)),
            "a range below every waiting segment is clipped"
        );
        // A torn segment whose last intact record lies outside the bound its
        // neighbours give (a name out of write order) stays unbounded.
        let odd = dir.join(format!("ws-frames-{:020}.wal", seq(2_000)));
        std::fs::write(&odd, torn_then(seq(90_000))).expect("w");
        assert!(waiting_segments(&dir, true).unknown);
        assert_eq!(trim_to_waiting(&wm, &dir), None, "refused, kept");
        std::fs::remove_file(&odd).expect("rm");
        // A torn segment with nothing readable before it is unbounded too.
        std::fs::write(
            dir.join("ws-frames-00000000000000000007.wal"),
            torn_then(seq(50)),
        )
        .expect("w");
        assert!(waiting_segments(&dir, true).unknown);
        // A file holding only a torn record holds no frame: skipped.
        let mut lone = record(seq(3_000));
        let n = lone.len();
        lone[n - 1] ^= 0xFF;
        std::fs::remove_file(dir.join("ws-frames-00000000000000000007.wal")).expect("rm");
        std::fs::write(dir.join("ws-frames-00000000000000000008.wal"), lone).expect("w");
        assert!(!waiting_segments(&dir, true).unknown);
    }

    /// Review round 3 (second pass): the tail probe took an I/O error for
    /// "no intact record", so a segment that could not be read at a trim was
    /// skipped as empty, its neighbours' spans no longer covered it, and the
    /// trim clipped the range over its frames. A path the probe cannot read
    /// (here a directory named like a segment: it opens, and every read
    /// fails) now stays unbounded, so the trim refuses and the range stays.
    #[test]
    fn test_regression_a_segment_that_cannot_be_read_is_never_taken_for_empty() {
        let dir = scratch("trim-unreadable-io");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        segment(&dir, 100, 3);
        let odd = dir.join(format!("ws-frames-{:020}.wal", seq(500)));
        std::fs::create_dir_all(&odd).expect("dir");
        std::fs::write(odd.join("pad"), b"x").expect("w");
        assert!(
            std::fs::metadata(&odd).expect("meta").len() >= WAL_SMALLEST_RECORD_BYTES,
            "the directory is not taken for a short file"
        );
        assert!(
            last_intact_frame_seq(&odd).is_err(),
            "a read error is an error, not `no record`"
        );
        assert!(!holds_no_intact_record(&odd));
        segment(&dir, 50_000, 3);
        wm.add_unapplied_range(seq(500), seq(500));
        assert!(waiting_segments(&dir, true).unknown, "unbounded, not empty");
        assert_eq!(trim_to_waiting(&wm, &dir), None, "refused, kept");
        assert!(
            wm.snapshot().range_has_unapplied(seq(500), seq(500)),
            "the range over the unreadable segment stays"
        );
    }

    /// Round 4 (finding 4): the pinned set, as a count and bytes.
    #[test]
    fn test_pinned_by_ranges_counts_and_sizes_the_segments_a_range_intersects() {
        let dir = scratch("pinned");
        let a = segment(&dir, 100, 2);
        let b = segment(&dir, 5_000, 2);
        let s = REPLAY_REORDER_SLACK_SEQ;
        let segs = vec![
            (seq(100) - s, seq(101) + s, a.clone()),
            (seq(5_000) - s, u64::MAX, b),
        ];
        let mut ranges = UnappliedRanges::new();
        assert_eq!(pinned_by_ranges(&segs, &ranges), (0, 0));
        ranges.add_range(seq(100), seq(100));
        let len = std::fs::metadata(&a).expect("meta").len();
        assert_eq!(pinned_by_ranges(&segs, &ranges), (1, len));
        ranges.add_range(seq(9_000), seq(9_000));
        assert_eq!(pinned_by_ranges(&segs, &ranges).0, 2);
        // The trim publishes it (smoke: no panic without a recorder).
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        publish_pinned(&wm, &segs);
    }

    fn ack_both(wm: &AppliedWatermark, s: u64) {
        wm.note_ticks_acked(s);
        wm.note_depth_acked(s);
    }

    /// Records the last value set on each gauge, through a recorder scoped
    /// with `metrics::with_local_recorder` (never a global install, so
    /// parallel tests cannot see each other's values).
    #[derive(Clone, Default)]
    struct GaugeRecorder {
        values: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    }

    struct OneGauge {
        name: String,
        values: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, f64>>>,
    }

    impl metrics::GaugeFn for OneGauge {
        fn increment(&self, _: f64) {}
        fn decrement(&self, _: f64) {}
        fn set(&self, value: f64) {
            if let Ok(mut map) = self.values.lock() {
                map.insert(self.name.clone(), value);
            }
        }
    }

    impl metrics::Recorder for GaugeRecorder {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            metrics::Counter::noop()
        }
        fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::from_arc(std::sync::Arc::new(OneGauge {
                name: key.name().to_string(),
                values: std::sync::Arc::clone(&self.values),
            }))
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    impl GaugeRecorder {
        /// `(segments, bytes)` last published, `None` when not published.
        fn pinned(&self) -> Option<(f64, f64)> {
            let map = self.values.lock().ok()?;
            Some((
                *map.get(WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE)?,
                *map.get(WAL_UNAPPLIED_PINNED_BYTES_GAUGE)?,
            ))
        }

        fn clear(&self) {
            if let Ok(mut map) = self.values.lock() {
                map.clear();
            }
        }
    }

    /// PR W review round 4, pass 2: the pinned gauges were set only by a
    /// trim, and a trim runs only after a replayed batch is acknowledged. A
    /// box under the replay disk floor acknowledges none, so the gauges were
    /// never set there, and a clean box that drained nothing read the same.
    /// The boot fold and the backlog guard now publish them on every boot,
    /// and a scan that finds nothing waiting publishes 0.
    #[test]
    fn test_backlog_segments_counts_only_segments_a_range_pins() {
        let dir = scratch("backlog-count");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        assert_eq!(backlog_segments_with(&wm, &dir), Some(0), "nothing waiting");
        let _a = segment(&dir, 0, 3);
        let _b = segment(&dir, 10_000, 3);
        assert_eq!(
            backlog_segments_with(&wm, &dir),
            Some(0),
            "no range: no backlog"
        );
        assert!(wm.add_unapplied_range(seq(10_000), seq(10_002)));
        assert_eq!(backlog_segments_with(&wm, &dir), Some(1), "the pinned one");
        assert!(
            backlog_segments(&dir).is_some(),
            "the global form reads too"
        );
        let unlistable = scratch("backlog-unlistable");
        std::fs::write(
            unlistable.join(crate::ws_frame_spill::REPLAYING_SUBDIR),
            b"x",
        )
        .expect("a file where a folder belongs");
        assert_eq!(backlog_segments_with(&wm, &unlistable), None, "unknown");
    }

    #[test]
    fn test_fold_at_boot_and_guard_pending_backlog_publish_pinned_without_an_acked_batch() {
        let dir = scratch("pinned-boot");
        let a = segment(&dir, 0, 3);
        let b = segment(&dir, 10_000, 3);
        let bytes =
            std::fs::metadata(&a).expect("meta").len() + std::fs::metadata(&b).expect("meta").len();
        // An older binary's overflow: the fold migrates it over both.
        write_v1_overflow_file(&dir, seq(1_000));
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let recorder = GaugeRecorder::default();
        let fold = metrics::with_local_recorder(&recorder, || fold_at_boot(&wm, &dir));
        assert!(fold.expect("fold").migrated_overflow);
        assert_eq!(
            recorder.pinned(),
            Some((2.0, bytes as f64)),
            "the fold publishes"
        );
        // The catch-up acknowledged nothing (the replay refused at the disk
        // floor); the guard publishes again.
        recorder.clear();
        let _guarded = metrics::with_local_recorder(&recorder, || {
            guard_pending_backlog(&wm, &dir, seq(20_000))
        });
        assert_eq!(
            recorder.pinned(),
            Some((2.0, bytes as f64)),
            "the guard publishes"
        );
        // Nothing waiting: 0, not absent.
        let empty = scratch("pinned-boot-empty");
        let wm = AppliedWatermark::new();
        wm.bind(&empty);
        let recorder = GaugeRecorder::default();
        let _fold = metrics::with_local_recorder(&recorder, || fold_at_boot(&wm, &empty));
        assert_eq!(recorder.pinned(), Some((0.0, 0.0)), "nothing waiting");
        recorder.clear();
        assert_eq!(
            metrics::with_local_recorder(&recorder, || trim_to_waiting(&wm, &empty)),
            None
        );
        assert_eq!(recorder.pinned(), Some((0.0, 0.0)), "the trim too");
        // A folder that cannot be listed publishes nothing (unknown).
        let unlistable = scratch("pinned-boot-unlistable");
        std::fs::write(
            unlistable.join(crate::ws_frame_spill::REPLAYING_SUBDIR),
            b"x",
        )
        .expect("a file where a folder belongs");
        let wm = AppliedWatermark::new();
        wm.bind(&unlistable);
        let recorder = GaugeRecorder::default();
        let _guarded = metrics::with_local_recorder(&recorder, || {
            guard_pending_backlog(&wm, &unlistable, seq(20_000))
        });
        assert_eq!(recorder.pinned(), None, "unknown is not published");
    }

    /// A segment named `name` (its write order) holding `seqs`, in order.
    fn segment_named(dir: &Path, name: u64, seqs: &[u64]) -> PathBuf {
        let mut bytes = Vec::new();
        for &s in seqs {
            bytes.extend_from_slice(&record(s));
        }
        let path = dir.join(format!("ws-frames-{name:020}.wal"));
        std::fs::write(&path, bytes).expect("write segment");
        path
    }

    /// PR W review round 4, pass 2: a tiny segment A, then a long one X
    /// whose first record sits half a slack BELOW A's (minted earlier on
    /// another socket, written after A's), then Y. Sorted by first
    /// sequence, X came first: X's span ended at A's first record and A's
    /// was cut to its last record plus the slack (the gap to Y is wide), so
    /// X's middle lay in no span, the clip dropped the range over a frame
    /// shed there, and once acks passed X that frame read applied. In write
    /// order X's span runs to its own last record plus the slack.
    #[test]
    fn test_regression_trim_keeps_a_range_over_a_long_segment_written_after_a_tiny_one() {
        let dir = scratch("spans-inverted");
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let a = seq(1_000);
        let x = a - slack / 2;
        let shed = seq(1_300);
        segment_named(&dir, a, &[a]);
        segment_named(
            &dir,
            a + 1,
            &[x, seq(1_100), seq(1_200), shed, seq(1_400), seq(1_600)],
        );
        let y = seq(2_000);
        segment_named(&dir, y, &[y, seq(2_001)]);
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.add_unapplied_range(shed, shed);
        assert!(trim_to_waiting(&wm, &dir).is_some());
        ack_both(&wm, seq(3_000));
        let snap = wm.snapshot();
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, shed),
            "the shed frame in X's middle still replays (ranges {:?})",
            snap.ranges.as_slice()
        );
        assert!(
            !snap.segment_is_applied(x, y - 1),
            "X (judged by name order up to Y) is kept"
        );
        // The spans themselves, in write order.
        assert_eq!(
            waiting_spans(&dir),
            Some(vec![(a - slack, seq(1_600) + slack), (y - slack, u64::MAX)])
        );
    }

    /// Round 4, pass 2: write order when every name parses and the firsts
    /// fall by at most the slack; otherwise first-sequence order and no gap
    /// cut.
    #[test]
    fn test_order_by_name_sorts_by_write_order_and_refuses_an_unknown_order() {
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let p = |name: &str| PathBuf::from(format!("/w/ws-frames-{name}.wal"));
        let mut v = vec![(500, p("3")), (100 + slack, p("2")), (100, p("1"))];
        assert!(order_by_name(&mut v));
        assert_eq!(
            v.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![100, 100 + slack, 500]
        );
        // An inversion within the slack keeps write order.
        let mut v = vec![(1_000 + slack, p("1")), (1_000, p("2"))];
        assert!(order_by_name(&mut v));
        assert_eq!(v[0].0, 1_000 + slack);
        // More than the slack: unknown order.
        let mut v = vec![(1_001 + slack, p("1")), (1_000, p("2"))];
        assert!(!order_by_name(&mut v));
        assert_eq!(v[0].0, 1_000, "first-sequence order");
        // A name that does not parse.
        let mut v = vec![(9, PathBuf::from("/w/odd.wal")), (1, p("2"))];
        assert!(!order_by_name(&mut v));
        assert_eq!(v[0].0, 1);
    }

    /// PR W review round 6 (medium, a documented limit): the migrated
    /// backlog shrinks only on a boot whose catch-up batches are promoted,
    /// and flat pinned bytes cannot tell that apart from "growth stopped".
    /// The header must name the counters that can: the settle held and
    /// recovered counts, the confirmed-segment count and the catch-up's
    /// apply-lag pause count.
    #[test]
    fn test_module_header_names_the_drain_progress_counters() {
        let src = include_str!("wal_unapplied_ranges.rs");
        let header: String = src
            .lines()
            .take_while(|l| l.starts_with("//!") || l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        // O(1) EXEMPT: four fixed names, test only
        for name in [
            crate::wal_replay_settle::WAL_REPLAY_SETTLE_HELD_COUNTER,
            crate::wal_replay_settle::WAL_REPLAY_SETTLE_RECOVERED_COUNTER,
            "tv_wal_replay_confirmed_segments_total",
            "tv_wal_catchup_lag_pause_total",
        ] {
            assert!(header.contains(name), "the header names {name}");
        }
        assert!(header.contains("ZERO net progress"));
    }

    /// A capturing writer for the boot-report test.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// PR W review round 6 (low): the boot fold runs before the tracing
    /// subscriber exists, so its summary and the warning of a failed fold
    /// persist were discarded (and the failure latch then silenced the rest
    /// of the episode). The fold now records a report, written once after
    /// the subscriber is installed: the summary always, the coded warning
    /// when either persist was failing.
    #[test]
    fn test_log_boot_fold_summary_writes_the_recorded_fold_once_after_logging_starts() {
        let dir = scratch("boot-report");
        segment(&dir, 0, 2);
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let _fold = fold_at_boot(&wm, &dir);
        assert!(
            BOOT_FOLD_REPORT.get().is_some(),
            "a fold recorded its report"
        );
        assert!(log_boot_fold_summary(), "written once");
        assert!(!log_boot_fold_summary(), "never twice");

        let report = BootFoldReport {
            fold: BootFold::default(),
            pinned: Some((3, 4_096)),
            listing_failed: false,
            watermark_persist_failing: false,
            ranges_persist_failing: true,
            wal_dir: dir.clone(),
        };
        let out = Captured::default();
        let sink = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || write_boot_fold_report(&report));
        let healthy = BootFoldReport {
            ranges_persist_failing: false,
            ..report.clone()
        };
        let quiet = Captured::default();
        let sink = quiet.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || write_boot_fold_report(&healthy));
        let text = |c: &Captured| {
            String::from_utf8_lossy(
                &c.0.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
            .into_owned()
        };
        let failing = text(&out);
        assert!(
            failing.contains("WAL unapplied record folded at boot"),
            "{failing}"
        );
        assert!(failing.contains("pinned_segments=3"), "{failing}");
        assert!(
            failing.contains("unapplied_ranges_persist_failed_at_boot"),
            "{failing}"
        );
        assert!(failing.contains("WS-SPILL-01"), "{failing}");
        let healthy = text(&quiet);
        assert!(healthy.contains("WAL unapplied record folded at boot"));
        assert!(!healthy.contains("persist_failed_at_boot"), "{healthy}");
    }

    /// PR W review round 6 (low): falls that ADD UP. Three firsts, each 0.9
    /// slack below the one written before it, passed the neighbour check,
    /// but the third sits 1.8 slacks below the highest first so far, where
    /// [`scan_spans`] starts its span: its own first record was in no span.
    /// The order is now checked against the running maximum and refused.
    #[test]
    fn test_order_by_name_refuses_falls_that_add_up_past_the_slack() {
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let fall = slack / 10 * 9;
        let p = |name: &str| PathBuf::from(format!("/w/ws-frames-{name}.wal"));
        let top = 100 * slack;
        let mut v = vec![
            (top, p("1")),
            (top - fall, p("2")),
            (top - 2 * fall, p("3")),
        ];
        assert!(!order_by_name(&mut v), "1.8 slacks below the maximum");
        assert_eq!(v[0].0, top - 2 * fall, "first-sequence order");
        // One fall of 0.9 slack, then a rise: within the bound.
        let mut v = vec![(top, p("1")), (top - fall, p("2")), (top + slack, p("3"))];
        assert!(order_by_name(&mut v));
        assert!(firsts_within_slack_of_running_max(
            [top, top - slack].into_iter()
        ));
        assert!(!firsts_within_slack_of_running_max(
            [top, top - fall, top - 2 * fall].into_iter()
        ));
    }

    /// PR W review round 6 (low): the same falls on disk. With the neighbour
    /// check, the third segment's span started at the running maximum less
    /// the slack (above its own first record), so a range over that first
    /// record was clipped away by the trim. Now every waiting segment's
    /// first record stays inside the spans, and the range survives.
    #[test]
    fn test_regression_waiting_spans_cover_every_first_after_falls_that_add_up() {
        let dir = scratch("spans-cumulative-fall");
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let fall = slack / 10 * 9;
        let top = seq(1_000);
        let firsts = [top, top - fall, top - 2 * fall];
        // Names rise in write order while the firsts fall.
        for (i, &first) in firsts.iter().enumerate() {
            segment_named(&dir, top + i as u64, &[first]);
        }
        let spans = waiting_spans(&dir).expect("spans");
        for first in firsts {
            assert!(
                spans.iter().any(|&(lo, hi)| lo <= first && first <= hi),
                "first record {first} lies in a span ({spans:?})"
            );
        }
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let third = firsts[2];
        wm.add_unapplied_range(third, third);
        assert!(trim_to_waiting(&wm, &dir).is_some());
        assert!(
            wm.snapshot().ranges.intersects(third, third),
            "the range over the third segment's first record survives the trim"
        );
    }
}
