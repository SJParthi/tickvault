//! Ratchet guard: the Groww Smart-Orders (GTT/OCO) area ships CODE-COMPLETE
//! but with ZERO production callers outside its own `oms/groww/` area files —
//! the margin_gate_off_guard template applied to the GROWW-OCO surface
//! (2026-07-16 implement-everything directive).
//!
//! The 4-gate live-fire lattice is pinned by `groww_order_lattice_guard.rs`
//! (Gates 1-4) and `groww_orders_transport_guard.rs` (Gate 5/6 path + HTTP
//! confinement). THIS guard pins the WIRING boundary: no boot orchestrator,
//! no strategy path, and no sibling area may call the smart-order surface
//! until a future wiring PR updates this ratchet's allowlist DELIBERATELY,
//! never as a side effect.
//!
//! Guard-file honesty: every scan targets SOURCE files under `crates/*/src`
//! — never this guard's own file (it lives under `tests/`), so no assertion
//! can vacuously match its own needle literals. The scanner carries its own
//! non-vacuity self-tests against the area files that genuinely contain the
//! needles.
//!
//! TRIPWIRE, not an adversarial barrier: the method-syntax needles do not
//! catch UFCS calls or aliased imports — an accepted residual class per the
//! house source-scan conventions (the margin_gate_off_guard precedent).

#![cfg(feature = "groww_orders")]

use std::fs;
use std::path::PathBuf;

use tickvault_common::config::GrowwOrdersConfig;

/// Repo root, resolved robustly from this crate's manifest dir.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("guard cannot read {}: {err}", path.display()))
}

/// Recursively collects every `.rs` file under `dir`.
fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// True when the file lives inside the Groww order-side area itself —
/// `crates/trading/src/oms/groww/` — whose executor / api_client /
/// smart_orders / in-file tests legitimately reference the smart surface.
fn is_groww_area_file(path: &std::path::Path) -> bool {
    path.components()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0].as_os_str() == "oms" && w[1].as_os_str() == "groww")
        || path
            .to_string_lossy()
            .replace('\\', "/")
            .contains("/oms/groww/")
}

/// Slices the body of one TOML section: from its `[header]` line to the next
/// line that starts a new `[` table (or EOF). Non-comment header match — a
/// commented-out `# [groww_orders]` banner can never satisfy the check.
fn toml_section<'a>(content: &'a str, header: &str) -> &'a str {
    let mut offset = 0_usize;
    let mut header_end = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') && trimmed.starts_with(header) {
            header_end = Some(offset + line.len());
            break;
        }
        offset += line.len() + 1;
    }
    let after_header =
        header_end.unwrap_or_else(|| panic!("section {header} not found as a non-comment line"));
    let rest = &content[after_header..];
    let end = rest
        .lines()
        .scan(0_usize, |offset, line| {
            let line_start = *offset;
            *offset += line.len() + 1;
            Some((line_start, line))
        })
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map_or(rest.len(), |(line_start, _)| line_start);
    &rest[..end]
}

// ---------------------------------------------------------------------------
// Gate 1 pins scoped to the smart-order knobs (the lattice guard pins the
// section exists + every key false; these pin the SMART keys specifically so
// a future key rename cannot silently drop them from the lattice scan).
// ---------------------------------------------------------------------------

#[test]
fn test_smart_order_config_knobs_default_off() {
    // An EMPTY TOML document (absent section) must deserialize DISABLED.
    let cfg: GrowwOrdersConfig =
        toml::from_str("").unwrap_or_else(|err| panic!("empty TOML must deserialize: {err}"));
    assert!(
        !cfg.smart_orders_read,
        "smart_orders_read serde default OFF"
    );
    assert!(
        !cfg.smart_orders_write,
        "smart_orders_write serde default OFF"
    );
    assert_eq!(cfg.oco_reconcile_poll_secs, 15, "design cadence 15s");
    assert_eq!(
        cfg.oco_sibling_cancel_deadline_secs, 30,
        "design deadline 30s"
    );
    let d = GrowwOrdersConfig::default();
    assert!(!d.smart_orders_read && !d.smart_orders_write);
}

#[test]
fn test_smart_order_base_toml_keys_ship_false() {
    let base_toml = read_repo_file("config/base.toml");
    let section = toml_section(&base_toml, "[groww_orders]");
    for key in ["smart_orders_read = false", "smart_orders_write = false"] {
        assert!(
            section.contains(key),
            "[groww_orders] in config/base.toml must ship `{key}`; section was:\n{section}"
        );
    }
}

// ---------------------------------------------------------------------------
// The no-production-caller ratchet: outside the oms/groww/ area itself, ZERO
// occurrences of the smart-order surface anywhere under crates/*/src.
// ---------------------------------------------------------------------------

#[test]
fn test_smart_order_surface_has_no_production_callers_outside_the_area() {
    // Leading-dot method needles avoid matching fn DEFINITIONS; the plain
    // identifiers catch imports / paths / spawn wiring.
    const NEEDLES: [&str; 10] = [
        ".place_smart_order(",
        ".modify_smart_order(",
        ".cancel_smart_order(",
        ".reconcile_smart_orders(",
        ".create_smart_order(",
        ".get_smart_order(",
        ".list_smart_orders(",
        "run_smart_order_reconcile_loop",
        "SmartOrderGates",
        "smart_orders::",
    ];

    let crates_dir = repo_root().join("crates");
    let mut src_files = Vec::new();
    for entry in fs::read_dir(&crates_dir)
        .unwrap_or_else(|err| panic!("guard cannot read crates/: {err}"))
        .flatten()
    {
        let src = entry.path().join("src");
        if src.is_dir() {
            collect_rs_files(&src, &mut src_files);
        }
    }
    assert!(
        src_files.len() > 50,
        "walker sanity: expected the workspace src walk to find many files, got {}",
        src_files.len()
    );

    let mut area_files_seen = 0_usize;
    for path in &src_files {
        if is_groww_area_file(path) {
            area_files_seen += 1;
            continue; // the area itself is the sanctioned home of the surface
        }
        let content = fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("guard cannot read {}: {err}", path.display()));
        for needle in NEEDLES {
            assert!(
                !content.contains(needle),
                "production caller of the Groww smart-order surface found ({needle:?} in {}) \
                 — the wiring PR must update this ratchet's allowlist DELIBERATELY, never \
                 as a side effect",
                path.display()
            );
        }
    }
    assert!(
        area_files_seen >= 5,
        "walker sanity: the oms/groww area must exist and be exempted \
         (saw {area_files_seen} area files)"
    );
}

// ---------------------------------------------------------------------------
// Non-vacuity self-tests: the needles are REAL — the exempted area files
// genuinely contain them, so an over-eager needle rename cannot make the
// scan above vacuously green.
// ---------------------------------------------------------------------------

#[test]
fn test_scanner_non_vacuity_area_contains_the_needles() {
    let executor = read_repo_file("crates/trading/src/oms/groww/executor.rs");
    assert!(
        executor.contains(".create_smart_order("),
        "executor.rs must call the smart transport (.create_smart_order() — an absent \
         needle would make the no-caller scan vacuous"
    );
    assert!(
        executor.contains("SmartOrderGates"),
        "executor.rs must reference SmartOrderGates"
    );
    let smart = read_repo_file("crates/trading/src/oms/groww/smart_orders.rs");
    assert!(
        smart.contains("pub async fn run_smart_order_reconcile_loop"),
        "smart_orders.rs must define the spawnable reconcile loop"
    );
    assert!(
        smart.contains("pub async fn reconcile_pass"),
        "smart_orders.rs must define reconcile_pass"
    );
}

#[test]
fn test_area_file_predicate_self_test() {
    use std::path::Path;
    assert!(is_groww_area_file(Path::new(
        "/repo/crates/trading/src/oms/groww/smart_orders.rs"
    )));
    assert!(is_groww_area_file(Path::new(
        "crates/trading/src/oms/groww/executor_tests.rs"
    )));
    assert!(!is_groww_area_file(Path::new(
        "/repo/crates/app/src/main.rs"
    )));
    assert!(!is_groww_area_file(Path::new(
        "/repo/crates/trading/src/oms/engine.rs"
    )));
}
