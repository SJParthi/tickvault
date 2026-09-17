//! Build-failing wiring ratchet for the REST-era bar-fold candle
//! derivation (operator directive 2026-07-16 — FOLD-01 runbook:
//! `.claude/rules/project/rest-candle-fold-error-codes.md`).
//!
//! Pins, source-scan style (the `spot_1m_rest_wiring_guard.rs` house
//! pattern):
//! 1. main.rs spawns the fold in the SHARED-INFRA prefix — config-gated
//!    on `config.rest_candle_fold.enabled`, AFTER the seal-writer install
//!    (`spawn_seal_writer_loop`) so `global_seal_sender` exists before the
//!    first seal emission.
//! 2. BOTH spot legs hand off persist-confirmed bars — exactly TWO
//!    `send_confirmed_bars` call sites per leg (fire + sweep), each fed by
//!    `ConfirmedBar::from_minute_candle` staging at the append-ok arms.
//! 3. The fold module keeps its load-bearing pieces: the boot catch-up
//!    call, the global-sender install/read pair, and the FOLD-01 coded
//!    degrade emissions.
//!
//! A refactor that drops any of these compiles green without this guard
//! and silently starves the candles_* tables the operator demanded.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_source(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

/// Production region = everything above the FIRST column-0 `#[cfg(test)]`
/// line (the house source-scan convention — assertion literals in a test
/// module must never satisfy a production needle).
fn production_region(source: &str) -> &str {
    match source.find("\n#[cfg(test)]") {
        Some(idx) => &source[..idx],
        None => source,
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[test]
fn main_rs_spawns_fold_gated_after_seal_writer_install() {
    let main_rs = read_source("crates/app/src/main.rs");

    // M5 (hostile review 2026-07-16): anchor the ordering assertion on the
    // CALL SITE, not the `fn spawn_seal_writer_loop(` DEFINITION — the
    // definition sits above the boot spine, so matching it made the
    // ordering check vacuous (deleting the call still passed). Collect
    // every occurrence, drop the ones preceded by `fn ` (definitions),
    // and require at least one real call whose LAST occurrence precedes
    // the fold spawn.
    //
    // LOW-3 accepted residual (documented, not fixed): this is a textual
    // source scan — a refactor that moves the seal-writer call (or the
    // fold spawn) into a helper fn defined elsewhere in main.rs keeps the
    // needle text present but the source-order comparison then tracks the
    // helper's DEFINITION position, not its runtime call order. The house
    // source-scan convention accepts this class (same as every sibling
    // wiring guard); a call-graph-accurate check would need rustc.
    let needle = "spawn_seal_writer_loop(";
    let occurrences: Vec<usize> = main_rs.match_indices(needle).map(|(i, _)| i).collect();
    assert!(
        occurrences.len() >= 2,
        "main.rs must carry BOTH the spawn_seal_writer_loop definition and \
         at least one call site (found {})",
        occurrences.len()
    );
    let call_sites: Vec<usize> = occurrences
        .iter()
        .copied()
        .filter(|&idx| !main_rs[..idx].ends_with("fn "))
        .collect();
    assert!(
        !call_sites.is_empty(),
        "main.rs must CALL spawn_seal_writer_loop — the definition alone \
         installs no global seal sender"
    );
    let seal_writer_call_idx = *call_sites
        .iter()
        .max()
        .expect("non-empty call_sites has a max");

    // The `if` — the GATE itself, not any mention of the flag. Since
    // 2026-08-11 the live-feed lane also reads this flag (it refuses to run
    // while the fold is writing the same Dhan candles), and that read sits
    // earlier in main.rs; a bare-flag needle found it and made the
    // same-block distance check compare a gate against an unrelated line.
    let gate_idx = main_rs
        .find("if config.rest_candle_fold.enabled {")
        .expect("main.rs must gate the fold spawn on [rest_candle_fold] enabled");
    let sender_install_idx = main_rs
        .find("rest_candle_fold::set_global_fold_bar_sender(")
        .expect("main.rs must install the global fold-bar sender");
    let spawn_idx = main_rs
        .find("rest_candle_fold::spawn_supervised_rest_candle_fold(")
        .expect("main.rs must spawn the supervised fold task");

    assert!(
        seal_writer_call_idx < spawn_idx,
        "the seal-writer CALL must precede the fold task spawn — otherwise \
         the first fold emission hits the no_seal_sender drop arm \
         (FOLD-01 stage=seal_send) on every boot (call at \
         {seal_writer_call_idx}, spawn at {spawn_idx})"
    );
    assert!(
        gate_idx < sender_install_idx && sender_install_idx < spawn_idx,
        "the config gate must precede the sender install, which must precede \
         the spawn (gate at {gate_idx}, install at {sender_install_idx}, \
         spawn at {spawn_idx})"
    );
    assert!(
        spawn_idx.saturating_sub(gate_idx) < 4096,
        "the gate and the spawn must be the SAME block — a gate that drifted \
         away from the spawn no longer guards it"
    );
}

// ---- `dhan_spot_leg_hands_off_confirmed_bars_at_both_flush_ok_arms` is
// ---- RETIRED 2026-09-17 ----
//
// It pinned the PRODUCER side of the fold's live hand-off inside
// `spot_1m_rest_boot.rs`: `ConfirmedBar::from_minute_candle(` at exactly the
// 3 append-ok arms (fire own-minute, fire backfill, sweep) and
// `rest_candle_fold::send_confirmed_bars(&confirmed_bars)` at exactly the 2
// flush-ok arms. Those counts were the mechanism behind the module's
// persist-CONFIRMED contract: a bar could only fold AFTER its ILP ACK.
//
// That file is gone. The Dhan per-minute spot REST leg is one of the two
// classes the operator's SOCKETS-ONLY narrowing removed
// (`no-rest-except-live-feed-2026-06-27.md` §12.10), so `read_source` on it
// panics with a bare `No such file or directory (os error 2)`.
//
// ## ⚠ THE RESIDUAL THIS LEAVES, which is the reason to read this tombstone
//
// `rest_candle_fold::send_confirmed_bars` now has **ZERO production callers**
// — the fold's live-bar inlet has no producer at all. `set_global_fold_bar_sender`
// is still installed (`main.rs`, inside the `[rest_candle_fold] enabled`
// gate), and that gate is `false` in `config/base.toml`, so nothing runs
// today and nothing is broken today.
//
// It is NOT harmless if the gate is ever flipped: `send_confirmed_bars` with
// no sender is a DOCUMENTED no-op (`test_send_confirmed_bars_no_sender_is_noop`),
// so a fold turned on would install its channel, spawn its task, log a clean
// start, and receive nothing — for the whole session, with no error and no
// counter. That is the same producer-less-channel shape this branch already
// had to close for `mark_forward` (§12.9(e) / §12.10.5 #4), where the fix was
// an explicit `drop` so the already-written "no live producer" warn could
// fire. There is no equivalent arm here, because the fold's inlet is a
// first-wins `OnceLock` install rather than a channel whose closure is
// observable.
//
// Recorded rather than fixed in this test: the disposition of the fold itself
// is §12.10.7(g)'s open item ("removed or explicitly recorded as inert"), and
// deciding that is a change to the fold, not to its guard. What this test can
// honestly do is stop asserting a hand-off from a file that no longer exists,
// and say plainly what went unwatched when it did.
//
// The three tests AROUND this one are UNCHANGED and still bind — the fold
// module's own load-bearing pieces, its refusal to write `ticks`, and the
// main.rs spawn ordering. None of them reads the deleted leg.

#[test]
fn fold_module_keeps_load_bearing_pieces() {
    let src = read_source("crates/app/src/rest_candle_fold.rs");
    let prod = production_region(&src);

    // The boot catch-up must actually be invoked by the run loop.
    assert!(
        prod.contains("boot_catchup(&mut runtime, config.catchup_days, today).await"),
        "run_rest_candle_fold must invoke the boot catch-up with the \
         configured window"
    );
    // Seal emission goes through the EXISTING shared seal-writer channel.
    assert!(
        prod.contains("seal_writer_runner::global_seal_sender()"),
        "the fold must emit seals via the shared global_seal_sender — never \
         a parallel writer"
    );
    // The handoff sender is the OnceLock first-wins global.
    assert!(
        prod.contains("pub fn set_global_fold_bar_sender"),
        "the global fold-bar sender install must exist"
    );
    // Degrades are CODED (tag-guard discipline), never silent.
    assert!(
        prod.contains("ErrorCode::RestCandleFold01Degraded.code_str()"),
        "fold degrades must carry the FOLD-01 code field"
    );
    // Persist-confirmed contract: bars from an unflushed batch must never
    // fold — the hook sites call send_confirmed_bars only after the ACK,
    // and the module documents that contract verbatim.
    assert!(
        prod.contains("persist-CONFIRMED"),
        "the persist-confirmed handoff contract wording must stay in the \
         module (the hook-site reviews key off it)"
    );
}

#[test]
fn fold_module_never_writes_ticks() {
    // live-feed-purity rules 1-6: the fold derives candles from bars — it
    // must never reference the ticks writer surface.
    let src = read_source("crates/app/src/rest_candle_fold.rs");
    for banned in [
        "TickPersistenceWriter",
        "append_tick(",
        "synthesize_ticks",
        "ParsedTick",
    ] {
        assert!(
            !src.contains(banned),
            "rest_candle_fold must never touch the ticks surface: found {banned}"
        );
    }
}
