//! RAM residency stores — boot install + chain-day rehydrate + stats task
//! (PR-2 of the data-completeness build; RAMSTORE-01 runbook:
//! `.claude/rules/project/ram-store-error-codes.md`).
//!
//! Operator directive 2026-07-16 (verbatim): *"how can i believe you that
//! you have all these already available in our in-memory app RAM —
//! especially for the current day and even in the future last one month
//! data should be entirely in memory app RAM, especially for trading
//! decisions of entry and exit"* — refined by *"for only spots we will
//! have minimum one month data because anyhow based on underlying spots
//! alone only trading decision will be entered or exited — but option only
//! for the current day"* and *"everything should be always available in
//! our own questdb right — our entire one month should be stored and
//! fetched from questdb even before premarket"*.
//!
//! Three responsibilities, all cold-path:
//! 1. **Install** ([`install_market_ram_stores`]): the process-global
//!    month-deep `SpotBarStore` (trading crate) + current-day
//!    `ChainDayStore` (core pipeline), gated on `[market_ram_store]`.
//!    Installed BEFORE the fold task spawns so PR-1's boot catch-up
//!    populates the spot rings — pre-market spot rehydration IS the
//!    existing catch-up (zero new spot QuestDB reads).
//! 2. **Chain rehydrate** ([`spawn_chain_day_rehydrate`]): a ONE-SHOT
//!    bounded read of TODAY's `option_chain_1m` rows per (feed,
//!    underlying, 30-minute session window) — hardened `/exec` shapes
//!    (micros WHERE window, nanos projection, explicit LIMIT tripwire,
//!    streamed 8 MiB cap, redirect-none client) — rebuilt into
//!    `ChainMoneynessSnapshot`s and recorded via `record_rehydrated`
//!    (NEVER overwriting live-published minutes). A mid-session restart
//!    gets the morning's chain history back.
//! 3. **Stats/heartbeat** ([`spawn_ram_store_stats_task`]): a supervised
//!    60 s loop publishing the depth gauges the operator's "is the month
//!    actually in RAM?" question reads — honest fill level, never a
//!    fabricated month (audit Rule 11).
//!
//! Every degrade is a coded RAMSTORE-01 `error!`/`warn!` (the boot/
//! rehydrate/task degrades here are `error!`; the chain store's own
//! row-cap / day-drop / minute-cap degrades are `warn!` — PR-2 round-1
//! doc alignment). Log-sink-only delivery boundary per the runbook —
//! QuestDB remains the durable truth; a RAM degrade re-fills at the next
//! boot.

use std::time::Duration;

use metrics::{counter, gauge};
use tickvault_common::config::MarketRamStoreConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_core::pipeline::chain_day_store::{chain_day_store, install_chain_day_store};
use tickvault_trading::in_mem::spot_bar_store::{
    MAX_SPOT_BAR_SLOTS, estimated_capacity_bytes, install_spot_bar_store, spot_bar_store,
};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Constants (all named — cold-path envelope bounds)
// ---------------------------------------------------------------------------

/// Stats/heartbeat cadence (the house 60 s stats-task cadence).
pub const RAM_STORE_STATS_INTERVAL_SECS: u64 = 60;

/// Backoff before respawning a dead stats task (house respawn pattern).
pub const RAM_STORE_STATS_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Chain rehydrate window width — 30 minutes per bounded `/exec` read so
/// one response stays well inside the 8 MiB streamed cap even at the
/// row-cap worst case.
pub const RAM_CHAIN_REHYDRATE_WINDOW_MINUTES: usize = 30;

/// Session windows per day: 14 × 30 min covers [09:00, 16:00) IST — the
/// 400-minute candle session plus the legs' boundary-fire margin.
///
/// **2026-08-28: 13 -> 14, and it had to move WITH the anchor above.** The
/// old pair (09:15 + 13 windows) covered [09:15, 15:45). Moving only the
/// anchor to 09:00 would have covered [09:00, 15:30) — trading one silent
/// hole at the START of the day for a new one at the END, dropping the
/// 15:30-15:40 closing-auction window that the 2026-08-03 NSE CAS change
/// added. Anchor and count are a pair; neither moves alone.
///
/// 400 min / 30 = 13.33, so 14 windows are the ceiling, and the 14th runs
/// past the close into 16:00 — harmless, since a window with no rows reads
/// nothing.
pub const RAM_CHAIN_REHYDRATE_WINDOW_COUNT: usize = 14;

/// Ceiling on the spot store's PROJECTED ring capacity, in bytes — the
/// FALLBACK, used only when the host's memory cannot be read.
///
/// The spot rings are `VecDeque::with_capacity(bars_per_day × spot_days)`,
/// allocated **eagerly when a slot is created** — so this memory is committed
/// the moment an instrument is first seen, whether or not a single bar ever
/// fills it. Capacity is therefore a promise the process makes up front, not
/// a high-water mark it grows into, and it is the right thing to bound.
///
/// 10 GiB is the operator's stated current-day RAM budget (2026-08-12,
/// restated three times), sized against the r8g.xlarge 32 GiB host.
///
/// **It is a FALLBACK rather than the budget itself** (2026-08-21). A budget
/// pinned to one machine is the same shape the frame ring was repaired for:
/// it is correct only while the host never changes, and it is silently wrong
/// the moment it does — too generous on a smaller box, needlessly tight on a
/// larger one, with no signal either way. [`ram_store_spot_capacity_budget_bytes`]
/// derives it from the host at runtime and falls back to this value only when
/// the host cannot be read, loudly.
pub const RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The spot store's share of host memory, as an exact fraction: 5/16.
///
/// Chosen so the reference host reproduces the operator's stated figure
/// EXACTLY rather than approximately — 5/16 × 32 GiB = 10 GiB to the byte.
/// A percentage would have been 31.25%, and rounding it to 31% would have
/// quietly tightened the live budget by 80 MiB while appearing to preserve
/// it. The fraction is the honest way to say "same as today, but derived".
///
/// The remaining 11/16 is not slack: it is QuestDB (`QDB_MEM_LIMIT` default
/// 12g), the aggregator's ~155 MB, the seal and frame rings, and the OS.
pub const RAM_STORE_SPOT_BUDGET_NUMERATOR: u64 = 5;
/// Denominator of [`RAM_STORE_SPOT_BUDGET_NUMERATOR`]'s fraction.
pub const RAM_STORE_SPOT_BUDGET_DENOMINATOR: u64 = 16;

/// Above this, a cgroup limit is "unlimited" rather than a real bound.
///
/// cgroup v1 reports no-limit as a saturated `u64` near `i64::MAX`, and v2
/// uses the literal `max`. 1 PiB is far above any real host and far below
/// the saturated sentinel, so it separates the two without pattern-matching
/// on a specific kernel's choice of sentinel.
const CGROUP_UNLIMITED_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024 * 1024 * 1024;

// `parse_meminfo_total_bytes` REMOVED 2026-09-03. It was a SECOND, local
// parser for `/proc/meminfo`, and `resolve_memory_ceiling` -- which this file
// now calls -- already reads and parses that file itself. Two hand-rolled
// readers of the same kernel file, disagreeing about which files to consult at
// all, is precisely how this site came to size its budget against the whole
// machine while the unit permitted 20 GiB. Its parsing tests go with it; the
// equivalent cases live in `resource_monitor`'s own suite, beside the parser
// that survived.

/// Parses a cgroup memory limit (v1 `memory.limit_in_bytes`, v2
/// `memory.max`), returning `None` for "unlimited" in either dialect.
fn parse_cgroup_limit_bytes(contents: &str) -> Option<u64> {
    let trimmed = contents.trim();
    if trimmed.is_empty() || trimmed == "max" {
        return None;
    }
    let value: u64 = trimmed.parse().ok()?;
    if value >= CGROUP_UNLIMITED_THRESHOLD_BYTES {
        return None;
    }
    Some(value)
}

/// The spot budget for a given host memory figure.
///
/// Saturating rather than wrapping: a nonsensical input yields a clamped
/// number instead of a tiny one that would silently pass the projection
/// check it exists to fail.
fn budget_from_host_bytes(host_bytes: u64) -> u64 {
    host_bytes
        .saturating_mul(RAM_STORE_SPOT_BUDGET_NUMERATOR)
        .saturating_div(RAM_STORE_SPOT_BUDGET_DENOMINATOR)
}

/// The memory this process may actually use, in bytes.
///
/// Takes the MINIMUM of the machine's RAM and any cgroup limit, because both
/// bind and the smaller one is what the OOM killer enforces. Checking the
/// cgroup is what makes this the same code on the AWS box (no container
/// limit) and in a Docker dev run (limit set) — the common-runtime property
/// a hardcoded constant cannot have.
fn host_memory_limit_bytes() -> Option<u64> {
    // ⚠ CORRECTED TWICE on 2026-09-03, and the FIRST correction was INERT.
    //
    // It began by reading `/sys/fs/cgroup/memory.max` -- the ROOT cgroup. This
    // process lives in a systemd slice, so that file does not exist for it,
    // the read failed, the limit read as ABSENT, and the budget was sized
    // against the machine's whole 30.75 GiB while the unit permits 20.
    //
    // The first repair swapped in `resolve_cgroup_memory_max_path()`, which
    // finds the real slice. It changed NOTHING, because of what that file
    // contains: the unit sets `MemoryHigh=20G` and deliberately no
    // `MemoryMax=`, so the slice's `memory.max` reads the literal `"max"`,
    // `parse_cgroup_limit_bytes` correctly returns `None` for that, and the
    // function fell through to machine RAM exactly as before. A fix that reads
    // the right file and still gets the wrong answer -- and the guard written
    // beside it PASSED, because it only checked that the root path literal was
    // gone.
    //
    // `resolve_memory_ceiling` is the function that already knows the answer:
    // it reads `memory.high` beside `memory.max` and prefers, in order, the
    // hard limit, the throttle, then the machine total. `ws_frame_spill` was
    // repaired with it hours earlier; this site got only half of that, which
    // is why both now call the SAME function instead of two hand-rolled
    // approximations of it.
    //
    // The cgroup-v1 file is kept as a separate check: `resolve_memory_ceiling`
    // is v2-shaped, and a v1 host still answers on the older path.
    let ceiling = tickvault_storage::resource_monitor::resolve_memory_ceiling(
        &tickvault_storage::resource_monitor::resolve_cgroup_memory_max_path(),
        std::path::Path::new("/proc/meminfo"),
    )
    .bytes();

    let v1 = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        .ok()
        .as_deref()
        .and_then(parse_cgroup_limit_bytes);

    // MIN preserved from the original: where two ceilings both bind, the
    // smaller is what the kernel enforces.
    match (ceiling, v1) {
        (Some(m), Some(c)) => Some(m.min(c)),
        (Some(m), None) => Some(m),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

/// The spot store's RAM budget, derived from the host once per process.
///
/// Resolved on first call and cached, so every later call is O(1) and the
/// value can never change mid-process — a budget that drifted between the
/// projection check and the log line would make the two disagree.
///
/// Falls back to [`RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES`] with a
/// coded `warn!` when the host cannot be read. That path is loud on purpose:
/// silently reverting to a number sized for one specific machine is exactly
/// the failure this function exists to remove.
pub fn ram_store_spot_capacity_budget_bytes() -> u64 {
    static BUDGET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| match host_memory_limit_bytes() {
        Some(host_bytes) => budget_from_host_bytes(host_bytes),
        None => {
            warn!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "capacity_budget",
                fallback_bytes = RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES,
                "RAMSTORE-01: host memory could not be read — the spot RAM \
                 budget falls back to the r8g.xlarge-sized figure. On a \
                 SMALLER host that budget is too generous and the projection \
                 check will under-report; verify the host size by hand"
            );
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES
        }
    })
}

/// The spot store's own slot ceiling, as the `u32` the capacity estimator
/// takes. Same 25,000 the aggregator, indicator engine and day-OHLC tracker
/// are sized to — projecting at anything smaller is what hid the overshoot.
pub const MAX_SPOT_BAR_SLOTS_U32: u32 = MAX_SPOT_BAR_SLOTS as u32;

/// A small illustrative slot count, reported ALONGSIDE the ceiling so the
/// gap between today's 4-index universe and the 25,000 target is visible in
/// one line rather than inferred. Never the only figure logged — reporting
/// this alone is precisely the bug being fixed.
pub const RAM_STORE_SAMPLE_SLOT_COUNT: u32 = 8;

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Installs BOTH process-global stores (first-wins). Called from the boot
/// path BEFORE the fold task spawns (`ram_store_wiring_guard` pins the
/// order) so the catch-up's seals land in the spot rings.
// TEST-EXEMPT: process-global OnceLock installs — pinned by the store crates' first-wins tests + ram_store_wiring_guard.
pub fn install_market_ram_stores(cfg: &MarketRamStoreConfig, catchup_days: u32) {
    let spot_ok = install_spot_bar_store(cfg.spot_days);
    let chain_ok = install_chain_day_store(cfg.chain_row_cap as usize);
    if !spot_ok || !chain_ok {
        // Defensive first-wins refusal — a duplicate install means a second
        // boot-path call in one process (loud, never silent).
        error!(
            code = ErrorCode::RamStore01Degraded.code_str(),
            stage = "install",
            spot_ok,
            chain_ok,
            "RAMSTORE-01: RAM store install refused — already installed \
             (first-wins; the first installation keeps serving)"
        );
        return;
    }
    if cfg.spot_days < catchup_days {
        warn!(
            spot_days = cfg.spot_days,
            catchup_days,
            "market_ram_store: spot_days is SHALLOWER than the fold catch-up \
             window — the rings evict the oldest catch-up days (harmless, but \
             RAM depth < the folded history; raise [market_ram_store] spot_days \
             to keep the whole window resident)"
        );
    }
    // The projected capacity at the store's OWN slot ceiling, not at a
    // sample size.
    //
    // This line used to pass a hardcoded `8` for slot_count, and that single
    // literal is why a 34.9 GB configuration read as harmless for months.
    // Eight slots is roughly today's universe (the 4 SPOT_1M_REST_INDICES),
    // so at `spot_days = 35` the log printed ~11 MB and every reader
    // reasonably concluded the store was cheap. The store's real ceiling is
    // `MAX_SPOT_BAR_SLOTS` (25,000) — the same number the aggregator, the
    // indicator engine and the day-OHLC tracker are all sized to — at which
    // the identical config commits **3,000× more memory**.
    //
    // A sizing log that is blind to scale is worse than no sizing log: it
    // answers the question "is this expensive?" with a number that is
    // accurate for a universe nobody is targeting. Project at the ceiling,
    // and report BOTH so the gap between today and the target is visible
    // rather than inferred.
    let projected_bytes = estimated_capacity_bytes(cfg.spot_days, MAX_SPOT_BAR_SLOTS_U32);
    let today_bytes = estimated_capacity_bytes(cfg.spot_days, RAM_STORE_SAMPLE_SLOT_COUNT);
    // Resolved ONCE and reused for both the check and the log line: reading
    // it twice would let the two disagree if the fallback path ever fired
    // between them, and a check that reports a different budget than it
    // enforced is unreadable at 3am.
    let budget_bytes_resolved = ram_store_spot_capacity_budget_bytes();

    if projected_bytes > budget_bytes_resolved {
        // Fail LOUD, not closed. Refusing the install would leave the
        // decision path with no RAM store at all, which is strictly worse
        // than an oversized one — and the overshoot only materialises as the
        // universe actually grows, so there is real time to act. The gauge +
        // this coded line are the signal; the operator lowers `spot_days`.
        error!(
            code = ErrorCode::RamStore01Degraded.code_str(),
            stage = "capacity_projection",
            spot_days = cfg.spot_days,
            projected_bytes,
            budget_bytes = budget_bytes_resolved,
            slot_ceiling = MAX_SPOT_BAR_SLOTS_U32,
            "RAMSTORE-01: spot ring capacity at the slot ceiling EXCEEDS the \
             RAM budget — the rings allocate eagerly per slot, so this much \
             memory is committed as instruments are first seen, not as bars \
             arrive. Lower [market_ram_store] spot_days until the projection \
             fits, or raise the budget deliberately"
        );
    }

    info!(
        spot_days = cfg.spot_days,
        chain_row_cap = cfg.chain_row_cap,
        spot_capacity_bytes_at_slot_ceiling = projected_bytes,
        spot_capacity_bytes_at_sample = today_bytes,
        slot_ceiling = MAX_SPOT_BAR_SLOTS_U32,
        budget_bytes = budget_bytes_resolved,
        "market_ram_store: RAM residency stores installed — spot depth bounded \
         by CAPTURED history (shown honestly by tv_ram_store_spot_days_depth), \
         options current-day (chain publishes + boot rehydrate)"
    );
}

// ---------------------------------------------------------------------------
// Chain-day rehydrate: REMOVED 2026-09-16
// ---------------------------------------------------------------------------
//
// A one-shot, bounded boot task read today's per-minute option-chain rows
// back out of QuestDB and replayed them into the in-RAM chain-day store, so
// a mid-session restart did not start with an empty options view. It owned
// `ChainRehydrateRow`, `chain_rehydrate_sql`, `parse_chain_rehydrate_rows`,
// `build_minute_snapshots`, `rehydrate_window_starts`, `ist_now_nanos`,
// `rehydrate_exec_query`, `run_chain_day_rehydrate` and
// `spawn_chain_day_rehydrate`.
//
// The table it read is `rest_option_chain_1m`, and its WRITER — the
// per-minute option-chain REST leg — was removed the same day under the
// operator's 2026-09-16 directive, recorded BEFORE the code in
// `no-rest-except-live-feed-2026-06-27.md` §12.10: "Bro just remove per
// minute price falls and 3.41 pm accuracy check alone dude okay".
//
// Deleting the reader rather than leaving it is deliberate, and it is the
// §12.6 REJECT row of that section in as many words: "Removes a WRITER and
// leaves a READER pointed at the now-frozen table." A rehydrate left in
// place would have replayed the last pre-removal session's rows into RAM on
// every boot thereafter, silently, as though they were today's — which is
// worse than an empty store, because an empty store is visibly empty.
//
// ⚠ WHAT THIS COSTS, stated rather than absorbed: after a mid-session
// restart the chain-day store now starts EMPTY and fills forward only from
// live publishes. Nothing back-fills the minutes before the restart. The
// TABLE and every row in it are RETAINED (no DROP, no DELETE) — what is
// gone is the writer and the reader, not the history.

// ---------------------------------------------------------------------------
// Stats / heartbeat task
// ---------------------------------------------------------------------------

/// One stats pass: publish the residency gauges (the operator's "is the
/// month actually in RAM?" read surface).
fn publish_ram_store_stats() {
    let mut estimated_bytes = 0u64;
    if let Some(store) = spot_bar_store() {
        let stats = store.stats();
        for &feed in Feed::ALL {
            gauge!("tv_ram_store_spot_bars_resident", "feed" => feed.as_str())
                .set(stats.bars_resident_per_feed[feed.index()] as f64);
            gauge!("tv_ram_store_spot_days_depth", "feed" => feed.as_str())
                .set(f64::from(stats.min_depth_days_per_feed[feed.index()]));
        }
        // PR-2 round-1 HIGH: the spot store is a pure ring core with NO emit
        // sites — its lifetime over-window drop total was previously an
        // UNPUBLISHED stat. Publish it here as a counter-style monotonic
        // gauge so spot drops are a real signal, not a runbook fiction
        // (chain-side drops keep their own tv_ram_store_dropped_total
        // reasons: row_cap / day_drop / minute_cap).
        gauge!("tv_ram_store_spot_dropped_over_window").set(stats.dropped_over_window as f64);
        estimated_bytes += stats.estimated_bytes;
    }
    if let Some(store) = chain_day_store() {
        let stats = store.stats();
        for &feed in Feed::ALL {
            gauge!("tv_ram_store_chain_minutes_resident", "feed" => feed.as_str())
                .set(stats.minutes_resident_per_feed[feed.index()] as f64);
        }
        estimated_bytes += stats.estimated_bytes;
    }
    gauge!("tv_ram_store_estimated_bytes").set(estimated_bytes as f64);
    counter!("tv_ram_store_heartbeat_total").increment(1);
}

/// The stats loop body (60 s cadence; dense heartbeat — a flatline means
/// the task is dead).
async fn run_ram_store_stats_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(RAM_STORE_STATS_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        publish_ram_store_stats();
    }
}

/// Spawns the SUPERVISED stats/heartbeat task (house respawn pattern —
/// DISK-WATCHER-01 family; unwind builds self-heal, release builds abort
/// per `panic = "abort"` — the honest TICK-FLUSH-01 envelope).
// TEST-EXEMPT: tokio spawn loop — gauge names pinned by ram_store_wiring_guard; stats math tested in the store crates.
pub fn spawn_ram_store_stats_task() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_ram_store_stats_loop());
            let result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            counter!("tv_ram_store_errors_total", "stage" => "task_respawn").increment(1);
            error!(
                code = ErrorCode::RamStore01Degraded.code_str(),
                stage = "task_respawn",
                reason,
                task = "ram_store_stats",
                "RAMSTORE-01: RAM store stats task died — respawning after backoff \
                 (a flatlining tv_ram_store_heartbeat_total means release-build \
                 abort; restart is the recovery)"
            );
            tokio::time::sleep(Duration::from_secs(RAM_STORE_STATS_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Neither memory-ceiling reader may hardcode the ROOT cgroup path.
    ///
    /// This process runs in a systemd slice, not the root cgroup, so
    /// `/sys/fs/cgroup/memory.max` does not exist for it -- both reads fail,
    /// the cgroup limit reads as absent, and the caller silently sizes itself
    /// against the machine's whole RAM instead of the limit the unit sets.
    /// It is a false OK: nothing errors, the number is simply too big.
    ///
    /// TWO files had independently hardcoded it (`market_ram_store_boot` and
    /// `ws_frame_spill`), which is why this is a guard rather than a comment
    /// at one call site. The v1 path (`/sys/fs/cgroup/memory/memory.limit_in_bytes`)
    /// is NOT banned -- it is a genuine cgroup-v1 fallback and is not the
    /// root-cgroup-v2 mistake.
    ///
    /// Scans comment-stripped source, because the explanatory comment above
    /// the repaired call site names the very path being banned.
    #[test]
    fn no_memory_ceiling_reader_hardcodes_the_root_cgroup_v2_path() {
        // Assembled from halves, never written whole: the guard scans this
        // very file, and a literal here would make it fail on itself. That is
        // not hypothetical -- the first version did exactly that.
        let banned = format!("{}{}", "/sys/fs/cgroup/", "memory.max");
        let banned = banned.as_str();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .to_path_buf();

        // The two files that got this wrong, plus the resolver itself, which
        // legitimately owns the constant.
        let owners = ["crates/storage/src/resource_monitor.rs"];
        let scanned = [
            "crates/app/src/market_ram_store_boot.rs",
            "crates/storage/src/ws_frame_spill.rs",
        ];

        for rel in scanned {
            let path = root.join(rel);
            let body =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
            let stripped: String = body
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    !t.starts_with("//") && !t.starts_with("///")
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !stripped.contains(banned),
                "{rel} hardcodes the ROOT cgroup path {banned}. This process \
                 is in a systemd slice, so that file does not exist for it and \
                 the limit reads as absent -- sizing against the whole machine \
                 instead. Use \
                 `tickvault_storage::resource_monitor::resolve_cgroup_memory_max_path()`, \
                 which reads /proc/self/cgroup to find the real slice."
            );
        }

        // ⚠ THE HALF THIS GUARD ORIGINALLY MISSED, added 2026-09-03 after a
        // review found the first repair INERT.
        //
        // Banning the root PATH is not enough. The first fix swapped in
        // `resolve_cgroup_memory_max_path()` -- right file, still wrong answer,
        // because the unit sets `MemoryHigh` and no `MemoryMax`, so that file
        // reads the literal "max", parses to None, and the budget fell back to
        // machine RAM exactly as before. THIS GUARD PASSED on that version.
        //
        // So it now also requires the call that actually knows the answer:
        // `resolve_memory_ceiling`, which reads `memory.high` beside
        // `memory.max`. A site that resolves the path without it is the inert
        // shape, and that is the failure worth catching -- a fix that LOOKS
        // applied is worse than none, because nobody looks again.
        for rel in scanned {
            let raw = std::fs::read_to_string(root.join(rel))
                .unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
            // PRODUCTION CODE ONLY, comment-stripped. Two self-contaminations
            // had to be removed before this could bite, and BOTH were found by
            // bite-testing rather than by reading it:
            //   * the prose above the repaired call site names
            //     `resolve_memory_ceiling`, so a raw scan matched a COMMENT;
            //   * this guard's own assertion message names it too, and that is
            //     a string literal in CODE, which comment-stripping cannot
            //     remove.
            // A guard that matches itself can never fail.
            let production = raw.split("\nmod tests").next().unwrap_or(&raw);
            let body: String = production
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    !t.starts_with("//") && !t.starts_with("///")
                })
                .collect::<Vec<_>>()
                .join("\n");
            if body.contains("resolve_cgroup_memory_max_path") {
                assert!(
                    body.contains("resolve_memory_ceiling"),
                    "{rel} resolves the cgroup PATH but never calls \
                     `resolve_memory_ceiling`. On this unit `memory.max` reads \
                     \"max\" (MemoryHigh is set, MemoryMax deliberately is not), \
                     so parsing that file alone yields None and the caller \
                     silently falls back to the whole machine's RAM -- the exact \
                     defect this guard exists to stop, wearing the fix."
                );
            }
        }

        // Non-vacuity: the constant must still exist SOMEWHERE, or this guard
        // would pass just as well if the resolver were deleted.
        let owner_has_it = owners.iter().any(|rel| {
            std::fs::read_to_string(root.join(rel))
                .map(|b| b.contains(banned))
                .unwrap_or(false)
        });
        assert!(
            owner_has_it,
            "the default cgroup-v2 path vanished from the resolver -- this \
             guard can no longer prove anything"
        );
    }

    use super::*;

    // -----------------------------------------------------------------
    // Current-day RAM at the 25,000-instrument ceiling.
    //
    // The 2026-08-12 budget was computed BEFORE depth-20 / depth-200
    // became a persisted stream (2026-08-15). Depth is the only path in
    // the process whose RAM is O(ROWS) rather than O(instruments), so it
    // is the one addition that could invalidate that budget — and the
    // arithmetic below is what proves it does not, rather than assuming.
    // -----------------------------------------------------------------

    /// Every eagerly-committed current-day RAM term at the slot ceiling,
    /// derived from the real constants and `size_of` rather than quoted.
    fn current_day_ram_terms_at_ceiling() -> Vec<(&'static str, u64)> {
        use tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;
        use tickvault_trading::candles::seal_ring::SEAL_BUFFER_CAPACITY;
        use tickvault_trading::candles::tf_index::TF_COUNT;

        let slots = AGGREGATOR_MAX_SLOTS as u64;
        vec![
            // spot_days = 1 (current day) at the full slot ceiling.
            (
                "spot_bar_store",
                estimated_capacity_bytes(1, MAX_SPOT_BAR_SLOTS_U32),
            ),
            // The aggregator's live candle grid: one cell per (slot, TF).
            (
                "aggregator",
                slots
                    * TF_COUNT as u64
                    * core::mem::size_of::<
                        tickvault_trading::candles::live_candle_state::LiveCandleState,
                    >() as u64,
            ),
            // The seal ring is already slots × TF_COUNT entries.
            (
                "seal_ring",
                SEAL_BUFFER_CAPACITY as u64
                    * core::mem::size_of::<tickvault_trading::candles::seal_ring::BufferedSeal>()
                        as u64,
            ),
            // The frame ring's byte CEILING — a bound, not a preallocation,
            // but it must be budgeted because it can legitimately be reached.
            (
                "frame_ring_ceiling",
                crate::dhan_feed_stack::FRAME_RING_MAX_BYTES as u64,
            ),
            // Depth's ONLY RAM term: the un-flushed ILP buffer, bounded by the
            // row threshold. ~160 B of line protocol per row.
            (
                "depth_ilp_buffer",
                crate::dhan_feed_stack::DEPTH_FLUSH_ROW_THRESHOLD * 160,
            ),
        ]
    }

    // ---- host-derived RAM budget (2026-08-21) -------------------------

    #[test]
    fn the_reference_host_reproduces_the_operator_budget_exactly() {
        // The whole point of the 5/16 fraction: on the r8g.xlarge the
        // derived budget must equal the operator's stated 10 GiB TO THE
        // BYTE, so deriving it changes nothing about the live box. A
        // rounded percentage would drift here.
        let r8g_xlarge = 32u64 * 1024 * 1024 * 1024;
        assert_eq!(
            budget_from_host_bytes(r8g_xlarge),
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES,
            "5/16 of 32 GiB must be exactly the 10 GiB fallback"
        );
    }

    #[test]
    fn the_budget_tracks_the_host_up_and_down() {
        // The failure being fixed: the budget did not move when the host
        // did. Half the host must halve it; double must double it.
        let base = 32u64 * 1024 * 1024 * 1024;
        assert_eq!(
            budget_from_host_bytes(base / 2),
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES / 2
        );
        assert_eq!(
            budget_from_host_bytes(base * 2),
            RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES * 2
        );
    }

    #[test]
    fn budget_saturates_instead_of_wrapping() {
        // u64::MAX * 5 wraps to a SMALL number, which would silently pass
        // the projection check this budget exists to fail.
        assert!(budget_from_host_bytes(u64::MAX) > RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES);
        assert_eq!(budget_from_host_bytes(0), 0);
    }

    #[test]
    fn cgroup_unlimited_is_not_mistaken_for_a_limit() {
        // Both dialects. v1 saturates near i64::MAX; v2 writes "max".
        // Reading either as a real limit would produce an astronomically
        // large budget and disable the check entirely.
        assert_eq!(parse_cgroup_limit_bytes("max\n"), None);
        assert_eq!(parse_cgroup_limit_bytes("9223372036854771712\n"), None);
        assert_eq!(parse_cgroup_limit_bytes(""), None);
        assert_eq!(
            parse_cgroup_limit_bytes("2147483648\n"),
            Some(2_147_483_648)
        );
    }

    #[test]
    fn ram_store_spot_capacity_budget_bytes_is_stable_and_plausible() {
        // Exercises the real host path, not a fixture. Cannot assert a
        // specific figure -- CI runners differ, which is the entire point --
        // so it asserts the two properties that must hold anywhere: it is
        // non-zero, and it is CACHED so the check and the log can never
        // disagree.
        let first = ram_store_spot_capacity_budget_bytes();
        assert!(first > 0, "a zero budget would fail every projection");
        assert_eq!(
            first,
            ram_store_spot_capacity_budget_bytes(),
            "the budget must resolve once per process"
        );
    }

    #[test]
    fn current_day_ram_at_25k_instruments_fits_the_operator_budget() {
        let terms = current_day_ram_terms_at_ceiling();
        let total: u64 = terms.iter().map(|(_, b)| *b).sum();
        let report: Vec<String> = terms
            .iter()
            .map(|(n, b)| format!("{n}={:.1} MB", *b as f64 / 1_048_576.0))
            .collect();
        assert!(
            total <= RAM_STORE_SPOT_CAPACITY_BUDGET_FALLBACK_BYTES,
            "current-day RAM at the 25,000-instrument ceiling is {:.2} GB, over the \
             operator's 10 GiB budget (2026-08-12, stated three times). Terms: {}. \
             Raw ticks contribute ZERO by design (folded then dropped), so an \
             overshoot here means one of these structures grew — check which \
             before raising the budget.",
            total as f64 / 1_073_741_824.0,
            report.join(", ")
        );
    }

    #[test]
    fn depth_is_a_rounding_error_in_the_current_day_ram_budget() {
        // The load-bearing claim of the 2026-08-15 depth change: depth adds
        // RAM proportional to the FLUSH THRESHOLD, not to instruments, rows
        // captured, or levels. If someone raises the threshold far enough to
        // make depth a real memory term, this fails and says so.
        let terms = current_day_ram_terms_at_ceiling();
        let depth = terms
            .iter()
            .find(|(n, _)| *n == "depth_ilp_buffer")
            .map(|(_, b)| *b)
            .expect("depth term present");
        let total: u64 = terms.iter().map(|(_, b)| *b).sum();
        assert!(
            depth * 20 < total,
            "the depth ILP buffer is {depth} B of a {total} B current-day footprint \
             — no longer the rounding error the budget assumed. Depth RAM is \
             DEPTH_FLUSH_ROW_THRESHOLD-bounded; raising that constant trades drain \
             occupancy for memory and both sides must be re-argued."
        );
    }

    #[test]
    fn the_depth_flush_threshold_bounds_drain_occupancy_not_just_payload() {
        // Reusing the tick threshold (1,000) would force 10–50 synchronous
        // HTTP round trips per second on the task that also folds ticks,
        // because depth emits 20–200 rows per packet where ticks emit one.
        // The constant exists to break that coupling; this pins the ratio.
        assert!(
            crate::dhan_feed_stack::DEPTH_FLUSH_ROW_THRESHOLD
                >= crate::dhan_feed_stack::FLUSH_ROW_THRESHOLD * 10,
            "depth must flush at least 10x less often per row than ticks, or the \
             drain spends its time blocked in ILP instead of folding ticks"
        );
    }

    // ---- the chain-rehydrate tests: REMOVED 2026-09-16 with the reader ----
    //
    // Four tests lived here (`test_chain_rehydrate_sql_shape`,
    // `test_parse_chain_rehydrate_rows_and_truncation_tripwire`,
    // `test_build_minute_snapshots_groups_rows_per_minute`,
    // `test_rehydrate_window_starts_cover_session`) plus the `row(..)`
    // fixture builder. Every one of them exercised the boot rehydrate that
    // read today's per-minute option-chain table — a table whose WRITER
    // was removed the same day, so the rehydrate could only ever have
    // returned rows from before the removal and then, from the next
    // trading day, nothing at all.
    //
    // Authority: `no-rest-except-live-feed-2026-06-27.md` §12.10.
    // The TABLE and every row already in it are RETAINED; what is gone is
    // the writer, and with it the only reason to read the table at boot.
}
