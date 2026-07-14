//! 🔷 DHAN exit-order layer lockout guard — **operator lock 2026-07-14**.
//!
//! Operator context (2026-07-14, coordinator-relayed): the DHAN exit-order
//! execution layer (Cluster B — super orders, forever/OCO, slicing, MPP
//! verify) is authorized to land code-complete but DEFAULT-OFF behind four
//! independent locks; enabling the config flag activates dry-run paper
//! behavior only, and going live requires the enable-time protocol in the
//! rule file (dated operator quote FIRST).
//!
//! This is a mechanical, build-failing tripwire: because `All Green`
//! (the ci.yml fan-in) is a GitHub-enforced required check on `main`,
//! any future PR that tries to weaken one of the four locks fails CI and
//! the merge button is physically blocked. Specifically it fails if:
//!
//!  1. Any `config/*.toml` sets `enabled = true` inside an `[exit_orders]`
//!     section (the deploy workflow refreshes the repo clone on the AWS
//!     box and copies `config/` — a flipped flag in ANY tracked config IS
//!     the activation path). Non-vacuous: `config/base.toml` must CARRY
//!     the section with `enabled = false`.
//!  2. The `ExitOrdersConfig` serde/`Default` `enabled` default drifts
//!     away from `false` (a true default would activate the dispatcher on
//!     every boot with zero config change).
//!  3. The engine's hardcoded `dry_run: true` in `new()` disappears, or a
//!     live-mode setter escapes the `#[cfg(test)]` gate.
//!  4. Any exit method's `if self.dry_run` branch stops preceding the
//!     token fetch in source order (the dry-run-before-token ladder), or
//!     the `// LIVE-EXIT-ARM` marker drifts.
//!  5. The app dispatcher loses its `!cfg.enabled` early-return gate or
//!     its `tv_exit_commands_dropped_total` drop counter.
//!  6. Anything under `deploy/`, the CI workflows, or the Makefile sets
//!     the exit-orders flag.
//!  7. The exit layer grows a Telegram dispatch path without the
//!     noise-lock family row (2026-07-14 Dhan noise lock — zero new
//!     Telegram emit sites; EXIT-ORDER-01 / EXIT-VERIFY-01 are
//!     log-sink-only).
//!  8. `validate_super_order_prices` regresses to `#[allow(dead_code)]`
//!     or loses its exit-region call site.
//!  9. The authoritative rule file
//!     `.claude/rules/project/dhan-exit-order-lockout-2026-07-14.md`
//!     disappears or stops carrying the lock contract.
//! 10. The TOML section scanner itself regresses (self-test fixtures —
//!     incl. inline-table + dotted-key forms via the `toml`-crate doc
//!     parser, M6a 2026-07-14).
//! 11. A `dry_run = false` / `dry_run: false` literal appears in
//!     engine.rs outside the single `#[cfg(test)]`-gated
//!     `enable_live_mode` body (M6b).
//! 12. Any engine exit method grows a caller OUTSIDE the dispatcher hub
//!     (`crates/app/src/exit_execution.rs`) / engine.rs / test code —
//!     the S6-G1 only-caller pin (M6c).
//!
//! See:
//! - `.claude/rules/project/dhan-exit-order-lockout-2026-07-14.md`
//! - `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md` §2
//! - `.claude/rules/project/merge-gate-lock-2026-07-04.md` (All Green)
//! - `crates/storage/tests/groww_scale_aws_lockout_guard.rs` (the house
//!   lockout-guard template this mirrors)

#![cfg(test)]

use std::path::{Path, PathBuf};

use tickvault_common::config::ExitOrdersConfig;

const LOCK_MSG: &str = "operator lock 2026-07-14 — the DHAN exit-order layer ships default-off \
     behind four independent locks; see \
     .claude/rules/project/dhan-exit-order-lockout-2026-07-14.md";

const RULE_FILE: &str = ".claude/rules/project/dhan-exit-order-lockout-2026-07-14.md";

/// The six exit methods whose dry-run-before-token source order is pinned
/// (design Ruling 1: the `// LIVE-EXIT-ARM` marker + source-order pin).
const EXIT_METHODS: &[&str] = &[
    "place_super_order",
    "modify_super_order_leg",
    "cancel_super_order_leg",
    "place_forever_oco",
    "place_order_sliced",
    "verify_order_execution",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/trading parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Strip a trailing `#`-comment from a TOML line (good enough for the
/// simple `key = value` lines this guard inspects — none of the keys we
/// check carry `#` inside a string value).
fn strip_toml_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Returns `true` if `body` (TOML text) sets `enabled = true` inside an
/// `[exit_orders]` section. Tracks the current section header
/// line-by-line; ignores comments and other sections.
fn toml_enables_exit_orders(body: &str) -> bool {
    matches!(
        toml_exit_orders_enabled_value(body).as_deref(),
        Some("true")
    )
}

/// M6a (2026-07-14 hostile review): FULL `toml`-crate document parse —
/// catches the shapes the line scanner cannot (inline tables
/// `exit_orders = { enabled = true }` and top-level dotted keys
/// `exit_orders.enabled = true`). Returns `None` when the body is not
/// parseable TOML or the key is absent.
fn toml_doc_exit_orders_enabled(body: &str) -> Option<bool> {
    let doc: toml::Value = toml::from_str(body).ok()?;
    doc.get("exit_orders")?.get("enabled")?.as_bool()
}

/// Returns the raw `enabled` value inside `[exit_orders]`, if the section
/// and key exist (comment-stripped, section-scoped).
fn toml_exit_orders_enabled_value(body: &str) -> Option<String> {
    let mut in_section = false;
    let mut value: Option<String> = None;
    for raw_line in body.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_section = line == "[exit_orders]";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            if key.trim() == "enabled" {
                value = Some(val.trim().to_string());
            }
        }
    }
    value
}

/// Recursively collect every file under `dir`.
///
/// M6e (2026-07-14 hostile review): uses the NON-FOLLOWING
/// `DirEntry::file_type()` — a symlinked directory is never descended
/// into (cycle/escape defense; `Path::is_dir()` follows symlinks).
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {} failed: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry under {}: {e}", dir.display()));
        let file_type = entry
            .file_type()
            .unwrap_or_else(|e| panic!("file_type of {}: {e}", entry.path().display()));
        let path = entry.path();
        if file_type.is_dir() {
            walk_files(&path, out);
        } else if file_type.is_file() {
            out.push(path);
        }
        // Symlinks are deliberately skipped (never followed).
    }
}

/// Rebuild `body` with `//`-comment lines removed, EXCEPT lines carrying
/// `keep` (so the `// LIVE-EXIT-ARM` sentinel survives a comment strip).
/// Relative source order of the surviving lines is preserved, which is
/// all the source-order pins below need.
fn strip_line_comments_keeping(body: &str, keep: &str) -> String {
    body.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("//") && !l.contains(keep))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The engine exit-execution region: from the `// ==== EXIT EXECUTION`
/// marker to the first `#[cfg(test)]` after it (the test module). Panics
/// loudly if the marker moved — the guard must never scan a stale slice.
fn engine_exit_region(engine_body: &str) -> &str {
    let start = engine_body
        .find("==== EXIT EXECUTION (Cluster B) ====")
        .unwrap_or_else(|| {
            panic!("EXIT EXECUTION region marker missing from engine.rs — {LOCK_MSG}")
        });
    let region = &engine_body[start..];
    match region.find("#[cfg(test)]") {
        Some(end) => &region[..end],
        None => region,
    }
}

// ---------------------------------------------------------------------------
// 1 — no tracked config file may flip the exit-orders flag on.
// ---------------------------------------------------------------------------

/// The AWS deploy workflow copies `config/` from the repo clone to the
/// box, so a `true` here IS the activation vector (Lock #1).
#[test]
fn no_config_file_enables_exit_orders() {
    let config_dir = repo_root().join("config");
    let mut files = Vec::new();
    walk_files(&config_dir, &mut files);
    let mut saw_section = false;
    for path in files {
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let body = read(&path);
        if body.contains("[exit_orders]") {
            saw_section = true;
        }
        assert!(
            !toml_enables_exit_orders(&body),
            "{} sets enabled = true inside [exit_orders] — {LOCK_MSG}",
            path.display()
        );
        // M6a: full-document parse — catches inline-table / dotted-key
        // forms the line scanner cannot. Config files MUST parse (figment
        // loads them at boot), so a parse failure is itself a violation.
        let doc: toml::Value = toml::from_str(&body).unwrap_or_else(|e| {
            panic!(
                "{} is not parseable TOML ({e}) — the config loader would reject it. {LOCK_MSG}",
                path.display()
            )
        });
        let doc_enabled = doc
            .get("exit_orders")
            .and_then(|section| section.get("enabled"))
            .and_then(toml::Value::as_bool);
        assert_ne!(
            doc_enabled,
            Some(true),
            "{} enables exit_orders (doc-level parse — inline/dotted form?) — {LOCK_MSG}",
            path.display()
        );
    }
    // Non-vacuous scan surface: base.toml must CARRY the section with an
    // explicit enabled = false (groww-lockout pattern — a vanished section
    // means the guard is scanning nothing).
    assert!(
        saw_section,
        "no config/*.toml carries an [exit_orders] section — this guard's scan \
         surface changed; re-verify the exit-orders config location. {LOCK_MSG}"
    );
    let base = read(&repo_root().join("config/base.toml"));
    assert_eq!(
        toml_exit_orders_enabled_value(&base).as_deref(),
        Some("false"),
        "config/base.toml must carry [exit_orders] with an explicit \
         enabled = false — {LOCK_MSG}"
    );
}

// ---------------------------------------------------------------------------
// 2 — the compiled-in default for ExitOrdersConfig.enabled must stay false.
// ---------------------------------------------------------------------------

#[test]
fn exit_orders_config_default_is_off() {
    let body = read(&repo_root().join("crates/common/src/config.rs"));

    // (a) The struct block: the `enabled` field must carry the BARE
    // `#[serde(default)]` (bool::default() == false, never a
    // `default = "fn"` that could return true).
    let struct_start = body
        .find("pub struct ExitOrdersConfig")
        .unwrap_or_else(|| panic!("ExitOrdersConfig struct missing from config.rs — {LOCK_MSG}"));
    let struct_block = &body[struct_start..];
    let struct_block = &struct_block[..struct_block
        .find("\n}")
        .unwrap_or_else(|| panic!("ExitOrdersConfig struct block unterminated — {LOCK_MSG}"))];
    let enabled_pos = struct_block
        .find("pub enabled: bool")
        .unwrap_or_else(|| panic!("ExitOrdersConfig.enabled field missing — {LOCK_MSG}"));
    let before_enabled = &struct_block[..enabled_pos];
    let last_attr_line = before_enabled
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("#["))
        .unwrap_or_else(|| panic!("ExitOrdersConfig.enabled has no serde attribute — {LOCK_MSG}"));
    assert_eq!(
        last_attr_line, "#[serde(default)]",
        "ExitOrdersConfig.enabled must use the BARE #[serde(default)] (bool -> false); \
         found `{last_attr_line}` — {LOCK_MSG}"
    );

    // (b) The Default impl: `enabled: false` and never `enabled: true`.
    let default_start = body
        .find("impl Default for ExitOrdersConfig")
        .unwrap_or_else(|| panic!("Default impl for ExitOrdersConfig missing — {LOCK_MSG}"));
    let default_block = &body[default_start..];
    let default_block = &default_block[..default_block
        .find("\n}")
        .unwrap_or_else(|| panic!("ExitOrdersConfig Default impl unterminated — {LOCK_MSG}"))];
    assert!(
        default_block.contains("enabled: false"),
        "ExitOrdersConfig::default() must set enabled: false — {LOCK_MSG}"
    );
    assert!(
        !default_block.contains("enabled: true"),
        "ExitOrdersConfig::default() sets enabled: true — {LOCK_MSG}"
    );

    // (c) Real behavior, not just source shape: an absent/empty section
    // deserializes DISABLED (fail-safe), and Default::default() is off.
    assert!(
        !ExitOrdersConfig::default().enabled,
        "ExitOrdersConfig::default().enabled must be false — {LOCK_MSG}"
    );
    let from_empty: ExitOrdersConfig = toml::from_str("").unwrap_or_else(|e| {
        panic!("empty TOML must deserialize to a disabled ExitOrdersConfig: {e} — {LOCK_MSG}")
    });
    assert!(
        !from_empty.enabled,
        "empty [exit_orders] TOML must deserialize with enabled = false — {LOCK_MSG}"
    );
}

// ---------------------------------------------------------------------------
// 3 — the engine's dry_run hardcode + cfg(test)-only live setter (Lock #3).
// ---------------------------------------------------------------------------

#[test]
fn oms_dry_run_hardcoded_true() {
    let body = read(&repo_root().join("crates/trading/src/oms/engine.rs"));

    // (a) `dry_run: true,` inside OrderManagementSystem::new().
    let impl_start = body
        .find("impl OrderManagementSystem")
        .unwrap_or_else(|| panic!("impl OrderManagementSystem missing — {LOCK_MSG}"));
    let after_impl = &body[impl_start..];
    let new_start = after_impl
        .find("fn new(")
        .unwrap_or_else(|| panic!("OrderManagementSystem::new missing — {LOCK_MSG}"));
    let new_block = &after_impl[new_start..];
    let new_block = &new_block[..new_block
        .find("\n    }")
        .unwrap_or_else(|| panic!("OrderManagementSystem::new block unterminated — {LOCK_MSG}"))];
    assert!(
        new_block.contains("dry_run: true,"),
        "OrderManagementSystem::new() must hardcode dry_run: true — {LOCK_MSG}"
    );
    assert!(
        !new_block.contains("dry_run: false"),
        "OrderManagementSystem::new() sets dry_run: false — {LOCK_MSG}"
    );

    // (b) No live-mode setter outside #[cfg(test)]. `set_dry_run` must not
    // exist at all; every `fn enable_live*` must be immediately gated.
    assert!(
        !body.contains("fn set_dry_run"),
        "a set_dry_run setter appeared in engine.rs — {LOCK_MSG}"
    );
    let lines: Vec<&str> = body.lines().collect();
    let mut saw_enable_live = false;
    for (idx, line) in lines.iter().enumerate() {
        if !line.contains("fn enable_live") {
            continue;
        }
        saw_enable_live = true;
        let gated = lines[idx.saturating_sub(4)..idx]
            .iter()
            .any(|l| l.trim() == "#[cfg(test)]");
        assert!(
            gated,
            "line {}: `{}` is not immediately preceded by #[cfg(test)] — a \
             live-mode setter escaped the test gate. {LOCK_MSG}",
            idx + 1,
            line.trim()
        );
    }
    assert!(
        saw_enable_live,
        "enable_live_mode disappeared — this guard's scan surface changed; \
         re-verify the live-mode setter location. {LOCK_MSG}"
    );
}

// ---------------------------------------------------------------------------
// 4 — dry-run branch BEFORE the token fetch, per exit method (Lock #3b).
// ---------------------------------------------------------------------------

/// Source-order pin (house pattern): inside every exit method body the
/// `if self.dry_run` branch must appear BEFORE `get_access_token`, and
/// the `// LIVE-EXIT-ARM` sentinel must sit between them — so no future
/// refactor can fetch (and thereby exercise) the live token path before
/// the paper short-circuit.
#[test]
fn exit_methods_dry_run_before_token_fetch() {
    let body = read(&repo_root().join("crates/trading/src/oms/engine.rs"));
    let region = engine_exit_region(&body);

    // Method start offsets within the region, in source order.
    let mut starts: Vec<(usize, &str)> = EXIT_METHODS
        .iter()
        .map(|m| {
            let needle = format!("pub async fn {m}");
            let pos = region.find(&needle).unwrap_or_else(|| {
                panic!("exit method `{m}` missing from the EXIT EXECUTION region — {LOCK_MSG}")
            });
            (pos, *m)
        })
        .collect();
    starts.sort_unstable();

    for (i, (start, method)) in starts.iter().enumerate() {
        let end = if i + 1 < starts.len() {
            starts[i + 1].0
        } else {
            // Last method: terminate at the next non-async fn (the
            // `super_order` accessor / free helpers) or the region end.
            let tail = &region[*start..];
            *start
                + tail
                    .find("\n    pub fn ")
                    .or_else(|| tail.find("\npub(crate) fn "))
                    .unwrap_or(tail.len())
        };
        let slice = strip_line_comments_keeping(&region[*start..end], "LIVE-EXIT-ARM");
        let dry_pos = slice
            .find("if self.dry_run")
            .unwrap_or_else(|| panic!("`{method}` has no `if self.dry_run` branch — {LOCK_MSG}"));
        let marker_pos = slice
            .find("// LIVE-EXIT-ARM")
            .unwrap_or_else(|| panic!("`{method}` lost its // LIVE-EXIT-ARM marker — {LOCK_MSG}"));
        let token_pos = slice.find("get_access_token").unwrap_or_else(|| {
            panic!("`{method}` has no get_access_token call (live arm vanished?) — {LOCK_MSG}")
        });
        assert!(
            dry_pos < marker_pos && marker_pos < token_pos,
            "`{method}` broke the dry-run-before-token ladder \
             (dry_run@{dry_pos}, LIVE-EXIT-ARM@{marker_pos}, token@{token_pos}) — \
             the paper short-circuit must precede the live arm. {LOCK_MSG}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5 — the app dispatcher's config gate (Lock #2a).
// ---------------------------------------------------------------------------

#[test]
fn dispatcher_gate_literal_present() {
    let path = repo_root().join("crates/app/src/exit_execution.rs");
    assert!(
        path.exists(),
        "crates/app/src/exit_execution.rs missing — the dispatcher (the S6-G1 \
         call-site hub + Lock #2a gate) vanished. {LOCK_MSG}"
    );
    let body = read(&path);
    for needle in [
        "!cfg.enabled",
        "tv_exit_commands_dropped_total",
        "\"disabled\"",
    ] {
        assert!(
            body.contains(needle),
            "exit_execution.rs lost the dispatcher gate literal `{needle}` — every \
             ExitCommand must be dropped (counted, reason=disabled) while \
             [exit_orders] is off. {LOCK_MSG}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6 — nothing on the deploy path may set the exit-orders flag.
// ---------------------------------------------------------------------------

/// Closes the "env var / provisioning script / workflow flips the flag on
/// the box" vector (terraform, docker, systemd, workflows, Makefile).
#[test]
fn deploy_path_never_sets_exit_flag() {
    let root = repo_root();
    let mut files = Vec::new();
    walk_files(&root.join("deploy"), &mut files);
    walk_files(&root.join(".github/workflows"), &mut files);
    files.push(root.join("Makefile"));

    for path in files {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue; // binary / non-UTF-8 file — cannot carry a TOML/env flag textually
        };
        let lower = body.to_lowercase();
        for banned in [
            "exit_orders_enabled=true",
            "exit_orders_enabled = true",
            "exit_orders.enabled=true",
            "exit_orders.enabled = true",
            // M6d: inline-table forms.
            "exit_orders={enabled=true",
            "exit_orders = { enabled = true",
            "exit_orders = {enabled = true",
            "exit_orders={ enabled=true",
        ] {
            assert!(
                !lower.contains(banned),
                "{} contains `{banned}` — {LOCK_MSG}",
                path.display()
            );
        }
        if body.contains("[exit_orders]") {
            assert!(
                !toml_enables_exit_orders(&body),
                "{} carries an [exit_orders] section with enabled = true — {LOCK_MSG}",
                path.display()
            );
        }
        // M6a/M6d: when the file happens to be parseable TOML, the full
        // document parse also vets inline-table/dotted forms (most deploy
        // files are not TOML — those skip; the needle scan above still
        // applies to them).
        assert_ne!(
            toml_doc_exit_orders_enabled(&body),
            Some(true),
            "{} enables exit_orders (doc-level TOML parse) — {LOCK_MSG}",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// 7 — zero Telegram dispatch from the exit layer (2026-07-14 noise lock).
// ---------------------------------------------------------------------------

/// Two pins:
/// (a) the exit-layer source (engine exit region + exit_rules.rs +
///     exit_execution.rs) never touches `NotificationService` / `.notify(`
///     — EXIT-ORDER-01 / EXIT-VERIFY-01 stay log-sink-only;
/// (b) `NotificationEvent::OrderRejected` / `::CircuitBreakerOpened` keep
///     ZERO production dispatch sites (their feed_badge() arms are
///     rendering-only prep) — UNLESS the noise-lock rule gains an
///     `order execution` family row FIRST (the legitimate enable-time
///     path; see the rule file's enable-time protocol).
#[test]
fn exit_layer_emits_no_telegram_dispatch() {
    let root = repo_root();

    // (a) the exit-layer source is sink-free.
    let engine = read(&root.join("crates/trading/src/oms/engine.rs"));
    let sources = [
        (
            "engine.rs EXIT EXECUTION region",
            engine_exit_region(&engine).to_string(),
        ),
        (
            "exit_rules.rs",
            read(&root.join("crates/trading/src/oms/exit_rules.rs")),
        ),
        (
            "exit_execution.rs",
            read(&root.join("crates/app/src/exit_execution.rs")),
        ),
    ];
    for (name, body) in sources {
        let code = strip_line_comments_keeping(&body, "\u{0}");
        for banned in ["NotificationService", ".notify("] {
            assert!(
                !code.contains(banned),
                "{name} contains `{banned}` — the exit layer must stay Telegram-free \
                 (log-sink-only EXIT-ORDER-01 / EXIT-VERIFY-01). {LOCK_MSG}"
            );
        }
    }

    // (b) badge-arm prep stays rendering-only: zero dispatch sites for the
    // order-path variants anywhere in production src — OR the noise-lock
    // family row exists (the rule-edited enable path).
    let mut dispatch_sites: Vec<String> = Vec::new();
    let mut files = Vec::new();
    walk_files(&root.join("crates"), &mut files);
    for path in files {
        let p = path.to_string_lossy().replace('\\', "/");
        if !p.contains("/src/") || !p.ends_with(".rs") {
            continue;
        }
        // The notification module itself defines + formats + unit-tests the
        // variants — that is not a dispatch site.
        if p.contains("crates/core/src/notification/") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let code = strip_line_comments_keeping(&body, "\u{0}");
        for variant in [
            "NotificationEvent::OrderRejected",
            "NotificationEvent::CircuitBreakerOpened",
        ] {
            if code.contains(variant) {
                dispatch_sites.push(format!("{p}: {variant}"));
            }
        }
    }
    if !dispatch_sites.is_empty() {
        let noise_lock =
            read(&root.join(".claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md"));
        assert!(
            noise_lock.to_lowercase().contains("order execution"),
            "order-path NotificationEvent dispatch sites appeared without an \
             `order execution` family row in dhan-rest-only-noise-lock-2026-07-14.md §2 \
             (the noise-lock rule edit must land FIRST — enable-time protocol):\n  {}\n{LOCK_MSG}",
            dispatch_sites.join("\n  ")
        );
    }
}

// ---------------------------------------------------------------------------
// 8 — validate_super_order_prices is WIRED, not dead code.
// ---------------------------------------------------------------------------

#[test]
fn validate_super_order_prices_wired_not_dead_code() {
    let body = read(&repo_root().join("crates/trading/src/oms/engine.rs"));

    // (a) no #[allow(dead_code)] above the definition.
    let def_pos = body
        .find("fn validate_super_order_prices")
        .unwrap_or_else(|| panic!("validate_super_order_prices missing — {LOCK_MSG}"));
    let above: Vec<&str> = body[..def_pos].lines().rev().take(8).collect();
    assert!(
        !above.iter().any(|l| l.contains("dead_code")),
        "validate_super_order_prices regressed to #[allow(dead_code)] — it must \
         stay wired into the exit path. {LOCK_MSG}"
    );

    // (b) a real call site exists inside the EXIT EXECUTION region
    // (an occurrence of `validate_super_order_prices(` that is not the
    // `fn` definition, in comment-stripped code).
    let region = strip_line_comments_keeping(engine_exit_region(&body), "\u{0}");
    let mut call_sites = 0usize;
    let mut cursor = 0usize;
    while let Some(idx) = region[cursor..].find("validate_super_order_prices(") {
        let absolute = cursor + idx;
        let prefix = region[..absolute].trim_end();
        if !prefix.ends_with("fn") {
            call_sites += 1;
        }
        cursor = absolute + "validate_super_order_prices(".len();
    }
    assert!(
        call_sites >= 1,
        "no call site for validate_super_order_prices inside the EXIT EXECUTION \
         region — the price-ordering check came unwired. {LOCK_MSG}"
    );
}

// ---------------------------------------------------------------------------
// 9 — the authoritative rule file pins the contract.
// ---------------------------------------------------------------------------

#[test]
fn lockout_rule_file_pins_the_contract() {
    let path = repo_root().join(RULE_FILE);
    assert!(
        path.exists(),
        "authoritative rule file missing at {} — {LOCK_MSG}",
        path.display()
    );
    let body = read(&path);
    for required in [
        "2026-07-14",
        "coordinator-relayed",
        "four independent locks",
        "dhan_exit_order_lockout_guard",
        "dry_run: true",
        "enable-time protocol",
        "EXIT-ORDER-01",
        "EXIT-VERIFY-01",
        "ExitOrder01ExecutionDegraded",
        "ExitVerify01Degraded",
        "log-sink-only",
    ] {
        assert!(
            body.contains(required),
            "rule file lost required phrase `{required}` — {LOCK_MSG}"
        );
    }
}

// ---------------------------------------------------------------------------
// 11 — every dry_run-false literal sits inside the #[cfg(test)] gate (M6b).
// ---------------------------------------------------------------------------

/// Lock #3 companion: engine.rs may carry EXACTLY ONE
/// `dry_run = false` / `dry_run: false` literal (the `enable_live_mode`
/// body), and it must sit within a `#[cfg(test)]`-gated item — a second
/// literal (or an ungated one) is a live-mode escape hatch.
#[test]
fn dry_run_false_literal_only_in_cfg_test_gate() {
    let body = read(&repo_root().join("crates/trading/src/oms/engine.rs"));
    let code = strip_line_comments_keeping(&body, "\u{0}");
    let lines: Vec<&str> = code.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("dry_run = false") || line.contains("dry_run: false"))
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly ONE dry_run-false literal in engine.rs (the cfg(test) \
         enable_live_mode body); found {} — {LOCK_MSG}",
        hits.len()
    );
    for idx in hits {
        let gated = lines[idx.saturating_sub(6)..idx]
            .iter()
            .any(|l| l.trim() == "#[cfg(test)]");
        assert!(
            gated,
            "the dry_run-false literal at comment-stripped line {} is not within a \
             #[cfg(test)]-gated item — {LOCK_MSG}",
            idx + 1
        );
    }
}

// ---------------------------------------------------------------------------
// 12 — only-caller pin: the dispatcher hub is the sole exit-method caller
//      outside the engine + test code (M6c / S6-G1).
// ---------------------------------------------------------------------------

#[test]
fn exit_methods_only_called_from_dispatcher_and_engine() {
    let root = repo_root();
    let mut files = Vec::new();
    walk_files(&root.join("crates"), &mut files);

    let needles = [
        ".place_super_order(",
        ".modify_super_order_leg(",
        ".cancel_super_order_leg(",
        ".place_forever_oco(",
        ".place_order_sliced(",
        ".verify_order_execution(",
    ];
    let mut violations: Vec<String> = Vec::new();
    for path in files {
        let p = path.to_string_lossy().replace('\\', "/");
        if !p.contains("/src/") || !p.ends_with(".rs") {
            continue;
        }
        // The two legitimate homes: the dispatcher hub + the engine itself
        // (whose exit methods delegate to the api-client twins).
        if p.ends_with("crates/app/src/exit_execution.rs")
            || p.ends_with("crates/trading/src/oms/engine.rs")
        {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Production region only — truncate at the first #[cfg(test)]
        // (test code may exercise the seam freely). Conservative: an
        // early inline cfg(test) shrinks the scanned region, never
        // widens it.
        let prod = body.split("#[cfg(test)]").next().unwrap_or("");
        let code = strip_line_comments_keeping(prod, "\u{0}");
        for needle in needles {
            if code.contains(needle) {
                violations.push(format!("{p}: {needle}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "engine exit methods called outside the dispatcher hub (S6-G1 seam \
         violation):\n  {}\n{LOCK_MSG}",
        violations.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// 10 — scanner self-test (guards this guard against a vacuous parser).
// ---------------------------------------------------------------------------

#[test]
fn toml_scanner_detects_flipped_flag() {
    let flipped = "[feeds]\ndhan_enabled = false\n\n[exit_orders]\nenabled = true\n";
    assert!(
        toml_enables_exit_orders(flipped),
        "scanner must detect enabled = true"
    );
    let off = "[exit_orders]\nenabled = false\n# enabled = true (comment)\n";
    assert!(
        !toml_enables_exit_orders(off),
        "scanner must not false-positive on comments"
    );
    assert_eq!(
        toml_exit_orders_enabled_value(off).as_deref(),
        Some("false"),
        "value extractor must read the explicit false"
    );
    let other_section = "[feeds]\nenabled = true\n[exit_orders]\nenabled = false\n";
    assert!(
        !toml_enables_exit_orders(other_section),
        "scanner must scope to the [exit_orders] section only"
    );
    let similar_section = "[exit_orders_extra]\nenabled = true\n";
    assert!(
        !toml_enables_exit_orders(similar_section),
        "scanner must match the exact [exit_orders] header, not prefixes"
    );
    assert_eq!(
        toml_exit_orders_enabled_value("[exit_orders]\n# only comments\n"),
        None,
        "value extractor must return None when the key is absent"
    );

    // M6a self-test: the doc-level parser catches the forms the line
    // scanner cannot — inline tables and top-level dotted keys.
    let inline = "exit_orders = { enabled = true }\n";
    assert_eq!(
        toml_doc_exit_orders_enabled(inline),
        Some(true),
        "doc parser must detect the inline-table form"
    );
    let dotted = "exit_orders.enabled = true\n";
    assert_eq!(
        toml_doc_exit_orders_enabled(dotted),
        Some(true),
        "doc parser must detect the top-level dotted-key form"
    );
    let section = "[exit_orders]\nenabled = true\n";
    assert_eq!(
        toml_doc_exit_orders_enabled(section),
        Some(true),
        "doc parser must detect the section form"
    );
    let off_inline = "exit_orders = { enabled = false }\n";
    assert_eq!(toml_doc_exit_orders_enabled(off_inline), Some(false));
    assert_eq!(
        toml_doc_exit_orders_enabled("not = 'toml' ["),
        None,
        "unparsable input must be None (the needle scan covers non-TOML files)"
    );
    assert_eq!(
        toml_doc_exit_orders_enabled("[other]\nenabled = true\n"),
        None
    );
}
