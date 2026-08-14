//! Ratchet: the ARM64 production build's `target-cpu` must stay compatible
//! with EVERY instance type the box can legitimately end up running on.
//!
//! ## The hazard
//!
//! The live instance is `r8g.xlarge` — Graviton4, which implements Neoverse
//! V2 — so `-C target-cpu=neoverse-v2` looks like the obvious maximal choice.
//! It is a trap, because the instance type is not fixed at runtime:
//! `.github/workflows/downsize-instance.yml` AUTO-ROLLS-BACK to `t4g.medium`
//! when AWS refuses capacity for the larger type.
//!
//! That is not hypothetical. On 2026-08-07 a `t4g.large` flip was refused with
//! `InsufficientInstanceCapacity` and the workflow rolled the box back. `t4g`
//! is Graviton2 (Neoverse N1). A binary compiled for V2 executes instructions
//! Graviton2 does not implement, so it dies with SIGILL — meaning the rollback
//! that exists to SAVE the box would instead brick the app on arrival, at the
//! exact moment nobody is watching closely.
//!
//! ## What this guard enforces
//!
//! A conditional, not a constant: **as long as a Graviton2 rollback path
//! exists, the target-cpu must be Graviton2-compatible.** If someone removes
//! the rollback (making the fleet uniformly Graviton4), raising the baseline
//! becomes legal automatically and this guard stops objecting — which is the
//! correct coupling, because the rollback is the actual constraint.

use std::fs;
use std::path::{Path, PathBuf};

/// Baselines every Graviton generation from 2 onward implements. Anything
/// outside this set requires the rollback path to be gone first.
const GRAVITON2_SAFE_TARGET_CPUS: &[&str] = &[
    "generic",
    "neoverse-n1", // ARMv8.2-A + LSE — Graviton2/3/4 all implement it
];

/// Baselines that are Graviton3-or-newer only. Listed explicitly so the
/// failure message can name WHY a value is rejected rather than just saying
/// "unrecognised".
const GRAVITON3_PLUS_ONLY: &[&str] = &[
    "neoverse-v1", // Graviton3
    "neoverse-v2", // Graviton4
    "neoverse-n2",
    "native", // resolves to the BUILDER's cpu — never correct for a cross-compile
];

fn repo_root() -> PathBuf {
    // crates/common -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root is two levels above crates/common")
        .to_path_buf()
}

/// Extracts the `target-cpu=<value>` configured for the ARM64 musl target, if
/// any. Returns `None` when no target-cpu is set (the pre-2026-08-14 state,
/// which is safe-but-slower and therefore allowed).
fn configured_arm_target_cpu(cargo_config: &str) -> Option<String> {
    let section = cargo_config.find("[target.aarch64-unknown-linux-musl]")?;
    // Bound the search to this section: the next `[` at line start, or EOF.
    let rest = &cargo_config[section + 1..];
    let end = rest.find("\n[").map_or(rest.len(), |i| i + 1);
    let body = &rest[..end];

    let key = body.find("target-cpu=")?;
    let after = &body[key + "target-cpu=".len()..];
    let value: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// True when a workflow still contains an automatic rollback to a Graviton2
/// (`t4g`) instance type.
fn graviton2_rollback_path_exists(root: &Path) -> bool {
    let workflow = root.join(".github/workflows/downsize-instance.yml");
    let Ok(text) = fs::read_to_string(&workflow) else {
        // Missing workflow == no rollback path we can see. Fail SAFE by
        // assuming the constraint still applies rather than silently
        // unlocking the wider baselines.
        return true;
    };
    text.contains("t4g.")
}

#[test]
fn arm_target_cpu_is_compatible_with_every_reachable_instance() {
    let root = repo_root();
    let cargo_config =
        fs::read_to_string(root.join(".cargo/config.toml")).expect("read .cargo/config.toml");

    let Some(target_cpu) = configured_arm_target_cpu(&cargo_config) else {
        // No target-cpu set: generic ARMv8.0-A. Slower (LL/SC atomics instead
        // of LSE) but correct everywhere. Nothing to enforce.
        return;
    };

    if !graviton2_rollback_path_exists(&root) {
        // The rollback is gone, so Graviton2 is no longer reachable and any
        // baseline is the author's call.
        return;
    }

    assert!(
        !GRAVITON3_PLUS_ONLY.contains(&target_cpu.as_str()),
        "`.cargo/config.toml` sets `target-cpu={target_cpu}` for the ARM64 production \
         target, but `.github/workflows/downsize-instance.yml` still auto-rolls-back to a \
         t4g (Graviton2 / Neoverse N1) instance.\n\n\
         A binary built for {target_cpu} executes instructions Graviton2 does not \
         implement and dies with SIGILL on arrival — so the rollback that exists to SAVE \
         the box would brick the app instead. This is not theoretical: the 2026-08-07 \
         t4g.large flip was refused with InsufficientInstanceCapacity and the workflow \
         rolled back.\n\n\
         Use one of {GRAVITON2_SAFE_TARGET_CPUS:?}, or remove the Graviton2 rollback path \
         FIRST and this guard will stop objecting on its own."
    );

    assert!(
        GRAVITON2_SAFE_TARGET_CPUS.contains(&target_cpu.as_str()),
        "`target-cpu={target_cpu}` is not on the known-Graviton2-safe list \
         {GRAVITON2_SAFE_TARGET_CPUS:?}. If it genuinely is safe on Graviton2, add it to \
         that list with a note; do not widen the guard by deleting the check."
    );
}

#[test]
fn the_extractor_reads_the_arm_section_and_not_a_neighbouring_one() {
    // Self-test: a target-cpu belonging to a DIFFERENT target must never be
    // attributed to the ARM64 one, or the guard would police the wrong value.
    let fixture = "\
[target.x86_64-unknown-linux-gnu]\n\
rustflags = [\"-C\", \"target-cpu=native\"]\n\
\n\
[target.aarch64-unknown-linux-musl]\n\
rustflags = [\"-C\", \"target-cpu=neoverse-n1\"]\n";
    assert_eq!(
        configured_arm_target_cpu(fixture).as_deref(),
        Some("neoverse-n1")
    );

    // And an ARM section with no target-cpu must read as None, not as the
    // x86 section's value.
    let no_cpu = "\
[target.x86_64-unknown-linux-gnu]\n\
rustflags = [\"-C\", \"target-cpu=native\"]\n\
\n\
[target.aarch64-unknown-linux-musl]\n\
rustflags = [\"-C\", \"link-arg=-s\"]\n";
    assert_eq!(configured_arm_target_cpu(no_cpu), None);
}

#[test]
fn the_live_config_actually_sets_the_lse_baseline() {
    // Non-vacuity: without this, deleting the whole section would make the
    // first test pass by early-return. The point of the change is the LSE
    // atomics win, so assert it is actually configured.
    let cargo_config = fs::read_to_string(repo_root().join(".cargo/config.toml"))
        .expect("read .cargo/config.toml");
    assert_eq!(
        configured_arm_target_cpu(&cargo_config).as_deref(),
        Some("neoverse-n1"),
        "the ARM64 production target should build with the neoverse-n1 baseline (ARMv8.2-A \
         + LSE atomics). Generic ARMv8.0-A compiles every atomic into an LL/SC retry loop, \
         and this codebase executes atomics per frame on the capture path."
    );
}
