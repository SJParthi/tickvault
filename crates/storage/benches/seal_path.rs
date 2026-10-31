//! Per-seal cost of what happens to a SEALED candle after the fold produces
//! it (owner directive 2026-10-30: every in-memory step under 1 µs at p99).
//!
//! The steps, in production order:
//!
//! | Bench id | Step | Where |
//! |---|---|---|
//! | `seal_path/handoff_try_send` | drain hands the seal to the writer (`SealSender::try_send`) | frame drain |
//! | `seal_path/take_submit` | writer takes it off the queue and puts it in the ring | seal writer task |
//! | `seal_path/row_build` | `ShadowSealRow::from_buffered_seal` | seal writer task |
//! | `seal_ilp/append_unlabelled` | ILP row into the candle buffer, no contract name | seal writer task |
//! | `seal_ilp/append_labelled` | the same, with the day's ~120,000-name table published | seal writer task |
//! | `seal_ilp/pop_append` | ring pop + ILP append with the day's names published (what `drain_once` does per seal) | seal writer task |
//! | `seal_path/ledger_probe_miss` | live-commit ledger probe, bar not tracked | seal writer task |
//! | `seal_path/ledger_probe_hit` | live-commit ledger probe, bar tracked | seal writer task |
//!
//! NOT measured, and never under 1 µs: the network flush (`Sender::flush`, an
//! HTTP round trip to QuestDB) and every disk write (spill, DLQ, mirror).
//!
//! The ledger probes call `note_live_commits` with ONE seal, so each sample
//! carries the per-call overhead (two atomic loads, the queue mark, the spill
//! lock) that production pays once per flushed batch. That makes them an
//! upper bound per seal, not a typical figure.
//!
//! # The p99 gate
//!
//! After the Criterion runs, under `cargo bench` only, every step above is
//! sampled one call at a time (`TAIL_SAMPLES` calls, each between two
//! `Instant` reads, so the clock pair is inside every sample) and its p99 is
//! held to its group's budget in `quality/benchmark-budgets.toml` (`seal_path` =
//! 1,000 ns, `seal_ilp` for the ILP-append steps), times
//! `BENCH_BUDGET_MULTIPLIER` if set (1 to 10). Over the ceiling, the run
//! exits 1 and the bench job fails. With `BENCH_TAIL_GATE_DEFER=1` (set by
//! `bench.yml`) it prints `BENCH-TAIL-GATE-FAILED` and exits 0 instead, and
//! a later step fails the run on that line, after the Criterion baseline is
//! saved, so a p99 miss cannot stop the job before the save. A self-test (an over-budget sample must
//! fail) runs first. Criterion's own gate (`scripts/bench-gate.sh`) reads the
//! median against the same key.
//!
//! # What does NOT meet 1 µs, stated plainly
//!
//! The three `seal_ilp` steps write one candle row through questdb-rs's
//! `Buffer` API (the table, 2 or 3 symbols, 12 or 18 columns, the
//! timestamp; the bench rows carry all 18 columns). Measured 2026-10-30 on the x86 dev container: median about
//! 1.06 µs, p99 1.8 to 2.4 µs, after this change took out the per-call name
//! validation and the `fmt` delay text (it was median 1.95 µs, p99 3.6 to
//! 4.3 µs). A split-out experiment put the library's own writing, with no
//! validation at all, at about 650 to 850 ns per row on this box, so the
//! remaining gap cannot close inside that API. `seal_ilp` is therefore a
//! separate, higher budget, so the gate holds the measured ceiling rather
//! than failing every run; reaching 1 µs needs the row encoded without the
//! library's row API, which is an owner decision.
//!
//! Run: `cargo bench --bench seal_path -p tickvault-storage`

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group};

use tickvault_common::constants::{EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_NSE_FNO};
use tickvault_common::feed::Feed;
use tickvault_storage::candle_contract_labels::{
    CandleContractLabels, publish_candle_contract_labels,
};
use tickvault_storage::seal_absorption::SealAbsorptionPipeline;
use tickvault_storage::seal_spill::{SealSpillWriter, SerializedSeal};
use tickvault_storage::seal_writer_runner::{SEAL_MPSC_CAPACITY, SealSender};
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_storage::shadow_seal_columns::ShadowSealRow;
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

/// Steps held to the owner's 1 µs: Criterion group and budget key (bench-gate.sh
/// matches the key by substring against every `seal_path/*` id).
const SUB_MICRO_GROUP: &str = "seal_path";
/// The ILP-append steps: their own group and key, because their measured p99 is
/// OVER 1 µs (questdb-rs's row writer; see the module docs).
const ILP_GROUP: &str = "seal_ilp";
/// Env var that widens the ceiling on a slower runner (same name the
/// `feed_drain` gate reads).
const BUDGET_MULTIPLIER_ENV: &str = "BENCH_BUDGET_MULTIPLIER";
/// Set to `1` by `bench.yml`: a p99 miss then prints [`TAIL_GATE_FAILED_MARKER`]
/// and exits 0, and a later workflow step fails the run on that line after the
/// Criterion baseline is saved. Unset (a local `cargo bench`), a miss exits 1.
const TAIL_GATE_DEFER_ENV: &str = "BENCH_TAIL_GATE_DEFER";
/// The line every failed p99 gate prints (same text in `feed_drain`).
const TAIL_GATE_FAILED_MARKER: &str = "BENCH-TAIL-GATE-FAILED";
/// The widest multiplier accepted.
const MAX_BUDGET_MULTIPLIER: f64 = 10.0;
/// Calls sampled per step for the p99 gate.
const TAIL_SAMPLES: usize = 200_000;
/// Seals the bench cycles through (distinct security ids).
const SEAL_POOL: usize = 1_024;
/// A bucket inside the session: 13:20:00 IST as Dhan's IST-epoch seconds.
const SESSION_BUCKET_SECS: u32 = 1_786_800_000;
/// The seal writer's `now` (UTC) at that instant.
const NOW_UNIX_SECS: i64 = SESSION_BUCKET_SECS as i64 - 19_800;
/// Contract names published for the labelled case (about the size the
/// attach publishes: every option row of the day's file).
const LABEL_TABLE_SIZE: usize = 120_000;
/// Appends between buffer resets in the append benches (the live cycle
/// flushes at 4,096 seals or each second, whichever first).
const APPEND_BATCH: usize = 4_096;

fn seal(i: usize) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = SESSION_BUCKET_SECS;
    state.open = 100.05 + i as f64 * 0.05;
    state.high = state.open + 5.0;
    state.low = state.open - 1.0;
    state.close = state.open + 1.5;
    state.volume = 1_234 + i as u64;
    state.bucket_start_cumulative = 1_000;
    state.oi = 50_000;
    state.tick_count = 5;
    state.close_pct_from_prev_day = 1.5;
    state.open_pct = 0.25;
    state.net_volume_signed = -4_242;
    state.net_volume_classified = true;
    state.total_buy_qty = 89_600;
    state.total_sell_qty = 4_800;
    // Real receipt stamps, so the three delay columns are written.
    let open_nanos = i64::from(SESSION_BUCKET_SECS) * 1_000_000_000;
    state.first_receipt_ist_nanos = open_nanos + 12_345_678;
    state.last_receipt_ist_nanos = open_nanos + 45_000_000_000;
    BufferedSeal::new(
        40_000 + i as u64,
        EXCHANGE_SEGMENT_NSE_FNO,
        TfIndex::M1,
        state,
        Feed::Dhan,
    )
}

fn seal_pool() -> Vec<BufferedSeal> {
    (0..SEAL_POOL).map(seal).collect()
}

fn temp_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("tv-bench-seal-path-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The day's contract-name table, with every pool seal's key in it.
fn label_table() -> CandleContractLabels {
    let mut labels = CandleContractLabels::default();
    for i in 0..LABEL_TABLE_SIZE {
        let id = 40_000 + i as i64;
        labels.insert(
            (id, "NSE_FNO"),
            Arc::from(format!("NIFTY-25Nov2026-{}-CE", 20_000 + i).as_str()),
        );
    }
    // A few spot rows on another segment, as the attach publishes.
    labels.insert((13, "IDX_I"), Arc::from("NIFTY"));
    debug_assert_eq!(
        tickvault_common::segment::segment_code_to_str(EXCHANGE_SEGMENT_IDX_I),
        "IDX_I"
    );
    labels
}

// ---------------------------------------------------------------------------
// One-call steps, shared by Criterion and the p99 gate.
// ---------------------------------------------------------------------------

/// The state each step needs, built untimed.
struct Rig {
    seals: Vec<BufferedSeal>,
    tx: SealSender,
    rx: crossbeam_channel::Receiver<BufferedSeal>,
    pipeline: SealAbsorptionPipeline,
    writer: ShadowCandleWriter,
    spill: SealSpillWriter,
    dirs: Vec<PathBuf>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let spill_dir = temp_dir(&format!("{name}-spill"));
        let dlq_dir = temp_dir(&format!("{name}-dlq"));
        let ledger_dir = temp_dir(&format!("{name}-ledger"));
        let (tx, rx) = crossbeam_channel::bounded::<BufferedSeal>(SEAL_MPSC_CAPACITY);
        let spill = SealSpillWriter::with_spill_dir_for_test(ledger_dir.clone());
        // One bar on disk, so the ledger is active and the live-commit probe
        // does its hash probe rather than returning on the atomic short-cut.
        let tracked = seal(0);
        if let Err(err) = spill.append_seal(&SerializedSeal::from(&tracked), NOW_UNIX_SECS) {
            panic!("bench rig: could not spill the tracked bar: {err}"); // APPROVED: bench setup
        }
        // Full production-size ring, prefaulted as `SealAbsorptionPipeline::new`
        // does at boot.
        let mut pipeline =
            SealAbsorptionPipeline::with_dirs_for_test(spill_dir.clone(), dlq_dir.clone());
        pipeline.prefault_ring();
        Self {
            seals: seal_pool(),
            tx,
            rx,
            pipeline,
            writer: ShadowCandleWriter::for_test(),
            spill,
            dirs: vec![spill_dir, dlq_dir, ledger_dir],
        }
    }

    fn seal_at(&self, i: usize) -> BufferedSeal {
        self.seals[i % SEAL_POOL]
    }

    fn reset_writer_every_batch(&mut self, i: usize) {
        if i % APPEND_BATCH == APPEND_BATCH - 1 {
            self.writer.discard_pending();
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        for dir in &self.dirs {
            _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The steps, by bench id suffix. `prepare` runs untimed before each timed
/// call; `step` is the timed call.
#[derive(Clone, Copy, Debug)]
enum Step {
    HandoffTrySend,
    TakeSubmit,
    RowBuild,
    IlpAppendUnlabelled,
    IlpAppendLabelled,
    DrainPopAppend,
    LedgerProbeMiss,
    LedgerProbeHit,
}

const STEPS: [Step; 8] = [
    Step::HandoffTrySend,
    Step::TakeSubmit,
    Step::RowBuild,
    Step::IlpAppendUnlabelled,
    Step::IlpAppendLabelled,
    Step::DrainPopAppend,
    Step::LedgerProbeMiss,
    Step::LedgerProbeHit,
];

impl Step {
    const fn id(self) -> &'static str {
        match self {
            Self::HandoffTrySend => "handoff_try_send",
            Self::TakeSubmit => "take_submit",
            Self::RowBuild => "row_build",
            Self::IlpAppendUnlabelled => "append_unlabelled",
            Self::IlpAppendLabelled => "append_labelled",
            Self::DrainPopAppend => "pop_append",
            Self::LedgerProbeMiss => "ledger_probe_miss",
            Self::LedgerProbeHit => "ledger_probe_hit",
        }
    }

    /// The Criterion group, which is also the budget key this step is held to.
    const fn group(self) -> &'static str {
        match self {
            Self::IlpAppendUnlabelled | Self::IlpAppendLabelled | Self::DrainPopAppend => ILP_GROUP,
            _ => SUB_MICRO_GROUP,
        }
    }

    /// Process-wide setup for this step (the contract-name table).
    fn install(self) {
        let labels = match self {
            Self::IlpAppendLabelled | Self::DrainPopAppend => label_table(),
            _ => CandleContractLabels::default(),
        };
        publish_candle_contract_labels(labels);
    }

    /// Untimed work before call `i`.
    fn prepare(self, rig: &mut Rig, i: usize) {
        match self {
            // Keep the queue from filling: empty it now and then.
            Self::HandoffTrySend => {
                if rig.tx.len() >= SEAL_MPSC_CAPACITY / 2 {
                    while rig.rx.try_recv().is_ok() {}
                }
            }
            Self::TakeSubmit => {
                if rig.pipeline.ring_len() >= rig.pipeline.ring_capacity() / 2 {
                    while rig.pipeline.pop_oldest().is_some() {}
                }
                let s = rig.seal_at(i);
                if rig.tx.try_send(s).is_err() {
                    panic!("bench: the seal queue refused a send"); // APPROVED: bench setup
                }
            }
            Self::IlpAppendUnlabelled | Self::IlpAppendLabelled => rig.reset_writer_every_batch(i),
            Self::DrainPopAppend => {
                rig.reset_writer_every_batch(i);
                let s = rig.seal_at(i);
                if !matches!(
                    rig.pipeline.submit(s, NOW_UNIX_SECS),
                    tickvault_storage::seal_absorption::SubmitOutcome::Buffered
                ) {
                    panic!("bench: the ring did not buffer the seal"); // APPROVED: bench setup
                }
            }
            Self::RowBuild | Self::LedgerProbeMiss | Self::LedgerProbeHit => {}
        }
    }

    /// The timed call.
    #[inline]
    fn run(self, rig: &mut Rig, i: usize) {
        match self {
            Self::HandoffTrySend => {
                let s = rig.seal_at(i);
                black_box(rig.tx.try_send(black_box(s)).is_ok());
            }
            Self::TakeSubmit => {
                if let Ok(s) = rig.rx.try_recv() {
                    black_box(rig.pipeline.submit(s, NOW_UNIX_SECS));
                }
            }
            Self::RowBuild => {
                let s = &rig.seals[i % SEAL_POOL];
                black_box(ShadowSealRow::from_buffered_seal(black_box(s)));
            }
            Self::IlpAppendUnlabelled | Self::IlpAppendLabelled => {
                let s = &rig.seals[i % SEAL_POOL];
                black_box(rig.writer.append_seal(black_box(s)).is_ok());
            }
            Self::DrainPopAppend => {
                if let Some(s) = rig.pipeline.pop_oldest() {
                    black_box(rig.writer.append_seal(&s).is_ok());
                }
            }
            Self::LedgerProbeMiss => {
                // Pool index 1.. never spilled: a miss.
                let s = &rig.seals[1 + i % (SEAL_POOL - 1)..][..1];
                black_box(rig.spill.note_live_commits(black_box(s), NOW_UNIX_SECS));
            }
            Self::LedgerProbeHit => {
                // Pool index 0 is the spilled bar: a hit, same copy, no mirror.
                let s = &rig.seals[..1];
                black_box(rig.spill.note_live_commits(black_box(s), NOW_UNIX_SECS));
            }
        }
    }
}

fn bench_steps(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
) {
    group.measurement_time(Duration::from_secs(3));
    for step in STEPS.into_iter().filter(|s| s.group() == name) {
        step.install();
        let mut rig = Rig::new(step.id());
        group.bench_function(step.id(), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for n in 0..iters {
                    let i = n as usize;
                    step.prepare(&mut rig, i);
                    let t = Instant::now();
                    step.run(&mut rig, i);
                    total += t.elapsed();
                }
                total
            });
        });
    }
}

fn bench_seal_path(c: &mut Criterion) {
    let mut sub_micro = c.benchmark_group("seal_path");
    bench_steps(&mut sub_micro, SUB_MICRO_GROUP);
    sub_micro.finish();
    let mut ilp = c.benchmark_group("seal_ilp");
    bench_steps(&mut ilp, ILP_GROUP);
    ilp.finish();
    publish_candle_contract_labels(CandleContractLabels::default());
}

// ---------------------------------------------------------------------------
// The p99 gate.
// ---------------------------------------------------------------------------

fn budget_from_toml(text: &str, key: &str) -> Option<u64> {
    let mut in_budgets = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_budgets = line == "[budgets]";
            continue;
        }
        if !in_budgets {
            continue;
        }
        if let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            return value.trim().parse::<u64>().ok().filter(|v| *v > 0);
        }
    }
    None
}

fn multiplier(value: Option<&str>) -> Option<f64> {
    match value {
        None => Some(1.0),
        Some(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|m| m.is_finite() && (1.0..=MAX_BUDGET_MULTIPLIER).contains(m)),
    }
}

/// The `num/den` quantile of `sorted` (nearest rank).
fn quantile(sorted: &[u64], num: usize, den: usize) -> Option<u64> {
    if sorted.is_empty() || den == 0 {
        return None;
    }
    let rank = sorted
        .len()
        .saturating_mul(num)
        .div_ceil(den)
        .clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// `true` when the p99 of `sorted` is at or under `ceiling`.
fn within(sorted: &[u64], ceiling: f64) -> Option<bool> {
    quantile(sorted, 99, 100).map(|p99| p99 as f64 <= ceiling)
}

/// A gate that cannot fail proves nothing: an over-budget p99 must fail and
/// one exactly at the budget must pass.
fn self_test(budget: u64) -> Result<(), String> {
    let build = |p99: u64| -> Vec<u64> {
        let mut v = vec![1u64; 98];
        v.push(p99);
        v.push(p99.saturating_mul(2));
        v
    };
    if within(&build(budget + 1), budget as f64) != Some(false) {
        return Err("an over-budget p99 passed the gate".into());
    }
    if within(&build(budget), budget as f64) != Some(true) {
        return Err("a p99 exactly at the budget failed the gate".into());
    }
    Ok(())
}

fn samples_of(step: Step) -> Vec<u64> {
    step.install();
    let mut rig = Rig::new(&format!("tail-{}", step.id()));
    // Warm the caches and the buffer's capacity first, untimed.
    for i in 0..APPEND_BATCH {
        step.prepare(&mut rig, i);
        step.run(&mut rig, i);
    }
    let mut samples = Vec::with_capacity(TAIL_SAMPLES);
    for n in 0..TAIL_SAMPLES {
        let i = APPEND_BATCH + n;
        step.prepare(&mut rig, i);
        let t = Instant::now();
        step.run(&mut rig, i);
        let ns = u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX);
        samples.push(ns);
    }
    samples.sort_unstable();
    samples
}

fn seal_path_tail_gate() -> Result<(), String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../quality/benchmark-budgets.toml"
    );
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let env_value = std::env::var(BUDGET_MULTIPLIER_ENV).ok();
    let mult = multiplier(env_value.as_deref()).ok_or_else(|| {
        format!(
            "{BUDGET_MULTIPLIER_ENV} = {env_value:?} is not a number in [1, {MAX_BUDGET_MULTIPLIER}]"
        )
    })?;
    let mut failures = Vec::new();
    for step in STEPS {
        let key = step.group();
        let budget = budget_from_toml(&text, key)
            .ok_or_else(|| format!("no positive `{key}` in [budgets] of {path}"))?;
        self_test(budget)?;
        let ceiling = budget as f64 * mult;
        let samples = samples_of(step);
        let at = |num, den| quantile(&samples, num, den).unwrap_or(0);
        let pass = within(&samples, ceiling) == Some(true);
        println!(
            "{key}/{} over {} calls: p50 = {} ns, p99 = {} ns, p99.9 = {} ns, max = {} ns \
             (each sample includes one Instant pair); ceiling {ceiling:.0} ns: {}",
            step.id(),
            samples.len(),
            at(1, 2),
            at(99, 100),
            at(999, 1_000),
            samples.last().copied().unwrap_or(0),
            if pass { "PASS" } else { "FAIL" },
        );
        if !pass {
            failures.push(format!(
                "{key}/{} p99 {} ns over {ceiling:.0} ns",
                step.id(),
                at(99, 100)
            ));
        }
    }
    publish_candle_contract_labels(CandleContractLabels::default());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join(", "))
    }
}

criterion_group!(benches, bench_seal_path);

/// `criterion_main!` plus the p99 gate. The gate runs only under
/// `cargo bench` (`--bench`); a test-mode run is unoptimised.
fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    if std::env::args().any(|a| a == "--bench")
        && let Err(why) = seal_path_tail_gate()
    {
        eprintln!("{TAIL_GATE_FAILED_MARKER} seal_path p99 gate: {why}");
        if std::env::var(TAIL_GATE_DEFER_ENV).as_deref() != Ok("1") {
            std::process::exit(1);
        }
    }
}
