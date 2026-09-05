//! Guard: the 4-vCPU core partition must stay COHERENT across the four files
//! that decide it, and must stay REVERSIBLE without a code change.
//!
//! # The defect this exists to stop recurring
//!
//! The box is r8g.xlarge — 4 vCPU, Graviton4, not burstable. Until 2026-08-19
//! three tenants each behaved as though they owned all four cores: this app's
//! tokio runtime defaulted to `worker_threads = num_cpus = 4`, QuestDB held a
//! `cpus: 3.0` QUOTA with no cpuset, and NIC softirq landed wherever the kernel
//! chose. The measured result is recorded in the compose file itself:
//! `nr_throttled 18,594 of nr_periods 85,149` — **21.8%** of QuestDB's
//! scheduling periods throttled.
//!
//! The reason a quota was not enough is the whole point of this guard: a quota
//! bounds how MUCH a tenant runs, and only a cpuset bounds WHERE. QuestDB could
//! be, and was, scheduled onto whichever core the tick drain was using.
//!
//! Why that costs ticks rather than milliseconds: when the drain is
//! descheduled the socket receive buffer fills, and Dhan's published
//! architecture skips a slow consumer forward to *"the latest available
//! state"*. The intermediate ticks are dropped at THEIR side, and the India
//! feed carries no sequence number, so the loss is undetectable after the fact.
//!
//! # The partition
//!
//! | core | owner |
//! |------|-------|
//! | 0 | OS + NIC IRQ/softirq + log sidecars — never a pin target for the drain |
//! | 1 | tickvault app — exclusive |
//! | 2 | shared burst: app + QuestDB |
//! | 3 | QuestDB — exclusive |
//!
//! # Why this guard is not itself rot
//!
//! It does not restate the partition as prose. It PARSES the three deploy files
//! and the app default, expands the cpu sets, and checks the RELATIONSHIPS —
//! so a change to any single file that breaks the fit fails here, whichever
//! file moved.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const COMPOSE: &str = "deploy/docker/docker-compose.yml";
const APP_UNIT: &str = "deploy/systemd/tickvault.service";
const TUNING_UNIT: &str = "deploy/systemd/tickvault-host-tuning.service";
const TUNING_SCRIPT: &str = "deploy/aws/host-tuning/apply-host-tuning.sh";
const MAIN_RS: &str = "crates/app/src/main.rs";

/// The core the NIC interrupts are concentrated on. CLAUDE.md records that this
/// is NOT a valid pin target for the decoder — pinning it beside the softirq it
/// depends on is contention, not isolation — which is exactly why the app's set
/// starts at 1.
const IRQ_CORE: u32 = 0;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("cpu_partition_guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("cpu_partition_guard: {rel} must be readable: {e}"))
}

/// Expand a systemd/docker cpu-list (`"1-2"`, `"2,3"`, `"0"`) into the set of
/// cores it names. Ranges and comma lists are both legal in both syntaxes.
fn expand_cpu_list(spec: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi) = (
                    lo.trim().parse::<u32>().unwrap_or_else(|_| {
                        panic!("cpu_partition_guard: bad range low bound in {spec:?}")
                    }),
                    hi.trim().parse::<u32>().unwrap_or_else(|_| {
                        panic!("cpu_partition_guard: bad range high bound in {spec:?}")
                    }),
                );
                for c in lo..=hi {
                    out.insert(c);
                }
            }
            None => {
                out.insert(
                    part.parse::<u32>().unwrap_or_else(|_| {
                        panic!("cpu_partition_guard: bad cpu index in {spec:?}")
                    }),
                );
            }
        }
    }
    out
}

/// Pull the DEFAULT out of a compose `${VAR:-default}` interpolation, so the
/// guard reads what an un-overridden box actually gets.
fn compose_default(line: &str) -> String {
    let after = line
        .split_once(":-")
        .unwrap_or_else(|| panic!("cpu_partition_guard: expected a ${{VAR:-default}} in {line:?}"))
        .1;
    after
        .split('}')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('"')
        .to_string()
}

fn compose_value_for(compose: &str, key: &str) -> String {
    let line = compose
        .lines()
        .find(|l| l.trim_start().starts_with(key))
        .unwrap_or_else(|| panic!("cpu_partition_guard: {COMPOSE} must carry `{key}`"));
    compose_default(line)
}

fn questdb_cores() -> BTreeSet<u32> {
    expand_cpu_list(&compose_value_for(&read(COMPOSE), "cpuset:"))
}

fn app_cores() -> BTreeSet<u32> {
    let unit = read(APP_UNIT);
    let line = unit
        .lines()
        .find(|l| {
            l.trim_start().starts_with("AllowedCPUs=") && l.trim().len() > "AllowedCPUs=".len()
        })
        .unwrap_or_else(|| {
            panic!("cpu_partition_guard: {APP_UNIT} must confine the app with `AllowedCPUs=`")
        });
    expand_cpu_list(line.trim().trim_start_matches("AllowedCPUs="))
}

#[test]
fn the_app_and_questdb_each_own_a_core_the_other_cannot_touch() {
    let app = app_cores();
    let qdb = questdb_cores();

    let app_only: Vec<_> = app.difference(&qdb).copied().collect();
    let qdb_only: Vec<_> = qdb.difference(&app).copied().collect();

    assert!(
        !app_only.is_empty(),
        "the app has NO core QuestDB is excluded from (app={app:?}, questdb={qdb:?}). \
         A quota alone was the pre-2026-08-19 arrangement and it produced 21.8% \
         throttling — the drain must own at least one core outright."
    );
    assert!(
        !qdb_only.is_empty(),
        "QuestDB has NO core of its own (app={app:?}, questdb={qdb:?}). Starving \
         the writer backs up the ILP flush, which backs up the drain — the same \
         harm from the other direction."
    );
}

#[test]
fn the_app_is_never_pinned_onto_the_irq_core() {
    let app = app_cores();
    assert!(
        !app.contains(&IRQ_CORE),
        "the app is allowed on core {IRQ_CORE}, which services NIC softirq. \
         CLAUDE.md records this explicitly: pinning the decoder there puts it in \
         direct contention with the very work that delivers its packets. \
         app={app:?}"
    );
}

#[test]
fn the_irq_core_is_left_free_of_both_heavy_tenants() {
    let app = app_cores();
    let qdb = questdb_cores();
    assert!(
        !app.contains(&IRQ_CORE) && !qdb.contains(&IRQ_CORE),
        "core {IRQ_CORE} carries the NIC interrupts and must host neither heavy \
         tenant (app={app:?}, questdb={qdb:?}) — otherwise the steering in \
         {TUNING_SCRIPT} concentrates interrupts onto a core that is already busy."
    );
}

#[test]
fn tokio_worker_count_is_explicit_and_matches_the_app_core_set() {
    let main_rs = read(MAIN_RS);

    // Match the ATTRIBUTE at line start, not the token anywhere in the file:
    // main.rs deliberately NAMES `#[tokio::main]` in the comment explaining why
    // it no longer uses it, and a substring check would trip on that
    // explanation. A guard that fails on its own subject's documentation
    // teaches people to delete the documentation.
    let bare_attribute = main_rs
        .lines()
        .any(|l| l.trim() == "#[tokio::main]" || l.trim().starts_with("#[tokio::main("));
    assert!(
        !bare_attribute,
        "main.rs is back on the `#[tokio::main]` attribute, which means \
         `worker_threads = num_cpus`. That is the setting this partition exists \
         to replace — the runtime must be built explicitly."
    );
    assert!(
        main_rs.contains("fn resolve_tokio_worker_threads(")
            && main_rs.contains(".worker_threads("),
        "main.rs must build the runtime with an EXPLICIT worker_threads count"
    );

    let default_line = main_rs
        .lines()
        .find(|l| l.contains("const DEFAULT_TOKIO_WORKER_THREADS"))
        .expect(
            "main.rs must name the worker count as a constant, never a literal at the call site",
        );
    let workers: usize = default_line
        .rsplit('=')
        .next()
        .and_then(|v| v.trim().trim_end_matches(';').parse().ok())
        .expect("DEFAULT_TOKIO_WORKER_THREADS must be a plain integer literal");

    let app = app_cores();
    assert_eq!(
        workers,
        app.len(),
        "the runtime is {workers} workers wide but the app is confined to {} core(s) \
         ({app:?}). More workers than cores buys context switches, not throughput — \
         and it is the exact mismatch that produced the throttling this change \
         removes. Move both together.",
        app.len()
    );
}

#[test]
fn questdb_worker_pools_never_exceed_its_cpuset() {
    let compose = read(COMPOSE);
    let cores = questdb_cores().len();

    for key in ["QDB_WAL_APPLY_WORKER_COUNT:", "QDB_SHARED_WORKER_COUNT:"] {
        let raw = compose_value_for(&compose, key);
        // 0 means "let QuestDB decide" and is a legal rollback value.
        let n: usize = raw.parse().unwrap_or_else(|_| {
            panic!("cpu_partition_guard: {key} default {raw:?} must be an integer")
        });
        assert!(
            n <= cores,
            "{key} defaults to {n} but QuestDB's cpuset is {cores} core(s). This \
             value has already been wrong twice in the same direction (4 against a \
             3.0 quota, then 3 against a 2-core reality); workers above the width \
             of the cpuset buy CFS throttling, never throughput."
        );
    }
}

#[test]
fn the_quota_never_undercuts_the_cpuset() {
    let compose = read(COMPOSE);
    let cores = questdb_cores().len() as f64;
    let quota: f64 = compose_value_for(&compose, "cpus:")
        .parse()
        .expect("cpu_partition_guard: the QuestDB `cpus:` default must parse as a number");
    assert!(
        quota >= cores,
        "QuestDB's CPU quota ({quota}) is BELOW its cpuset width ({cores}). The \
         cpuset is meant to be the binding constraint; a lower quota re-introduces \
         period-based CFS throttling on top of it, which is the thing being removed."
    );
}

#[test]
fn every_partition_knob_is_reversible_by_configuration_alone() {
    let compose = read(COMPOSE);
    // Each docker-side knob must be an env interpolation, or rollback needs a
    // code edit + redeploy rather than a restart.
    for key in [
        "cpuset:",
        "cpus:",
        "QDB_WAL_APPLY_WORKER_COUNT:",
        "QDB_SHARED_WORKER_COUNT:",
    ] {
        let line = compose
            .lines()
            .find(|l| l.trim_start().starts_with(key))
            .unwrap_or_else(|| panic!("cpu_partition_guard: {COMPOSE} must carry `{key}`"));
        assert!(
            line.contains("${") && line.contains(":-"),
            "`{key}` is hardcoded in {COMPOSE}. The operator accepted a real risk \
             here (the box wakes on a trading day in an arrangement never run \
             before), and the mitigation he was promised is that rollback is \
             config plus a restart — so every knob must be `${{VAR:-default}}`. \
             Offending line: {line}"
        );
    }

    let unit = read(APP_UNIT);
    assert!(
        unit.contains("TICKVAULT_TOKIO_WORKER_THREADS="),
        "{APP_UNIT} must set TICKVAULT_TOKIO_WORKER_THREADS so the runtime width \
         is changeable without a rebuild"
    );
    assert!(
        unit.contains(r"printf '[Service]\nAllowedCPUs=\n'"),
        "{APP_UNIT} must document the EMPTY-assignment drop-in rollback VERBATIM. \
         `AllowedCPUs=` with no value is what resets the list in systemd, and an \
         operator reaching for it at 09:00 on a bad morning should not have to \
         remember the syntax."
    );

    assert!(
        read(TUNING_UNIT).contains("EnvironmentFile=-/etc/tickvault/host-tuning.env"),
        "{TUNING_UNIT} must read an OPTIONAL operator override file (leading `-`), \
         or the IRQ steering has no on-box rollback"
    );
}

#[test]
fn irq_steering_is_idempotent_fail_safe_and_defeats_irqbalance() {
    let script = read(TUNING_SCRIPT);

    assert!(
        script.contains("smp_affinity_list"),
        "{TUNING_SCRIPT} must steer NIC IRQs via /proc/irq/*/smp_affinity_list"
    );
    assert!(
        script.contains("TV_IRQ_STEER") && script.contains("TV_IRQ_CPU"),
        "the steering must be switchable and its target core configurable"
    );
    assert!(
        script.contains("irqbalance"),
        "{TUNING_SCRIPT} must deal with irqbalance explicitly. It re-spreads \
         interrupts on a timer, so writing static masks beside a running \
         irqbalance produces tuning that LOOKS applied and silently reverts — \
         the false-OK class the charter forbids."
    );
    assert!(
        script.contains("ip -o route show default"),
        "the NIC must be resolved from the default route, not hardcoded — a wrong \
         interface name would steer nothing while reporting success"
    );
    assert!(
        script.trim_end().ends_with("exit 0"),
        "{TUNING_SCRIPT} must still end `exit 0`. A tuning failure must never keep \
         the trading app down: a host with default IRQ placement is slower, a host \
         whose boot aborted has no app at all."
    );
}

#[test]
fn a_host_too_small_for_the_partition_runs_the_app_unconfined() {
    let script = read(TUNING_SCRIPT);
    let app = app_cores();
    let highest = *app.iter().next_back().expect("the app core set is empty");

    assert!(
        script.contains("TV_APP_CPUS_MIN_CORES"),
        "{TUNING_SCRIPT} must guard the AllowedCPUs set against a host with fewer \
         cores than it names, and run the app UNCONFINED there rather than pinning \
         it into a set the kernel cannot honour"
    );
    let min_line = script
        .lines()
        .find(|l| l.trim_start().starts_with("TV_APP_CPUS_MIN_CORES="))
        .expect("TV_APP_CPUS_MIN_CORES must be assigned");
    let min: u32 = min_line
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().parse().ok())
        .expect("TV_APP_CPUS_MIN_CORES must be an integer");
    assert_eq!(
        min,
        highest + 1,
        "the app's highest allowed CPU is {highest}, so the partition needs at \
         least {} cores, but the safety net triggers below {min}. A mismatch here \
         either pins the app on a host that cannot honour it, or removes the \
         confinement on a host that can.",
        highest + 1
    );
    assert!(
        script.contains("AllowedCPUs=\n\"") || script.contains("AllowedCPUs="),
        "the safety net must write an EMPTY AllowedCPUs assignment — that is what \
         resets the list in systemd"
    );
}

#[test]
fn expand_cpu_list_parses_both_syntaxes() {
    assert_eq!(expand_cpu_list("1-2"), BTreeSet::from([1, 2]));
    assert_eq!(expand_cpu_list("2,3"), BTreeSet::from([2, 3]));
    assert_eq!(expand_cpu_list("0"), BTreeSet::from([0]));
    assert_eq!(expand_cpu_list("0-3"), BTreeSet::from([0, 1, 2, 3]));
    assert_eq!(expand_cpu_list("0,2-3"), BTreeSet::from([0, 2, 3]));
}

/// The memory drop-in must NEVER lower `MemoryHigh` on a host that can reach
/// the unit's own value.
///
/// # Why this shape and not a budget formula
///
/// `MemoryHigh` is a THROTTLE, not a reservation, and the unit's value
/// deliberately overcommits: its own comment records that 15G re-entered a
/// SIGABRT loop, so 20G was measured, not estimated. A drop-in that "budgeted"
/// the app against QuestDB's share would compute ~17G on the locked 32 GiB
/// host and LOWER the ceiling there — and because `resolve_memory_ceiling`
/// reads `memory.high` when no `MemoryMax=` is set, that would silently move
/// the WAL catch-up stand-down (60%) and the RESOURCE-02 page line (80%) down
/// with it. Tuning the live box was never the goal.
///
/// The goal is narrower: on a host where the unit's value EXCEEDS physical RAM,
/// the throttle is inert and takes both derived thresholds with it — three
/// safety nets disabled while every surface reads healthy. The drop-in exists
/// only for that case.
///
/// So the property pinned here is the one that protects the locked host: the
/// comparison must be `unit < MemTotal → no drop-in`. An inverted or widened
/// comparison would start writing a lower ceiling on r8g.xlarge.
#[test]
fn the_memory_dropin_only_fires_when_the_unit_value_exceeds_host_ram() {
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/aws/host-tuning/apply-host-tuning.sh"),
    )
    .expect("apply-host-tuning.sh must be readable");

    assert!(
        script.contains(r#"[ "$tv_unit_high_g" -lt "$tv_mem_g" ]"#),
        "the reachable-host arm must compare the UNIT value against MemTotal \
         with `-lt`. Widening it (`-le`, or a budget subtraction) starts \
         writing a lower MemoryHigh on the locked 32 GiB host, which moves the \
         WAL stand-down and the RESOURCE-02 page line down with it."
    );
    assert!(
        script.contains("91-memory-guard.conf"),
        "the drop-in must have its own filename, separate from the CPU guard's \
         90-cpu-guard.conf — one file serving both would make removing either \
         remove the other"
    );

    // The value is READ from the unit, never restated in the script. A second
    // copy of that number is drift waiting to happen, and this script would be
    // the copy nobody updates when the unit is retuned.
    assert!(
        script.contains("/etc/systemd/system/tickvault.service"),
        "the script must READ MemoryHigh from the installed unit"
    );
    assert!(
        !script.contains("MemoryHigh=20G"),
        "the script must not hardcode the unit's current value — it reads it"
    );

    // Unreadable inputs must change NOTHING. A ceiling derived from a guessed
    // MemTotal is worse than the unit's own value.
    assert!(
        script.contains(r#"[ -z "$tv_unit_high_g" ] || [ "$tv_mem_g" -le 0 ]"#),
        "an unreadable unit value or an unreadable MemTotal must skip, never \
         derive a ceiling from a guess"
    );

    // The host-grew-back path, the same shape the CPU guard already has.
    assert!(
        script.contains("removed the memory escape hatch"),
        "a host that grows back must have its drop-in REMOVED, or the lowered \
         ceiling outlives the reason for it"
    );

    // QuestDB's share must reuse the deploy formula rather than invent a
    // second one; two derivations that disagree hand the same RAM twice.
    assert!(
        script.contains("tv_qdb_g=$(( tv_mem_g * 4 / 10 ))")
            && script.contains(r#"[ "$tv_qdb_g" -gt 12 ] && tv_qdb_g=12"#),
        "the QuestDB share must reproduce deploy-aws.yml's 4/10-capped-at-12 \
         formula exactly"
    );
}

/// The unit keeps a fixed `MemoryHigh` and no `MemoryMax`.
///
/// `MemoryMax` would turn a memory spike into an OOM kill of the only
/// tick-capture process — the outcome `OOMScoreAdjust=-900` exists to avoid.
#[test]
fn the_unit_throttles_memory_and_never_hard_caps_it() {
    let unit = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/systemd/tickvault.service"),
    )
    .expect("tickvault.service must be readable");

    assert!(
        unit.lines().any(|l| l.starts_with("MemoryHigh=")),
        "the unit must carry a MemoryHigh throttle — it is also the ceiling \
         `resolve_memory_ceiling` reads, so removing it disables the WAL \
         stand-down and the RESOURCE-02 page line as well"
    );
    assert!(
        !unit.lines().any(|l| l.starts_with("MemoryMax=")),
        "MemoryMax turns a spike into an OOM kill of the one process whose \
         death loses ticks. Throttle and page, never kill."
    );
    assert!(
        unit.contains("OOMScoreAdjust=-900"),
        "the app must rank BELOW QuestDB (-500) in the kill order, so host \
         exhaustion takes the recoverable tenant first"
    );
}
