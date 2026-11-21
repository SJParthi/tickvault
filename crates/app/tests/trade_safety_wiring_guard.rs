//! Wiring guard for the trade-safety switch (plan item UTC-SAFETY,
//! daily-universe §28.7).
//!
//! Behaviour tests pin what each gate does (`trade_safety_tests.rs`, the
//! chain, runtime, self-test and feed-time page tests). This source scan pins
//! the ORDER of statements a behaviour test cannot see in production:
//!
//! 1. `FeedTimePage::fire` engages the switch BEFORE its `error!` and before
//!    the hand-off to the latch writer, so a failed hand-off still blocks.
//! 2. Gate A (`LiveFillChain::decide`) sits behind `!= IntentKind::Exit` and
//!    before the order is built (`marketable_limit`).
//! 3. Gate B (`on_finished`) is an arm guarded on `!order.meta.exit`, before
//!    the arm that queues the `PaperCmd::Execute`, beside the arm for entries
//!    cut at the switch (`Finished::Blocked`); the cut itself
//!    (`block_entries`) runs in `on_pass_bounded` before the release and the
//!    ready-collection (2026-11-21 review finding 3).
//! 4. Gate C (`execute`) sits behind `!x.exit` and before `place_order`.
//! 5. Gate D (`start_self_test_entry`) sits before `place_order`.
//! 6. The chain start reads the markers before it builds the chain.
//! 7. Production reads the PROCESS switch (chain and runtime context).
//! 8. `FeedTimePage::start` and its day change re-engage from the markers
//!    (STRICT reader, never the fail-open latch reader) and the day's
//!    `.hold`; the day change's re-read runs on the writer thread.
//! 9. The switch's own log is never `error!` and never `DHAN-FEED-TIME-01`
//!    (a second error would page again, noise lock §2.24).
//! 10. The drain-side page code (`fire`, `scan`, `on_day_change`,
//!     `offer_pending`) pays only the switch's one store: no
//!     `trade_safety::engage`, no `report_engaged`, no `publish_gauge`, no
//!     strict marker read (§28.7; 2026-11-21 review finding 1). The writer
//!     thread (`run_persist`) does the report and the day change's re-read.
//!
//! Bite-tested 2026-11-21: each ordering was broken by hand and the matching
//! test failed, then restored.

use std::path::PathBuf;

fn src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The production half of a file (before its first `#[cfg(test)]` module).
fn prod(text: &str) -> String {
    let cut = text.find("\n#[cfg(test)]\nmod ").unwrap_or(text.len());
    code_only(&text[..cut])
}

/// Drops `//` comments (line and trailing), keeping strings intact enough for
/// these scans (no scanned body puts `//` inside a string).
fn code_only(text: &str) -> String {
    text.lines()
        .map(|l| match l.find("//") {
            Some(i) if !l[..i].contains('"') => &l[..i],
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of the first item whose text starts with `sig`, brace-matched.
fn body<'a>(text: &'a str, sig: &str) -> &'a str {
    let start = text
        .find(sig)
        .unwrap_or_else(|| panic!("`{sig}` not found"));
    let open = start + text[start..].find('{').expect("opening brace");
    let mut depth = 0i32;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open..=open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced body for `{sig}`")
}

fn fire_body(text: &str) -> String {
    body(&prod(text), "fn fire(&mut self, day: i64, cause: Cause)").to_owned()
}

fn check_fire(text: &str) -> Result<(), String> {
    let b = fire_body(text);
    let engage = b
        .find("self.safety.engage(day, cause.safety_bit())")
        .ok_or("fire does not engage the switch")?;
    let err = b.find("error!(").ok_or("fire has no error!")?;
    let send = b.find("try_send(").ok_or("fire has no hand-off")?;
    if engage < err && engage < send {
        Ok(())
    } else {
        Err(format!("fire must engage first:\n{b}"))
    }
}

fn check_gate_a(text: &str) -> Result<(), String> {
    let p = prod(text);
    let b = body(
        &p,
        "fn decide(&mut self, intent: &OrderIntent, bar: &ClosedBar)",
    );
    let gate = b
        .find("if intent.kind != IntentKind::Exit && self.entries_blocked()")
        .ok_or("Gate A (entries only) missing from decide")?;
    let reason = b[gate..]
        .find("REASON_FEED_TIME_ALARM")
        .ok_or("Gate A does not refuse with REASON_FEED_TIME_ALARM")?;
    let build = b
        .find("marketable_limit(")
        .ok_or("decide builds no order")?;
    if gate + reason < build {
        Ok(())
    } else {
        Err("Gate A must refuse before the order is built".to_owned())
    }
}

fn check_gate_b(text: &str) -> Result<(), String> {
    let p = prod(text);
    let b = body(&p, "fn on_finished(&mut self, done: Finished<OrderMeta>)");
    let gate = b
        .find("Finished::Filled { order, summary, .. }\n                if !order.meta.exit && self.entries_blocked()")
        .ok_or("Gate B (entries only) missing from on_finished")?;
    if !b[gate..].contains("Finished::Blocked { order }") {
        return Err("the cut entries' arm (Finished::Blocked) is missing".to_owned());
    }
    let exec = b
        .find("PaperCmd::Execute(")
        .ok_or("on_finished queues no Execute")?;
    if gate < exec {
        Ok(())
    } else {
        Err("Gate B must come before the Execute arm".to_owned())
    }
}

fn check_entry_cut(text: &str) -> Result<(), String> {
    let p = prod(text);
    let b = body(&p, "pub fn on_pass_bounded(");
    let cut = b
        .find("self.pending.block_entries()")
        .ok_or("on_pass_bounded does not cut held entries")?;
    let gate = b
        .find("self.entries_cut_day != self.day && self.entries_blocked()")
        .ok_or("the cut is not gated on the switch for the chain's day")?;
    let release = b.find("release_due(").ok_or("no release")?;
    let collect = b.find("collect_ready(").ok_or("no collect")?;
    if gate < cut && cut < release && cut < collect {
        Ok(())
    } else {
        Err("the cut must come before the release and the collection".to_owned())
    }
}

fn check_gate_c(text: &str) -> Result<(), String> {
    let p = prod(text);
    let b = body(&p, "async fn execute(");
    let gate = b
        .find("if !x.exit\n        && ctx\n            .safety\n            .entries_blocked_on(")
        .ok_or("Gate C (entries only) missing from execute")?;
    let place = b
        .find("oms.place_order(")
        .ok_or("execute places no order")?;
    if gate < place && b[gate..place].contains("FEED_TIME_ALARM_REJECT") {
        Ok(())
    } else {
        Err("Gate C must reject before place_order".to_owned())
    }
}

fn check_gate_d(text: &str) -> Result<(), String> {
    let p = prod(text);
    let b = body(&p, "async fn start_self_test_entry(");
    let gate = b
        .find("safety.entries_blocked_on(today).is_some()")
        .ok_or("Gate D missing from start_self_test_entry")?;
    let place = b
        .find("oms.place_order(")
        .ok_or("no self-test entry placed")?;
    if gate < place && b[gate..place].contains("SELF_TEST_SKIPPED_FEED_TIME_ALARM") {
        Ok(())
    } else {
        Err("Gate D must skip before place_order".to_owned())
    }
}

fn check_chain_start(text: &str) -> Result<(), String> {
    let p = prod(text);
    let b = body(&p, "pub fn start_chain<");
    let read = b
        .find("engage_from_feed_time_markers(")
        .ok_or("start_chain does not read the markers")?;
    let build = b.find("LiveFillChain::new(").ok_or("no chain built")?;
    if read < build
        && b[read..build].contains("crate::daily_task_marker::marker_state")
        && b[read..build].contains("engage_from_feed_time_hold(")
        && b[read..build].contains("crate::trade_safety::process_feed_time_hold_bits")
    {
        Ok(())
    } else {
        Err("start_chain must read the markers (strict reader) before building".to_owned())
    }
}

#[test]
fn feed_time_page_fire_engages_before_the_error_and_the_hand_off() {
    let text = src("src/dhan_feed_stack/feed_time_page.rs");
    check_fire(&text).unwrap_or_else(|e| panic!("{e}"));
    // Bite: an engage moved after the error must fail.
    let broken = text.replacen(
        "let changed = self.safety.engage(day, cause.safety_bit());",
        "let changed = false;",
        1,
    );
    let broken = broken.replacen(
        "let sent = self",
        "let _again = self.safety.engage(day, cause.safety_bit());\n        let sent = self",
        1,
    );
    assert!(check_fire(&broken).is_err(), "the scan must bite");
    // The start and the day-change branch re-engage from the markers with the
    // STRICT reader (fail closed, §28.7), never through the fail-open latch
    // reader (2026-11-21 review finding 1).
    let p = prod(&text);
    check_page_reengage(&p).unwrap_or_else(|e| panic!("{e}"));
    let broken = p.replacen(
        "crate::daily_task_marker::marker_state",
        "|t, d| if crate::daily_task_marker::daily_marker_exists(t, d) { crate::daily_task_marker::MarkerState::Present } else { crate::daily_task_marker::MarkerState::Absent }",
        1,
    );
    assert!(check_page_reengage(&broken).is_err(), "the scan must bite");
}

fn check_page_reengage(p: &str) -> Result<(), String> {
    if p.contains("engage_restored(") {
        return Err("the fail-open restore engage is back".to_owned());
    }
    let start = body(p, "pub(super) fn start() -> Self");
    let with = body(p, "fn start_with(");
    let persist = body(p, "fn run_persist(");
    let for_day = body(p, "fn engage_for_day(");
    let ok = start.contains("crate::daily_task_marker::marker_state,")
        && start.contains("crate::trade_safety::process_feed_time_hold_bits,")
        && with.contains("engage_for_day(safety, day, state_of, hold_of)")
        && with.find("engage_for_day(") < with.find("restore_latch(")
        && persist.contains("crate::daily_task_marker::marker_state,")
        && persist.contains("crate::trade_safety::process_feed_time_hold_bits,")
        && for_day.contains("engage_from_feed_time_markers(")
        && for_day.contains("engage_from_feed_time_hold(");
    if ok {
        Ok(())
    } else {
        Err(
            "start and the writer's day change must engage from the strict markers and the hold"
                .to_owned(),
        )
    }
}

/// Drain-side page code pays only the switch's one store (§28.7).
fn check_drain_side(p: &str) -> Result<(), String> {
    const BANNED: [&str; 5] = [
        "trade_safety::engage(",
        "report_engaged(",
        "publish_gauge(",
        "marker_state",
        "engage_for_day(",
    ];
    for sig in [
        "fn fire(&mut self, day: i64, cause: Cause)",
        "pub(super) fn scan(&mut self, ingest: &mut LiveIngest)",
        "fn on_day_change(&mut self, day: i64)",
        "fn offer_pending(&mut self)",
    ] {
        let b = body(p, sig);
        if let Some(hit) = BANNED.iter().find(|x| b.contains(**x)) {
            return Err(format!("`{sig}` calls `{hit}` on the frame drain"));
        }
    }
    let fired = body(p, "fn persist_fired(");
    if !fired.contains("crate::trade_safety::report_engaged(safety, day)") {
        return Err("the writer thread does not report the engage".to_owned());
    }
    let change = body(p, "fn on_day_change(&mut self, day: i64)");
    if !change.contains("PersistMsg::NewDay { day }") {
        return Err("the day change does not hand the re-read to the writer".to_owned());
    }
    Ok(())
}

#[test]
fn feed_time_page_drain_side_pays_only_the_store() {
    let p = prod(&src("src/dhan_feed_stack/feed_time_page.rs"));
    check_drain_side(&p).unwrap_or_else(|e| panic!("{e}"));
    // Bite: the report put back on the drain must fail.
    let broken = p.replacen(
        "let changed = self.safety.engage(day, cause.safety_bit());",
        "let changed = crate::trade_safety::engage(self.safety, day, cause.safety_bit());",
        1,
    );
    assert!(check_drain_side(&broken).is_err(), "the scan must bite");
    // Bite: a strict re-read put back on the drain's day change must fail.
    let broken = p.replacen(
        "self.restored = restore_latch(day);",
        "self.restored = restore_latch(day);\n        let _g = engage_for_day(self.safety, day, crate::daily_task_marker::marker_state, |_| 0);",
        1,
    );
    assert!(check_drain_side(&broken).is_err(), "the scan must bite");
}

#[test]
fn gate_a_and_gate_b_guard_entries_only_and_come_first() {
    let text = src("src/live_fill/chain.rs");
    check_gate_a(&text).unwrap_or_else(|e| panic!("{e}"));
    check_gate_b(&text).unwrap_or_else(|e| panic!("{e}"));
    let broken_a = text.replacen(
        "if intent.kind != IntentKind::Exit && self.entries_blocked()",
        "if self.entries_blocked()",
        1,
    );
    assert!(check_gate_a(&broken_a).is_err(), "Gate A scan must bite");
    let broken_b = text.replacen(
        "if !order.meta.exit && self.entries_blocked() =>",
        "if self.entries_blocked() =>",
        1,
    );
    assert!(check_gate_b(&broken_b).is_err(), "Gate B scan must bite");
    // The on-engage cut runs in the pass BEFORE the release and the
    // ready-collection (2026-11-21 review finding 3).
    check_entry_cut(&text).unwrap_or_else(|e| panic!("{e}"));
    let broken_cut = text.replacen("let _cut = self.pending.block_entries();", "", 1);
    assert!(
        check_entry_cut(&broken_cut).is_err(),
        "the cut scan must bite"
    );
    // Production reads the process switch.
    assert!(prod(&text).contains("safety: &crate::trade_safety::FEED_TIME_BLOCK,"));
}

#[test]
fn gate_c_and_gate_d_refuse_before_any_order() {
    let arm = src("src/order_runtime_live_fill.rs");
    check_gate_c(&arm).unwrap_or_else(|e| panic!("{e}"));
    let broken_c = arm.replacen("if !x.exit\n        && ctx", "if true\n        && ctx", 1);
    assert!(check_gate_c(&broken_c).is_err(), "Gate C scan must bite");
    let rt = src("src/order_runtime.rs");
    check_gate_d(&rt).unwrap_or_else(|e| panic!("{e}"));
    let broken_d = rt.replacen("safety.entries_blocked_on(today).is_some()", "false", 1);
    assert!(check_gate_d(&broken_d).is_err(), "Gate D scan must bite");
    // The production runtime context reads the process switch.
    assert!(prod(&rt).contains("safety: &crate::trade_safety::FEED_TIME_BLOCK,"));
}

#[test]
fn chain_start_reads_the_markers_before_building() {
    let text = src("src/live_fill/mod.rs");
    check_chain_start(&text).unwrap_or_else(|e| panic!("{e}"));
    let broken = text.replacen(
        "crate::daily_task_marker::marker_state,",
        "|_, _| crate::daily_task_marker::MarkerState::Absent,",
        1,
    );
    assert!(check_chain_start(&broken).is_err(), "the scan must bite");
    // Bite: the hold read removed (2026-11-21 review finding 2).
    let broken = text.replacen(
        "crate::trade_safety::process_feed_time_hold_bits,",
        "|_| 0,",
        1,
    );
    assert!(
        check_chain_start(&broken).is_err(),
        "the hold scan must bite"
    );
}

#[test]
fn the_switch_never_logs_an_error_or_the_page_code() {
    let text = src("src/trade_safety.rs");
    let p = prod(&text);
    assert!(!p.contains("error!("), "the switch's own log must be warn!");
    assert!(!p.contains("DhanFeedTime01"), "never the page code");
    assert!(p.contains("warn!("));
    assert!(p.contains("source = \"trade_safety_entries_blocked\""));
}
