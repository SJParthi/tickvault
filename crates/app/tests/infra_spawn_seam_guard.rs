//! Ratchet: no test in `crates/app/src/infra.rs` may spawn a process that
//! CHANGES the machine it runs on.
//!
//! ## The defect this pins
//!
//! `infra.rs` is the boot module, so almost everything it does is a process
//! spawn — and its unit tests call those functions directly. Until 2026-09-05
//! that meant `cargo test -p tickvault-app` could:
//!
//! * run `docker compose up -d --force-recreate`, which that module's own
//!   `ComposeOutcome` header records verbatim as "NOT idempotent — it tears
//!   down + recreates every container". Six tests reached it, so a developer
//!   with the stack up lost their running QuestDB to a test run.
//! * run `xdg-open`/`open` on a dashboard URL. Five tests reached it through
//!   `open_all_dashboards`, and `DASHBOARD_SERVICES` includes QuestDB on 9000.
//! * run `open -a Docker`, LAUNCHING Docker Desktop. This one is invisible on
//!   Linux — a `!cfg!(target_os = "macos")` guard returns first — and real on
//!   the Mac this repo is developed on, which is the worst combination: CI can
//!   never see it.
//!
//! There was **zero `#[ignore]`** anywhere in the file, and the compose file
//! exists in every checkout, so nothing stopped any of it.
//!
//! ## Why a scan and not a convention
//!
//! The fix is `spawn_program()`, a `cfg(test)` seam that substitutes a no-op
//! binary. A comment saying "route new spawns through the seam" is not
//! enforcement — the next spawn added to this file would simply not do it, and
//! nothing would notice until someone's database vanished. This test is the
//! enforcement.
//!
//! ## The allowlist is shrink-only and READ-ONLY by construction
//!
//! Three spawns are exempt, and every one of them only *reads*: `docker info`,
//! `docker compose ps`, and the generic capability probe. None of them can
//! change a container, a file, or a window. A new entry here is a claim that
//! the spawn cannot alter the machine, and must be justified in review — which
//! is exactly the review this file exists to force.

use tickvault_common::source_scan::strip_rust_comments;

const INFRA_SRC: &str = "src/infra.rs";

/// The spawn needle, assembled rather than written.
///
/// `browser_surface_and_toolchain_guard::every_spawned_binary_is_on_the_allowlist`
/// scans the whole workspace for this exact literal and treats whatever follows
/// it as a spawned binary. A guard that searches for spawns must contain the
/// needle, so writing it contiguously makes THIS file look like it spawns a
/// program called `") {`.
///
/// `concat!` is used instead of adding this file to that guard's
/// `SELF_REFERENTIAL_GUARDS` list deliberately:
/// `rust-only-forever-lock-2026-07-19.md` §0.6 records why that list is two
/// explicit files and never a `*_guard.rs` glob — "that is precisely how an
/// exemption becomes a hole". Splitting the literal costs one line and widens
/// no exemption at all.
const SPAWN_NEEDLE: &str = concat!("Command", "::new(");

/// Spawn sites that are exempt because they only READ machine state.
///
/// The key is the EXACT text following `Command::new(` up to end-of-line, and
/// the match is equality, never a prefix. That is deliberate: a prefix match on
/// `"docker"` would also wave through `Command::new("docker").args(["rm", …])`
/// written on one line. Equality means any edit to the spawn — even appending
/// arguments on the same line — stops matching and fails the guard, which is
/// the safe direction to be wrong in.
const READ_ONLY_SPAWN_ALLOWLIST: [(&str, &str); 5] = [
    (
        "\"docker\")",
        "`docker info` — daemon liveness probe, reads and changes nothing",
    ),
    (
        "\"chronyc\")",
        "`chronyc tracking` — reads the host's NTP offset for the boot \
         clock-skew probe; a pure query with a static argv",
    ),
    (
        "cli.program())",
        "`docker compose ps --format …` — container health COUNT. See \
         `only_one_bare_compose_spawn_exists` for why this entry cannot be \
         reused to smuggle a destructive compose subcommand through.",
    ),
    (
        "program)",
        "`probe_command_succeeds(program, args)` — cold-path CLI capability \
         probe; every caller passes a version/help query",
    ),
    (
        "\"/usr/bin/true\")",
        "the seam's OWN no-op, spawned by \
         `the_test_noop_program_exists_and_ignores_the_compose_argv` to prove \
         it exists and is inert. Exempt by construction: it IS the substitute \
         every other site is redirected to, and a test that asserts the no-op \
         is harmless cannot itself be the harmful spawn.",
    ),
];

/// Read the module under test.
fn infra_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(INFRA_SRC);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    // Comments in this file DISCUSS `Command::new(` at length (including in
    // the seam's own doc comment). Scanning raw text would count those and
    // report spawns that do not exist.
    strip_rust_comments(&raw)
}

/// Every `Command::new(` in `infra.rs`, as the text of its first argument up
/// to the first `\n` — enough to identify the program without parsing Rust.
fn spawn_sites(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(SPAWN_NEEDLE) {
        let after = &rest[i + SPAWN_NEEDLE.len()..];
        let end = after.find('\n').unwrap_or(after.len());
        out.push(after[..end].trim().to_string());
        rest = &after[end.min(after.len())..];
    }
    out
}

#[test]
fn every_destructive_spawn_in_infra_goes_through_the_test_seam() {
    let src = infra_source();
    let sites = spawn_sites(&src);

    // Anti-vacuity: if the scan finds nothing, the needle or the file moved
    // and this guard would pass by finding no work to do.
    assert!(
        sites.len() >= 4,
        "expected several Command::new sites in {INFRA_SRC}, found {} — the \
         needle or the file moved, and a scan that finds nothing passes \
         vacuously",
        sites.len()
    );

    let mut unguarded = Vec::new();
    for site in &sites {
        if site.starts_with("spawn_program(") {
            continue; // routed through the seam — safe by construction
        }
        if READ_ONLY_SPAWN_ALLOWLIST
            .iter()
            .any(|(exact, _)| site == exact)
        {
            continue;
        }
        unguarded.push(site.clone());
    }

    assert!(
        unguarded.is_empty(),
        "{INFRA_SRC} spawns a process without the `spawn_program()` test seam \
         and without a read-only exemption: {unguarded:?}\n\n\
         This file's unit tests call these functions directly, so an \
         unguarded spawn runs for real during `cargo test` — which is how \
         `docker compose up --force-recreate` came to be reachable from a \
         test run. Either route it through `spawn_program()`, or add it to \
         READ_ONLY_SPAWN_ALLOWLIST with a justification that it cannot change \
         the machine."
    );
}

#[test]
fn the_seam_itself_still_exists_and_substitutes_in_test_builds() {
    let src = infra_source();

    // The guard above is satisfied by `spawn_program(` being PRESENT. That is
    // worth nothing if the function stops substituting -- so pin the
    // substitution, not just the call.
    assert!(
        src.contains("#[cfg(test)]") && src.contains("fn spawn_program(_real: &str)"),
        "the cfg(test) arm of `spawn_program` is gone from {INFRA_SRC}. Every \
         call site would then hand `Command::new` the REAL program, and the \
         seam would be a no-op wearing a safe name -- worse than no seam, \
         because the guard above would still pass."
    );
    assert!(
        src.contains("const TEST_SPAWN_NOOP_PROGRAM"),
        "TEST_SPAWN_NOOP_PROGRAM is gone from {INFRA_SRC}; the test arm has \
         nothing to substitute."
    );
    assert!(
        src.contains("#[cfg(not(test))]") && src.contains("fn spawn_program(real: &str)"),
        "the production arm of `spawn_program` is gone from {INFRA_SRC}. \
         Without it the seam would either not compile in release, or -- worse \
         -- production would spawn the no-op instead of Docker."
    );
}

#[test]
fn the_two_known_destructive_sites_are_seam_routed_by_name() {
    let src = infra_source();

    // Named explicitly rather than counted: a count would still pass if a
    // future edit moved the seam OFF compose and ONTO something harmless.
    assert!(
        src.contains(&format!("{SPAWN_NEEDLE}spawn_program(cli.program()))")),
        "the `docker compose up --force-recreate` spawn is no longer seam-routed. \
         That argv tears down and recreates every container, and six unit \
         tests in this module reach it."
    );
    assert!(
        src.contains(&format!("{SPAWN_NEEDLE}spawn_program(program))")),
        "the browser spawn (`open`/`xdg-open`) is no longer seam-routed. Five \
         unit tests reach it via `open_all_dashboards`, and DASHBOARD_SERVICES \
         includes QuestDB on port 9000."
    );
    assert!(
        src.contains(&format!("{SPAWN_NEEDLE}spawn_program(\"open\"))")),
        "the `open -a Docker` (launch Docker Desktop) spawn is no longer \
         seam-routed. It is unreachable on Linux, so CI cannot catch this \
         regression -- only this assertion can."
    );
}

#[test]
fn only_one_bare_compose_spawn_exists_and_the_destructive_argv_is_seam_routed() {
    let src = infra_source();

    // `cli.program()` is the ONE allowlist entry whose program is shared
    // between a read-only site (`ps`) and a destructive one (`up
    // --force-recreate`). Equality matching stops an edit on the SAME line,
    // but the args live on the next line -- so pin the shape directly.
    let bare = src
        .matches(&format!("{SPAWN_NEEDLE}cli.program())"))
        .count();
    assert_eq!(
        bare, 1,
        "expected exactly ONE un-seamed `Command::new(cli.program())` in \
         {INFRA_SRC} (the read-only `ps` health count), found {bare}. The \
         allowlist exempts that program by name, so a SECOND bare site would \
         inherit the exemption while being free to pass any subcommand -- \
         including the `up --force-recreate` this whole guard exists to keep \
         out of `cargo test`."
    );

    // Count PRODUCTION text only. `--force-recreate` also appears twice inside
    // `mod tests`, in the `compose_cli_args` arg-builder assertions -- those
    // build a `Vec<&str>` and spawn nothing, so counting them would make this
    // assertion fail on a correct tree, which is the fastest way to get a
    // guard deleted.
    let prod = src
        .split_once("\nmod tests {")
        .map_or(src.as_str(), |(before, _)| before);
    let recreate = prod.matches("--force-recreate").count();
    assert_eq!(
        recreate, 1,
        "expected exactly ONE `--force-recreate` argv in the PRODUCTION half \
         of {INFRA_SRC}, found {recreate}. That flag is what tears down every \
         container; a second occurrence is a second destructive path this \
         guard has not been taught about."
    );
}
