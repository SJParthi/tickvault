//! B1 — Feed-state chaos matrix: 7 permutations × (watchdog / ledger / durable floor).
//!
//! For EVERY feed-state permutation this file asserts, per `Feed::ALL` entry:
//!
//!   (a) WATCHDOG — the pure feed-agnostic stall decision
//!       (`should_restart_on_stall` + `StallRestartStorm` + the
//!       `FeedHealthRegistry` clock-injected liveness/verdict engine) fires
//!       exactly when it must and NEVER on a healthy feed;
//!   (b) CONSERVATION LEDGER — the pure daily-audit core (`compute_residuals`
//!       + the per-lane classifiers) reports residuals HONESTLY: `Balanced`
//!       only when the identity truly closes, `Leak`/`Partial` otherwise —
//!       never a false `Balanced`;
//!   (c) DURABLE FLOOR — every captured tick is recoverable across a simulated
//!       process death, through the feed's REAL recovery surface (WAL
//!       `replay_all` / per-day NDJSON count).
//!
//! FEED-AGNOSTIC CONTRACT: every permutation test iterates `for &feed in
//! Feed::ALL`. No feed-label string literal appears anywhere in this file
//! (ratcheted by the meta test below); the ONLY feed naming is inside
//! exhaustive `match feed { … }` arms where a capability genuinely differs
//! (durable-floor kind, per-lane ledger classifier) — so adding a new `Feed`
//! variant breaks compilation here and forces an explicit decision.
//!
//! HONEST ENVELOPE (documented per-arm below): the Groww lane has NO WAL; its
//! durable capture floor is the sidecar's fsync'd append-only NDJSON, and its
//! recovery is the bridge's re-tail-from-byte-0 (DEDUP-idempotent via the
//! deterministic `capture_seq`). Its "(c)" assertions therefore drive the real
//! pub per-day line-count surface the production conservation audit trusts —
//! they do NOT fake a WAL replay. The private 60s in-loop ledger fns in
//! `tick_processor.rs` are covered by their own in-crate unit tests; this
//! matrix drives the equivalent pub daily-audit layer.
//!
//! In-process only: no Docker, no network, no `#[ignore]`; unique temp dirs
//! per test (the `chaos_tmp` idiom from `chaos_ws_frame_spill_saturation.rs`),
//! cleaned up on exit. All decision fns take explicit `now_*` / `market_open`
//! parameters — no ambient clock.

#![cfg(test)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tickvault_app::groww_sidecar_supervisor::{
    FEED_STALL_RESTART_SECS, STALL_RESTART_STORM_MAX, STALL_RESTART_STORM_WINDOW_SECS,
    StallRestartStorm, should_restart_on_stall,
};
use tickvault_app::tick_conservation_boot::{
    OutcomeCounters, boot_covers_full_session, classify_groww_outcome, classify_outcome,
    compute_residuals, count_groww_ndjson_lines_for_ist_day,
};
use tickvault_common::constants::RESPONSE_CODE_TICKER;
use tickvault_common::feed::Feed;
use tickvault_common::feed_health::{FEED_STALE_TICK_SECS, FeedHealthRegistry, FeedHealthVerdict};
use tickvault_storage::tick_conservation_audit_persistence::ConservationOutcome;
use tickvault_storage::ws_frame_spill::{
    AppendOutcome, WsFrameSpill, WsType, confirm_replayed, count_frames_for_ist_day,
    ist_day_of_wall_nanos, next_frame_seq, replay_all,
};

/// Canonical IST-nanos test epoch (the `feed_health.rs` unit-test idiom).
const T0: i64 = 1_780_000_000_000_000_000;
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Fixed synthetic IST day number for the NDJSON durable-floor assertions
/// (`ts_ist_nanos / 86_400e9` day attribution — pure, no wall clock).
const NDJSON_TARGET_DAY: u64 = 20_600;

/// IST market open, seconds-of-day (the `boot_covers_full_session` boundary).
const MARKET_OPEN_SECS_OF_DAY_IST: u32 = 9 * 3600;

fn secs_nanos(s: u64) -> i64 {
    i64::try_from(s).expect("seconds fit in i64") * NANOS_PER_SEC
}

fn ndjson_day_base(day: u64) -> i64 {
    i64::try_from(day).expect("IST day fits in i64") * 86_400 * NANOS_PER_SEC
}

/// Unique per-test temp dir (pid + nanos keyed), pre-cleaned.
fn chaos_tmp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-feed-state-chaos-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create chaos temp dir");
    p
}

/// Poll `persisted_count()` until it reaches `target` or the budget expires.
fn wait_until_persisted_at_least(spill: &WsFrameSpill, target: u64, budget: Duration) -> u64 {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let count = spill.persisted_count();
        if count >= target {
            return count;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    spill.persisted_count()
}

/// A 16-byte synthetic live-feed ticker frame (response code 2 in byte 0 so
/// `classify_frame_for_day` counts it as a TICK class), carrying `i` in the
/// tail bytes so payload identity is assertable across replay.
fn ticker_frame(i: u64) -> Vec<u8> {
    let mut frame = vec![0u8; 16];
    frame[0] = RESPONSE_CODE_TICKER;
    frame[8..16].copy_from_slice(&i.to_le_bytes());
    frame
}

/// Same-price-advancing variant: constant price bytes (8..12), advancing
/// LTT-position bytes (12..16) — a flat market with a live clock.
fn flat_price_ticker_frame(i: u64) -> Vec<u8> {
    let mut frame = vec![0u8; 16];
    frame[0] = RESPONSE_CODE_TICKER;
    frame[8..12].copy_from_slice(&25_000u32.to_le_bytes()); // frozen price
    #[allow(clippy::cast_possible_truncation)] // test tick index ≪ u32::MAX
    frame[12..16].copy_from_slice(&(i as u32).to_le_bytes()); // advancing ts
    frame
}

/// Wall-nanos-derived frame_seq base guaranteed not to straddle an IST day
/// boundary across `n` consecutive seqs (deterministic day attribution).
fn same_day_seq_base(n: u64) -> u64 {
    let base = next_frame_seq();
    if ist_day_of_wall_nanos(base) == ist_day_of_wall_nanos(base.saturating_add(n)) {
        base
    } else {
        base.saturating_sub(2 * n)
    }
}

/// EXHAUSTIVE capability map: which durable capture floor each feed has.
/// A new `Feed` variant FAILS TO COMPILE here, forcing an explicit decision
/// about its durable floor + recovery story (the B1 task contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DurableFloor {
    /// Binary WS-frame WAL (`ws_frame_spill`) — replayed on boot via `replay_all`.
    Wal,
    /// Sidecar fsync'd append-only NDJSON. HONEST ENVELOPE: this feed has NO
    /// WAL; recovery is the bridge's re-tail from byte 0 (DEDUP-idempotent via
    /// deterministic `capture_seq`). The pub recovery-count surface is the
    /// per-day line count the production conservation audit itself trusts.
    Ndjson,
}

fn durable_floor(feed: Feed) -> DurableFloor {
    match feed {
        Feed::Dhan => DurableFloor::Wal,
        Feed::Groww => DurableFloor::Ndjson,
    }
}

/// The ONE place the sidecar NDJSON tick wire format exists in this file.
fn write_ndjson_line(f: &mut std::fs::File, ts: i64) {
    writeln!(f, "{{\"ts_ist_nanos\": {ts}}}").expect("ndjson write");
}

/// Write `n` synthetic NDJSON tick lines for `NDJSON_TARGET_DAY` and fsync.
/// `advance_ts=false` models frozen (duplicate) exchange timestamps.
/// Returns the STILL-OPEN producer handle so the caller controls the
/// process-death boundary explicitly (`drop(handle)` = the crash instant).
#[must_use]
fn write_ndjson_ticks(path: &Path, n: u64, advance_ts: bool) -> std::fs::File {
    let base = ndjson_day_base(NDJSON_TARGET_DAY);
    let mut f = std::fs::File::create(path).expect("ndjson create");
    for i in 0..n {
        let ts = if advance_ts {
            base + i64::try_from(i).expect("tick index fits in i64")
        } else {
            base
        };
        write_ndjson_line(&mut f, ts);
    }
    f.sync_all().expect("ndjson fsync");
    f
}

/// Capture `n` synthetic ticks onto `feed`'s durable floor in `dir` (WAL
/// payloads built by `wal_payload`), then simulate process death and recover
/// through the feed's REAL pub recovery surface. Returns
/// `(persisted_at_capture, recovered_after_reopen)` — two INDEPENDENT signals:
///   - `persisted_at_capture` = the per-day durable count taken while the
///     producer is still ALIVE (writer thread / open NDJSON handle held);
///   - `recovered_after_reopen` = the count handed back AFTER the death
///     boundary through a fresh path (`replay_all` post-drop / a recount from
///     a freshly copied NDJSON file — fresh inode, fresh open), so a cached
///     or still-held handle can never satisfy the recovery assertion.
fn capture_then_recover_with(
    feed: Feed,
    dir: &Path,
    n: u64,
    wal_payload: impl Fn(u64) -> Vec<u8>,
) -> (u64, u64) {
    match durable_floor(feed) {
        DurableFloor::Wal => {
            let base = same_day_seq_base(n);
            let day = ist_day_of_wall_nanos(base);
            let spill = WsFrameSpill::new(dir).expect("wal spill new");
            for i in 0..n {
                assert_eq!(
                    spill.append_with_seq(WsType::LiveFeed, wal_payload(i), base + i),
                    AppendOutcome::Spilled,
                    "healthy-ops contract: every frame must land in the WAL"
                );
            }
            assert_eq!(
                spill.drop_critical_count(),
                0,
                "drop_critical must stay zero — a non-zero value is a P0 durable-loss bug"
            );
            let persisted = wait_until_persisted_at_least(&spill, n, Duration::from_secs(15));
            assert_eq!(persisted, n, "writer must persist every enqueued frame");
            // Capture-time signal: read-only per-day scan while the writer is
            // still alive (documented safe against a live appender).
            let persisted_at_capture = count_frames_for_ist_day(dir, day).tick_frames;
            drop(spill); // process death: writer joined, segments fsynced

            let recovered_after_reopen = replay_all(dir).expect("replay after crash").len() as u64;
            (persisted_at_capture, recovered_after_reopen)
        }
        DurableFloor::Ndjson => {
            // No WAL on this feed (honest envelope — see DurableFloor::Ndjson).
            let path = dir.join("ticks.ndjson");
            let producer_handle = write_ndjson_ticks(&path, n, true);
            // Capture-time signal: producer handle still held (process alive).
            let persisted_at_capture =
                count_groww_ndjson_lines_for_ist_day(&path, NDJSON_TARGET_DAY);
            drop(producer_handle); // process-death boundary
            // Independent recovery signal: recount AFTER the death boundary from
            // a freshly copied file (fresh inode + fresh open) — proves the
            // on-disk BYTES carry the recovery, not any held handle or cache.
            let reopened = dir.join("ticks-reopened.ndjson");
            std::fs::copy(&path, &reopened).expect("durable bytes must survive the restart copy");
            let recovered_after_reopen =
                count_groww_ndjson_lines_for_ist_day(&reopened, NDJSON_TARGET_DAY);
            (persisted_at_capture, recovered_after_reopen)
        }
    }
}

fn capture_then_recover(feed: Feed, dir: &Path, n: u64) -> (u64, u64) {
    capture_then_recover_with(feed, dir, n, ticker_frame)
}

/// Fully-populated outcome counters where every processed tick persisted.
fn balanced_counters(n: u64) -> OutcomeCounters {
    OutcomeCounters {
        processed: Some(n),
        persisted: Some(n),
        junk: Some(0),
        stale_day: Some(0),
        outside_hours: Some(0),
        dedup: Some(0),
        parse_errors: Some(0),
        storage_errors: Some(0),
        dropped_total: Some(0),
    }
}

/// EXHAUSTIVE per-lane ledger verdict — the production daily audit is
/// lane-asymmetric BY DESIGN (WAL lane pages `Leak` on a positive residual;
/// the NDJSON lane records it as a diagnostic `Partial`, never a false
/// CRITICAL). The Groww delivery residual (`lines − persisted`) is computed
/// inline in the non-exported orchestrator; this derives the classifier INPUT
/// with the same one-line subtraction and drives the REAL pub classifier —
/// trivial arithmetic, not mirrored production logic. A new `Feed` variant
/// fails to compile here.
fn classify_ledger_for_feed(
    feed: Feed,
    delivered: u64,
    replay_recovered: u64,
    counters: &OutcomeCounters,
    partial_coverage: bool,
) -> ConservationOutcome {
    match feed {
        Feed::Dhan => {
            let (delivery, outcome) = compute_residuals(delivered, replay_recovered, counters);
            classify_outcome(delivery, outcome, partial_coverage)
        }
        Feed::Groww => {
            let persisted =
                i64::try_from(counters.persisted.unwrap_or(0)).expect("count fits in i64");
            let delivery = i64::try_from(delivered).expect("count fits in i64") - persisted;
            classify_groww_outcome(delivery, partial_coverage)
        }
    }
}

// ───────────────────────────── PERMUTATION 1 — silent feed ──────────────────

/// Dead socket: no ticks past the stall threshold during market hours.
/// (a) the pure watchdog decision FIRES (strict `>` boundary pinned);
/// (b) ledger `Balanced` for the pre-silence captured set;
/// (c) WAL/NDJSON recovers exactly the N pre-silence ticks.
#[test]
fn chaos_perm1_silent_feed_watchdog_fires_ledger_balanced_wal_recovers() {
    for &feed in Feed::ALL {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(feed, true);
        reg.record_tick(feed, T0);

        // Exactly-at-threshold must NOT fire (strict `>` — false-positive guard).
        let at = reg.last_tick_age_secs(feed, T0 + secs_nanos(FEED_STALL_RESTART_SECS));
        assert_eq!(at, Some(FEED_STALL_RESTART_SECS));
        assert!(
            !should_restart_on_stall(at, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: exactly-at-threshold age must NOT restart (strict >)",
            feed.as_str()
        );

        // One second past the threshold, market open, enabled → FIRES.
        let now = T0 + secs_nanos(FEED_STALL_RESTART_SECS + 1);
        let past = reg.last_tick_age_secs(feed, now);
        assert_eq!(past, Some(FEED_STALL_RESTART_SECS + 1));
        assert!(
            should_restart_on_stall(past, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: a dead socket past the stall threshold during market hours MUST restart",
            feed.as_str()
        );
        // A disabled feed never restarts, whatever the age.
        assert!(!should_restart_on_stall(
            past,
            true,
            false,
            FEED_STALL_RESTART_SECS
        ));
        // The verdict engine's OWN stale boundary, pinned INDEPENDENTLY of the
        // restart threshold via FEED_STALE_TICK_SECS (strict >): age == stale
        // threshold is still Ok; one second past is Down. Changing either
        // constant breaks exactly the assertions that reference it by name.
        let at_stale = T0 + secs_nanos(FEED_STALE_TICK_SECS);
        assert_eq!(
            reg.snapshot(feed, true, true, true, at_stale).verdict,
            FeedHealthVerdict::Ok,
            "feed {}: age == FEED_STALE_TICK_SECS is not yet stale (strict >)",
            feed.as_str()
        );
        assert_eq!(
            reg.snapshot(feed, true, true, true, at_stale + NANOS_PER_SEC)
                .verdict,
            FeedHealthVerdict::Down,
            "feed {}: age == FEED_STALE_TICK_SECS + 1 in-market must report Down",
            feed.as_str()
        );

        // (b) + (c): the N pre-silence ticks were captured; the crash loses none;
        // the identity closes → Balanced.
        let dir = chaos_tmp(&format!("perm1-{}", feed.as_str()));
        const N: u64 = 64;
        let (persisted_at_capture, recovered_after_reopen) = capture_then_recover(feed, &dir, N);
        assert_eq!(
            persisted_at_capture,
            N,
            "feed {}: durable per-day count",
            feed.as_str()
        );
        assert_eq!(
            recovered_after_reopen,
            N,
            "feed {}: crash recovery count",
            feed.as_str()
        );
        assert_eq!(
            classify_ledger_for_feed(feed, persisted_at_capture, 0, &balanced_counters(N), false),
            ConservationOutcome::Balanced,
            "feed {}: wal==processed==persisted must classify Balanced",
            feed.as_str()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ─────────────────── PERMUTATION 2 — frozen-timestamp duplicates ────────────

/// Same exchange timestamp repeated. Liveness is stamped at CAPTURE/parse time
/// (the registry's parse-time API), so duplicates keep the feed alive — the
/// watchdog must NOT fire. Every duplicate stays DISTINCT on the durable floor.
/// The ledger absorbs duplicates honestly per lane.
#[test]
fn chaos_perm2_frozen_timestamp_duplicates_watchdog_quiet_all_duplicates_durable() {
    const DUPES: u64 = 10;
    const UNIQUE: u64 = 1;
    for &feed in Feed::ALL {
        // (a) capture-time liveness advances even though the exchange ts is frozen.
        let reg = FeedHealthRegistry::new();
        for k in 0..DUPES {
            // parse-time liveness stamp — capture clock, not the frozen exchange ts
            reg.record_feed_liveness(feed, T0 + secs_nanos(k));
        }
        let age = reg.last_tick_age_secs(feed, T0 + secs_nanos(DUPES));
        assert_eq!(age, Some(1));
        assert!(
            !should_restart_on_stall(age, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: frozen exchange timestamps with live capture must NOT restart",
            feed.as_str()
        );

        // (c) every duplicate is its own durable record.
        let dir = chaos_tmp(&format!("perm2-{}", feed.as_str()));
        match durable_floor(feed) {
            DurableFloor::Wal => {
                let base = same_day_seq_base(DUPES);
                let payload = Bytes::from(ticker_frame(7)); // identical bytes per duplicate
                let spill = WsFrameSpill::new(&dir).expect("wal spill new");
                for i in 0..DUPES {
                    assert_eq!(
                        spill.append_with_seq(WsType::LiveFeed, payload.clone(), base + i),
                        AppendOutcome::Spilled
                    );
                }
                assert_eq!(
                    wait_until_persisted_at_least(&spill, DUPES, Duration::from_secs(15)),
                    DUPES
                );
                drop(spill);
                let frames = replay_all(&dir).expect("replay duplicates");
                assert_eq!(
                    frames.len() as u64,
                    DUPES,
                    "append_with_seq must keep EVERY duplicate distinct — replay count \
                     equals appended count, never the unique count"
                );
                for (i, f) in frames.iter().enumerate() {
                    assert_eq!(f.frame_seq, base + i as u64, "distinct seq per duplicate");
                    assert_eq!(f.frame.as_slice(), payload.as_ref(), "payload identity");
                }
            }
            DurableFloor::Ndjson => {
                // Identical-ts lines: each duplicate is its own durable line.
                let path = dir.join("ticks.ndjson");
                drop(write_ndjson_ticks(&path, DUPES, false)); // crash boundary
                assert_eq!(
                    count_groww_ndjson_lines_for_ist_day(&path, NDJSON_TARGET_DAY),
                    DUPES,
                    "the NDJSON floor must count every duplicate line"
                );
            }
        }

        // (b) ledger honesty per lane: dedup is a TERMINAL outcome, never a lie.
        let counters = OutcomeCounters {
            processed: Some(DUPES),
            persisted: Some(UNIQUE),
            junk: Some(0),
            stale_day: Some(0),
            outside_hours: Some(0),
            dedup: Some(DUPES - UNIQUE),
            parse_errors: Some(0),
            storage_errors: Some(0),
            dropped_total: Some(0),
        };
        match feed {
            Feed::Dhan => {
                let (delivery, outcome) = compute_residuals(DUPES, 0, &counters);
                assert_eq!((delivery, outcome), (0, 0));
                assert_eq!(
                    classify_outcome(delivery, outcome, false),
                    ConservationOutcome::Balanced,
                    "dedup is a counted outcome bucket — duplicates neither fake nor hide a leak"
                );
            }
            Feed::Groww => {
                // This lane's DB collapses identical capture_seq duplicates, so its
                // delivery residual (lines − persisted) is legitimately positive; the
                // production classifier records that as a DIAGNOSTIC Partial — never a
                // false Balanced, never a false CRITICAL Leak.
                let delivery = i64::try_from(DUPES - UNIQUE).expect("fits");
                let verdict = classify_groww_outcome(delivery, false);
                assert_eq!(verdict, ConservationOutcome::Partial);
                assert_ne!(
                    verdict,
                    ConservationOutcome::Balanced,
                    "a non-zero residual must never read Balanced"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ──────────────────── PERMUTATION 3 — same price, advancing ts ──────────────

/// Flat price with advancing timestamps: perfectly HEALTHY. This is the
/// false-positive guard pair for permutation 1 — the watchdog must stay quiet,
/// every frame is captured + replayed, and the ledger closes Balanced.
#[test]
fn chaos_perm3_same_price_advancing_healthy_no_false_positive() {
    const N: u64 = 20;
    for &feed in Feed::ALL {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(feed, true);
        for k in 0..N {
            reg.record_tick(feed, T0 + secs_nanos(k)); // advancing ts, flat price
        }
        let now = T0 + secs_nanos(N);
        let age = reg.last_tick_age_secs(feed, now);
        assert_eq!(age, Some(1));
        assert!(
            !should_restart_on_stall(age, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: a flat price with advancing timestamps is HEALTHY — no restart",
            feed.as_str()
        );
        assert_eq!(
            reg.snapshot(feed, true, true, true, now).verdict,
            FeedHealthVerdict::Ok,
            "feed {}: healthy flat market must report Ok",
            feed.as_str()
        );

        let dir = chaos_tmp(&format!("perm3-{}", feed.as_str()));
        let (persisted_at_capture, recovered_after_reopen) =
            capture_then_recover_with(feed, &dir, N, flat_price_ticker_frame);
        assert_eq!(persisted_at_capture, N);
        assert_eq!(
            recovered_after_reopen, N,
            "every flat-price frame must be recovered after the reopen"
        );
        assert_eq!(
            classify_ledger_for_feed(feed, persisted_at_capture, 0, &balanced_counters(N), false),
            ConservationOutcome::Balanced
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ───────────────────────────── PERMUTATION 4 — flooding ─────────────────────

/// Tick burst/storm. The watchdog must NOT fire (liveness is fresh); the
/// stall-restart STORM bound trips only past the in-window max; the durable
/// floor absorbs the whole burst with zero drops; and a processor-view loss
/// is reported HONESTLY (`Leak` / diagnostic `Partial`) — NEVER `Balanced`.
#[test]
fn chaos_perm4_flooding_burst_absorbed_storm_bounded_leak_reported_honestly() {
    const BURST: u64 = 20_000;
    for &feed in Feed::ALL {
        // (a) a flood keeps feed-level liveness fresh → no restart.
        let reg = FeedHealthRegistry::new();
        reg.record_ticks(feed, BURST, T0);
        let age = reg.last_tick_age_secs(feed, T0 + NANOS_PER_SEC);
        assert_eq!(age, Some(1));
        assert!(
            !should_restart_on_stall(age, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: a burst must never trip the stall watchdog",
            feed.as_str()
        );

        // (a') storm bound: repeated stall-RESTARTS inside the window trip the
        // escalation edge; a restart after window expiry resets the curve.
        let mut storm = StallRestartStorm::default();
        let start = STALL_RESTART_STORM_WINDOW_SECS + 1; // lands past the default window → fresh
        assert!(!storm.record_and_is_storm(start));
        assert_eq!(storm.count_in_window(), 1);
        for k in 1..u64::from(STALL_RESTART_STORM_MAX) {
            assert!(
                !storm.record_and_is_storm(start + k),
                "restarts up to the in-window max are bounded, not yet a storm"
            );
        }
        assert!(
            storm.record_and_is_storm(start + u64::from(STALL_RESTART_STORM_MAX)),
            "exceeding STALL_RESTART_STORM_MAX inside the window IS the escalation edge"
        );
        assert!(
            !storm.record_and_is_storm(start + STALL_RESTART_STORM_WINDOW_SECS + 999),
            "a restart after window expiry starts a fresh window"
        );
        assert_eq!(storm.count_in_window(), 1);

        // (a'') window-EDGE pins — production resets on `now − start > WINDOW`,
        // so Δ == WINDOW is still INSIDE the window (storm can trip) and
        // Δ == WINDOW + 1 starts a fresh window (no storm). Pin BOTH sides.
        let mut edge_in = StallRestartStorm::default();
        let w0 = STALL_RESTART_STORM_WINDOW_SECS + 1;
        assert!(!edge_in.record_and_is_storm(w0)); // fresh window, count = 1
        for k in 1..u64::from(STALL_RESTART_STORM_MAX) {
            assert!(!edge_in.record_and_is_storm(w0 + k)); // counts 2..=MAX
        }
        assert!(
            edge_in.record_and_is_storm(w0 + STALL_RESTART_STORM_WINDOW_SECS),
            "Δ == STALL_RESTART_STORM_WINDOW_SECS exactly is still INSIDE the \
             window — the storm MUST trip on this restart"
        );
        let mut edge_out = StallRestartStorm::default();
        assert!(!edge_out.record_and_is_storm(w0));
        for k in 1..u64::from(STALL_RESTART_STORM_MAX) {
            assert!(!edge_out.record_and_is_storm(w0 + k));
        }
        assert!(
            !edge_out.record_and_is_storm(w0 + STALL_RESTART_STORM_WINDOW_SECS + 1),
            "Δ == STALL_RESTART_STORM_WINDOW_SECS + 1 starts a FRESH window — no storm"
        );
        assert_eq!(edge_out.count_in_window(), 1);

        // (c) the durable floor absorbs the whole burst — zero drops, all recovered.
        let dir = chaos_tmp(&format!("perm4-{}", feed.as_str()));
        let (persisted_at_capture, recovered_after_reopen) =
            capture_then_recover(feed, &dir, BURST);
        assert_eq!(persisted_at_capture, BURST);
        assert_eq!(
            recovered_after_reopen, BURST,
            "the burst must replay in full"
        );

        // (b) HONESTY: `lost` ticks vanish from the processor view. The verdict
        // must surface the residual — NEVER a false Balanced.
        const LOST_FROM_PROCESSOR_VIEW: u64 = 25;
        let seen = BURST - LOST_FROM_PROCESSOR_VIEW;
        let verdict = classify_ledger_for_feed(feed, BURST, 0, &balanced_counters(seen), false);
        assert_ne!(
            verdict,
            ConservationOutcome::Balanced,
            "feed {}: a {LOST_FROM_PROCESSOR_VIEW}-tick processor-view loss must NEVER \
             classify Balanced",
            feed.as_str()
        );
        match feed {
            Feed::Dhan => assert_eq!(
                verdict,
                ConservationOutcome::Leak,
                "feed {}: a {LOST_FROM_PROCESSOR_VIEW}-tick processor-view loss must NEVER \
                 classify Balanced — the WAL lane pages it as Leak",
                feed.as_str()
            ),
            Feed::Groww => assert_eq!(
                verdict,
                ConservationOutcome::Partial,
                "feed {}: this lane records a positive residual as a diagnostic Partial — \
                 honest, never a false CRITICAL, never Balanced",
                feed.as_str()
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ───────────────────────── PERMUTATION 5 — partial universe ─────────────────

/// Only a subset of SIDs streams. FEED-level liveness stays fresh → the
/// watchdog must NOT fire (the documented illiquid-vs-dead contract). Recovery
/// returns exactly the captured subset — never the universe-sized expectation.
/// The ledger is Balanced for what ARRIVED (honesty = not claiming loss).
#[test]
fn chaos_perm5_partial_universe_feed_level_liveness_no_false_kill() {
    const UNIVERSE_SIDS: u64 = 10;
    const STREAMING_SIDS: u64 = 3;
    const TICKS_PER_STREAMING_SID: u64 = 8;
    const ARRIVED: u64 = STREAMING_SIDS * TICKS_PER_STREAMING_SID;
    for &feed in Feed::ALL {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(feed, true);
        reg.set_subscribed(feed, UNIVERSE_SIDS, 0);
        reg.record_ticks(feed, ARRIVED, T0);
        let now = T0 + secs_nanos(2);
        let age = reg.last_tick_age_secs(feed, now);
        assert_eq!(age, Some(2));
        assert!(
            !should_restart_on_stall(age, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: a fresh FEED-level tick must suppress the restart even when most \
             SIDs are quiet (illiquid-vs-dead contract)",
            feed.as_str()
        );
        assert_eq!(
            reg.snapshot(feed, true, true, true, now).verdict,
            FeedHealthVerdict::Ok
        );

        let dir = chaos_tmp(&format!("perm5-{}", feed.as_str()));
        let (persisted_at_capture, recovered_after_reopen) =
            capture_then_recover(feed, &dir, ARRIVED);
        // Recovery is judged against what ARRIVED (the captured subset of
        // STREAMING_SIDS × TICKS_PER_STREAMING_SID), NEVER against a
        // universe-sized (UNIVERSE_SIDS × per-SID) expectation.
        assert_eq!(persisted_at_capture, ARRIVED);
        assert_eq!(
            recovered_after_reopen, ARRIVED,
            "recovery must match what ARRIVED — the captured subset"
        );
        assert_eq!(
            classify_ledger_for_feed(
                feed,
                persisted_at_capture,
                0,
                &balanced_counters(ARRIVED),
                false
            ),
            ConservationOutcome::Balanced,
            "feed {}: Balanced for what arrived — honesty here means NOT claiming loss",
            feed.as_str()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ─────────────────────── PERMUTATION 6 — late resume at 09:15 ───────────────

/// Silent until market open, then resumes. Pre-open (`market_open=false`) the
/// watchdog must NOT fire regardless of age; an unknown (`None`) last-tick
/// never kills even in-market. After resume, liveness is fresh → no fire.
/// `boot_covers_full_session` boundary honesty pins the ledger's coverage flag.
#[test]
fn chaos_perm6_late_resume_at_market_open_never_fires_pre_open() {
    for &feed in Feed::ALL {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(feed, true);

        // Pre-open, no first tick ever: None age never kills, closed market never fires.
        let age = reg.last_tick_age_secs(feed, T0);
        assert_eq!(age, None);
        assert!(!should_restart_on_stall(
            age,
            false,
            true,
            FEED_STALL_RESTART_SECS
        ));
        assert!(
            !should_restart_on_stall(age, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: an unknown last-tick must NEVER kill, even in-market",
            feed.as_str()
        );
        assert!(
            !should_restart_on_stall(Some(86_400), false, true, FEED_STALL_RESTART_SECS),
            "feed {}: a closed market suppresses ANY age",
            feed.as_str()
        );
        // Market-closed snapshot is Ok (idle is normal) — never a pre-open page.
        assert_eq!(
            reg.snapshot(feed, true, true, false, T0).verdict,
            FeedHealthVerdict::Ok
        );

        // Resume at open: fresh tick → no fire, verdict Ok.
        reg.record_tick(feed, T0);
        let now = T0 + NANOS_PER_SEC;
        let age = reg.last_tick_age_secs(feed, now);
        assert_eq!(age, Some(1));
        assert!(!should_restart_on_stall(
            age,
            true,
            true,
            FEED_STALL_RESTART_SECS
        ));
        assert_eq!(
            reg.snapshot(feed, true, true, true, now).verdict,
            FeedHealthVerdict::Ok
        );

        // Ledger coverage-flag boundary honesty (09:00:00 IST is exclusive).
        assert!(boot_covers_full_session(MARKET_OPEN_SECS_OF_DAY_IST - 1));
        assert!(!boot_covers_full_session(MARKET_OPEN_SECS_OF_DAY_IST));
        assert!(boot_covers_full_session(8 * 3600 + 1800)); // 08:30 boot covers

        // Frames from resume onward are all captured + replayed; a full-session
        // boot with a closed identity → Balanced; a mid-session boot cannot
        // vouch → Partial even with ZERO residuals (never a false Balanced).
        let dir = chaos_tmp(&format!("perm6-{}", feed.as_str()));
        const N: u64 = 32;
        let (persisted_at_capture, recovered_after_reopen) = capture_then_recover(feed, &dir, N);
        assert_eq!(persisted_at_capture, N);
        assert_eq!(recovered_after_reopen, N);
        assert_eq!(
            classify_ledger_for_feed(feed, persisted_at_capture, 0, &balanced_counters(N), false),
            ConservationOutcome::Balanced
        );
        assert_eq!(
            classify_ledger_for_feed(feed, persisted_at_capture, 0, &balanced_counters(N), true),
            ConservationOutcome::Partial,
            "feed {}: partial coverage must win over zero residuals",
            feed.as_str()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ──────────────────── PERMUTATION 7 — process down mid-market ───────────────

/// Simulated crash: drop the WAL writer mid-stream and re-open on the SAME
/// dir. `replay_all` recovers every fsynced frame; un-confirmed segments
/// re-replay (replay-then-confirm idempotency); after `confirm_replayed` they
/// never re-replay. A mid-market boot classifies `Partial`, never a false
/// `Balanced`. On restart, a `None` last-tick age never false-kills at boot.
#[test]
fn chaos_perm7_process_down_mid_market_replay_recovers_partial_never_balanced() {
    const MID_MARKET_BOOT_SECS: u32 = 11 * 3600 + 30 * 60; // 11:30 IST restart
    for &feed in Feed::ALL {
        // (a) watchdog on restart: fresh registry, no tick yet → NEVER a false kill.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(feed, true);
        assert_eq!(reg.last_tick_age_secs(feed, T0), None);
        assert!(
            !should_restart_on_stall(None, true, true, FEED_STALL_RESTART_SECS),
            "feed {}: a just-restarted feed with no first tick must not be killed",
            feed.as_str()
        );
        // Honest visibility instead of a kill: connected-but-no-ticks-yet = Down.
        assert_eq!(
            reg.snapshot(feed, true, true, true, T0).verdict,
            FeedHealthVerdict::Down
        );

        // (b) ledger: a mid-market boot cannot vouch for the session → Partial,
        // NEVER a false Balanced — even with zero residuals.
        assert!(!boot_covers_full_session(MID_MARKET_BOOT_SECS));
        let partial_coverage = !boot_covers_full_session(MID_MARKET_BOOT_SECS);
        const SEEN: u64 = 100;
        let verdict =
            classify_ledger_for_feed(feed, SEEN, 0, &balanced_counters(SEEN), partial_coverage);
        assert_eq!(verdict, ConservationOutcome::Partial);
        assert_ne!(
            verdict,
            ConservationOutcome::Balanced,
            "feed {}: partial coverage must never read Balanced",
            feed.as_str()
        );

        // (c) durable floor across the crash boundary.
        let dir = chaos_tmp(&format!("perm7-{}", feed.as_str()));
        const N: u64 = 48;
        const M: u64 = 16;
        match durable_floor(feed) {
            DurableFloor::Wal => {
                let base = same_day_seq_base(N);
                let spill = WsFrameSpill::new(&dir).expect("wal spill new");
                for i in 0..N {
                    assert_eq!(
                        spill.append_with_seq(WsType::LiveFeed, ticker_frame(i), base + i),
                        AppendOutcome::Spilled
                    );
                }
                assert_eq!(spill.drop_critical_count(), 0);
                assert_eq!(
                    wait_until_persisted_at_least(&spill, N, Duration::from_secs(15)),
                    N
                );
                drop(spill); // process death: writer joined, segments fsynced on disk

                // Restart #1: replay recovers every fsynced frame, in order, with
                // seq identity across the crash boundary.
                let frames = replay_all(&dir).expect("replay #1");
                assert_eq!(frames.len() as u64, N);
                for (i, f) in frames.iter().enumerate() {
                    assert_eq!(f.frame_seq, base + i as u64, "FIFO + seq identity");
                }
                // Crash BETWEEN replay and confirm: un-confirmed segments MUST
                // re-replay (the replay-then-confirm idempotency window).
                let again = replay_all(&dir).expect("replay #2 (unconfirmed)");
                assert_eq!(
                    again.len() as u64,
                    N,
                    "un-confirmed segments must re-replay after a crash between \
                     replay and confirm (DEDUP-idempotent downstream)"
                );
                // Confirm → archived → never re-replayed.
                confirm_replayed(&dir);
                assert_eq!(replay_all(&dir).expect("replay #3").len(), 0);

                // Restart #2 on the SAME dir: fresh capture recovers independently.
                let base2 = same_day_seq_base(M);
                let spill2 = WsFrameSpill::new(&dir).expect("wal spill reopen");
                for i in 0..M {
                    assert_eq!(
                        spill2.append_with_seq(WsType::LiveFeed, ticker_frame(i), base2 + i),
                        AppendOutcome::Spilled
                    );
                }
                assert_eq!(
                    wait_until_persisted_at_least(&spill2, M, Duration::from_secs(15)),
                    M
                );
                drop(spill2);
                assert_eq!(
                    replay_all(&dir).expect("replay #4").len() as u64,
                    M,
                    "frames captured after the re-open must recover on the next boot"
                );
            }
            DurableFloor::Ndjson => {
                // HONEST ENVELOPE: no WAL on this feed. Process death cannot lose
                // fsync'd NDJSON lines; production recovery is the bridge's re-tail
                // from byte 0, DEDUP-idempotent via deterministic capture_seq. The
                // assertable pub surface is the per-day count the conservation
                // audit trusts: exact + stable across process-death re-reads, and
                // append-only across a re-open (the restart appends, never rewrites).
                let path = dir.join("ticks.ndjson");
                drop(write_ndjson_ticks(&path, N, true)); // process-death boundary
                let first = count_groww_ndjson_lines_for_ist_day(&path, NDJSON_TARGET_DAY);
                let second = count_groww_ndjson_lines_for_ist_day(&path, NDJSON_TARGET_DAY);
                assert_eq!(first, N);
                assert_eq!(second, N, "re-tail of the durable floor is idempotent");
                // Restart: append M more lines to the SAME file (append mode, as the
                // sidecar does), then the per-day count reflects N + M exactly.
                {
                    let base_ts = ndjson_day_base(NDJSON_TARGET_DAY) + secs_nanos(1);
                    let mut f = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .expect("ndjson re-open append");
                    for i in 0..M {
                        let ts = base_ts + i64::try_from(i).expect("fits");
                        write_ndjson_line(&mut f, ts);
                    }
                    f.sync_all().expect("ndjson fsync");
                }
                assert_eq!(
                    count_groww_ndjson_lines_for_ist_day(&path, NDJSON_TARGET_DAY),
                    N + M,
                    "post-restart appends must add to the durable floor, losing nothing"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ───────────────────────────── META-RATCHET ─────────────────────────────────

/// Matrix-completeness ratchet, ZERO-SLACK: (1) all 7 permutation tests exist
/// by exact name; (2) EACH permutation's OWN BODY (the source region from its
/// `fn` up to the next `#[test]`) contains the `Feed::ALL` iteration exactly
/// once — a global count would let a permutation be silently de-generalized to
/// a single-feed array while other occurrences keep the total high; (3) ZERO
/// quoted feed-label string literals appear in this file (the banned patterns
/// are built at runtime from `Feed::ALL` so this meta test cannot self-match).
/// Deleting/renaming a permutation, de-generalizing one, or hardcoding a feed
/// label fails the build.
#[test]
fn chaos_matrix_completeness_ratchet_all_7_permutations_iterate_feed_all() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/chaos_feed_state_matrix.rs"
    );
    let src = std::fs::read_to_string(path).expect("read own source");

    const PERMUTATION_TESTS: [&str; 7] = [
        "chaos_perm1_silent_feed_watchdog_fires_ledger_balanced_wal_recovers",
        "chaos_perm2_frozen_timestamp_duplicates_watchdog_quiet_all_duplicates_durable",
        "chaos_perm3_same_price_advancing_healthy_no_false_positive",
        "chaos_perm4_flooding_burst_absorbed_storm_bounded_leak_reported_honestly",
        "chaos_perm5_partial_universe_feed_level_liveness_no_false_kill",
        "chaos_perm6_late_resume_at_market_open_never_fires_pre_open",
        "chaos_perm7_process_down_mid_market_replay_recovers_partial_never_balanced",
    ];
    // Built at runtime so the pattern never self-matches in this meta test's
    // own body (which is deliberately NOT one of the scanned regions anyway).
    let feed_all_iter = format!("for &feed in {}::ALL", "Feed");
    for name in PERMUTATION_TESTS {
        let fn_marker = format!("fn {name}()");
        let start = src.find(&fn_marker).unwrap_or_else(|| {
            panic!(
                "matrix incomplete: permutation test `{name}` is missing — every \
                 feed-state permutation must stay covered"
            )
        });
        // Body region = from this fn to the next `#[test]` (or EOF for the last).
        let rest = &src[start..];
        let end = rest[fn_marker.len()..]
            .find("#[test]")
            .map_or(rest.len(), |i| i + fn_marker.len());
        let body = &rest[..end];
        let loops_in_body = body.matches(&feed_all_iter).count();
        assert_eq!(
            loops_in_body, 1,
            "zero-slack ratchet: permutation `{name}` must iterate Feed::ALL \
             exactly once in its own body (found {loops_in_body}) — a \
             de-generalized single-feed loop or a duplicated loop both break \
             the feed-agnostic contract"
        );
    }

    for &feed in Feed::ALL {
        let banned = format!("\"{}\"", feed.as_str());
        assert!(
            !src.contains(&banned),
            "feed-agnosticism broken: quoted feed-label literal {banned} found in \
             this file — use Feed::ALL / feed.as_str(); identifiers in exhaustive \
             `match feed` arms are the only allowed feed naming"
        );
    }
}

/// Source-scan pin for the ONE piece of arithmetic this file derives instead
/// of calling: the Groww lane's delivery residual. The production derivation
/// lives INLINE in the non-exported `run_groww_tick_conservation_audit`
/// (crates/app/src/tick_conservation_boot.rs) as
/// `ndjson_lines − db_rows` feeding `classify_groww_outcome(delivery_residual,
/// partial_coverage)`. `classify_ledger_for_feed` above derives the classifier
/// INPUT with the same subtraction — this pin makes any drift in the
/// production expression a build failure, so the test arithmetic can never
/// silently diverge from the orchestrator it models. Whitespace-normalized so
/// rustfmt line-wrapping cannot false-fail it. No src file is modified.
#[test]
fn groww_residual_derivation_source_pin_matches_test_arithmetic() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tick_conservation_boot.rs");
    let src = std::fs::read_to_string(path).expect("read tick_conservation_boot.rs");
    let collapsed = src.split_whitespace().collect::<Vec<_>>().join(" ");

    let pinned_residual = "let delivery_residual = i64::try_from(ndjson_lines) \
                           .unwrap_or(i64::MAX) \
                           .saturating_sub(groww_db_rows.max(0));";
    let pinned_collapsed = pinned_residual
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        collapsed.contains(&pinned_collapsed),
        "the Groww orchestrator's delivery-residual derivation \
         (`ndjson_lines − db_rows`) has drifted from the expression \
         `classify_ledger_for_feed` in this file derives — update BOTH in the \
         same commit (the chaos matrix models the orchestrator's classifier \
         input; drift here makes the matrix assert the wrong contract)"
    );
    assert!(
        collapsed.contains("classify_groww_outcome(delivery_residual, partial_coverage)"),
        "the Groww orchestrator no longer feeds its delivery residual into \
         classify_groww_outcome(delivery_residual, partial_coverage) — the \
         matrix's ledger-honesty assertions model exactly that call"
    );
}
