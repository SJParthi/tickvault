//! UTC-SAFETY (daily-universe §28.7) wiring guard: source scans pinning where
//! the trade-safety switch is set and read, so a refactor cannot quietly move
//! a gate past its exit guard, drop the fail-closed seed, or add a second
//! page line. Each rule was bite-tested by breaking it once.

const TRADE_SAFETY: &str = include_str!("../src/trade_safety.rs");
const PAGE: &str = include_str!("../src/dhan_feed_stack/feed_time_page.rs");
const CHAIN: &str = include_str!("../src/live_fill/chain.rs");
const LIVE_FILL_MOD: &str = include_str!("../src/live_fill/mod.rs");
const RUNTIME: &str = include_str!("../src/order_runtime.rs");
const RUNTIME_ARM: &str = include_str!("../src/order_runtime_live_fill.rs");
const MAIN: &str = include_str!("../src/main.rs");
const FEED_STACK: &str = include_str!("../src/dhan_feed_stack.rs");

/// The production half of a file (everything before its first test module).
fn prod(src: &str) -> &str {
    src.split("#[cfg(test)]\nmod tests").next().unwrap_or(src)
}

/// The body of the first function whose signature contains `sig`, up to its
/// closing `\n    }\n` (methods) or `\n}\n` (free functions).
fn body<'a>(src: &'a str, sig: &str, end: &str) -> &'a str {
    let at = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[at..];
    &rest[..rest.find(end).unwrap_or(rest.len())]
}

#[test]
fn fire_engages_before_the_page_and_the_hand_off() {
    let fire = body(prod(PAGE), "fn fire(&mut self", "\n    }\n");
    let engage = fire.find("self.safety.engage(day, bit);").expect("engage");
    let page = fire.find("error!(").expect("page");
    let hand = fire.find("self.hand_off(").expect("hand_off");
    assert!(engage < page && page < hand, "{fire}");
    assert_eq!(fire.matches("error!(").count(), 1, "one page line only");
}

#[test]
fn the_switch_is_seeded_at_boot_and_by_restore_latch_never_by_the_chain_or_replay() {
    let boot = body(
        LIVE_FILL_MOD,
        "pub fn boot(config: &ApplicationConfig)",
        "\n}\n",
    );
    assert!(
        boot.contains("crate::trade_safety::seed_from_markers("),
        "{boot}"
    );
    assert!(boot.contains("crate::trade_safety::register_metrics();"));
    let restore = body(PAGE, "fn restore_latch_in(", "\n}\n");
    assert!(restore.contains("seed_from_markers_in("), "{restore}");
    // `start` and the scan's day change both seed through `restore_latch`
    // with the page's own switch.
    let page = prod(PAGE);
    assert!(page.contains("let restored = restore_latch(safety, day);"));
    assert_eq!(page.matches("restore_latch(self.safety, day)").count(), 1);
    assert!(page.contains("Self::with_writer(day, restored, persist, safety)"));
    // The chain never reads markers itself: the offline replay builds chains
    // with `new` and must never read the box's `data/state`.
    let new = body(CHAIN, "pub fn new(\n        mut engine: E,", "\n    }\n");
    assert!(!new.contains("seed_from_markers"), "{new}");
    assert!(!CHAIN.contains("seed_from_markers"));
    let start = body(LIVE_FILL_MOD, "pub fn start_chain", "\n}\n");
    assert!(!start.contains("seed_from_markers"), "{start}");
    for f in walk("src/live_fill/replay") {
        let src = std::fs::read_to_string(&f).expect("read replay file");
        assert!(!src.contains("trade_safety"), "{f:?} reads the switch");
    }
}

#[test]
fn every_gate_sits_inside_its_entry_guard_and_before_its_work() {
    // Gate A: first order refusal in decide, before the taint scan.
    let decide = body(prod(CHAIN), "fn decide(&mut self", "\n    }\n");
    let gate_a = decide
        .find("if intent.kind != IntentKind::Exit && self.safety.blocked_on(self.day).is_some()")
        .expect("gate A");
    assert!(
        gate_a
            < decide
                .find("self\n            .taints")
                .expect("taint scan")
    );
    // Gate B: first statement of Filled, before the outbox check.
    let filled = &prod(CHAIN)[prod(CHAIN).find("Finished::Filled {").expect("Filled")..];
    let gate_b = filled
        .find("if !order.meta.exit && self.safety.blocked_on(self.day).is_some()")
        .expect("gate B");
    assert!(gate_b < filled.find("self.outbox_cap").expect("outbox"));
    assert!(gate_b < filled.find("self.net_shares.get_mut").expect("net_shares"));
    // Gate C: first in the Execute arm.
    let arm = &prod(RUNTIME_ARM)[prod(RUNTIME_ARM)
        .find("PaperCmd::Execute(x) => {")
        .expect("arm")..];
    let gate_c = arm
        .find("if !x.exit && entry_blocked_by_feed_time(ctx.safety, &x)")
        .expect("gate C");
    assert!(gate_c < arm.find("waits_for_same_instrument(").expect("wait"));
    assert!(gate_c < arm.find("queue_exit_retry(").expect("queue"));
    assert!(gate_c < arm.find("retry_live_fill_exits(").expect("retry"));
    let refuse = body(prod(RUNTIME_ARM), "fn refuse_feed_time_entry(", "\n}\n");
    assert!(
        refuse.contains("report_position(risk, ctx, x);"),
        "{refuse}"
    );
    // Gate D: before the no-marks skip, and in the mark arm before the entry.
    let timers = body(prod(RUNTIME), "async fn drive_self_test_timers(", "\n}\n");
    let gate_d = timers
        .find("if ctx.safety.blocked_on(today).is_some()")
        .expect("gate D");
    assert!(
        gate_d
            > timers
                .find("!ctx.calendar.is_trading_day_today()")
                .expect("base gates")
    );
    assert!(gate_d < timers.find("if !marks_available {").expect("marks"));
    assert_eq!(
        prod(RUNTIME)
            .matches("self_test.skip_with(today, SKIP_OUTCOME_FEED_TIME, SKIP_TEXT_FEED_TIME)")
            .count(),
        2,
        "both gate-D halves skip, never fail"
    );
    let mark = body(prod(RUNTIME), "async fn process_mark(", "\n}\n");
    let gate_d2 = mark
        .find("if ctx.safety.blocked_on(today).is_some()")
        .expect("gate D2");
    assert!(gate_d2 < mark.find("start_self_test_entry(").expect("entry"));
}

#[test]
fn the_switch_never_pages_or_halts() {
    let p = prod(TRADE_SAFETY);
    assert!(
        !p.contains("error!("),
        "a second DHAN-FEED-TIME-01 error would page twice"
    );
    assert!(
        !p.contains("trigger_halt"),
        "§28.7: never the risk halt (it blocks exits)"
    );
    for src in [CHAIN, RUNTIME, RUNTIME_ARM] {
        let gates: Vec<_> = src.match_indices("blocked_on(").collect();
        assert!(!gates.is_empty());
    }
    assert!(!prod(RUNTIME).contains("trade_safety::SWITCH.engage"));
}

#[test]
fn production_reads_the_process_switch_and_main_and_the_feed_stack_stay_untouched() {
    let p = prod(TRADE_SAFETY);
    let real = body(
        p,
        "#[cfg(not(test))]\n#[must_use]\npub(crate) fn default_switch()",
        "\n}\n",
    );
    assert!(real.contains("&SWITCH"), "{real}");
    assert!(
        !MAIN.contains("trade_safety"),
        "main.rs has no line budget for it"
    );
    assert!(
        !FEED_STACK.contains("trade_safety"),
        "dhan_feed_stack.rs is size-pinned"
    );
}

fn walk(rel: &str) -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    assert!(!out.is_empty(), "replay folder moved: update the guard");
    out
}
