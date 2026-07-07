//! ADVERSARIAL REFUTER round 3, lens 1 — the 2026-07-07 storm construction.
//!
//! Claim under attack: "one incident = one bubble". Construction: a WS
//! disconnect+reconnect pair every minute for 30 minutes (the observed
//! Dhan RST-storm shape), with the drain ticker firing every 10s, driven
//! through a FAITHFUL simulation of the `dispatch_episode_event` /
//! `run_episode_tick` shell over the real pure registry + FSM + renderers.
//!
//! The harness counts every NETWORK-VISIBLE Telegram action:
//!   - `NewBubble` (sendMessage — a fresh chat bubble)
//!   - `Edit`      (editMessageText — in-place, no push notification)
//! and every SNS-SMS leg, exactly where the shell fires them.

#![allow(clippy::arithmetic_side_effects)] // APPROVED: test code

use tickvault_core::notification::episode::{
    EpisodeAction, EpisodeConfig, EpisodeFamily, EpisodeKey, EpisodePhase, EpisodeRegistry,
    EpisodeRenderCtx, EpisodeRole, fnv1a_hash, render_episode_recovered, render_episode_recovering,
    render_episode_steady,
};
use tickvault_core::notification::events::Severity;

const T0: u64 = 1_783_485_120_000; // 2026-07-07 10:02 AM IST, epoch ms
const SEC: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Net {
    NewBubble { id: i64, sms: bool },
    Edit { id: i64, green_close: bool },
    LegacySend,
}

/// Faithful shell model: mirrors `dispatch_episode_event`'s bookkeeping
/// (record_sent / record_edit_applied / hash-skip) and `run_episode_tick`'s
/// close/expire edit loop. Transport always succeeds (happy-path storm).
struct Shell {
    reg: EpisodeRegistry,
    cfg: EpisodeConfig,
    next_id: i64,
    log: Vec<Net>,
    ignored_high: u32,
}

impl Shell {
    fn new() -> Self {
        Self {
            reg: EpisodeRegistry::new(),
            cfg: EpisodeConfig::default(),
            next_id: 100,
            log: Vec::new(),
            ignored_high: 0,
        }
    }

    fn key() -> EpisodeKey {
        EpisodeKey {
            family: EpisodeFamily::MainFeedWs,
            conn: 0,
        }
    }

    fn event(&mut self, role: EpisodeRole, sev: Severity, attempts: u32, now_ms: u64) {
        let d = self
            .reg
            .apply_event(Self::key(), role, sev, attempts, now_ms, &self.cfg);
        match d.action {
            EpisodeAction::SendFirstPage | EpisodeAction::SendNewFallback => {
                let id = self.next_id;
                self.next_id += 1;
                self.log.push(Net::NewBubble {
                    id,
                    sms: matches!(d.action, EpisodeAction::SendFirstPage) && sev >= Severity::High,
                });
                self.reg.record_sent(Self::key(), Some(id), now_ms);
            }
            EpisodeAction::Edit { message_id, .. } => {
                let ctx = EpisodeRenderCtx { now_ms };
                let rendered = match d.state.phase {
                    EpisodePhase::Recovering => render_episode_recovering(&d.state, &ctx),
                    EpisodePhase::Down => render_episode_steady(&d.state, &ctx),
                };
                let hash = fnv1a_hash(&rendered);
                if hash == self.reg.last_render_hash(Self::key()) {
                    return; // shell's no-op-edit skip
                }
                self.log.push(Net::Edit {
                    id: message_id,
                    green_close: false,
                });
                self.reg.record_edit_applied(Self::key(), now_ms, hash);
            }
            EpisodeAction::EditThrottled => {} // folded, counters advance
            EpisodeAction::SendLegacy => self.log.push(Net::LegacySend),
            EpisodeAction::Ignore => {
                if sev >= Severity::High
                    && matches!(role, EpisodeRole::Open | EpisodeRole::Progress)
                {
                    self.ignored_high += 1;
                }
            }
        }
    }

    /// Mirrors `run_episode_tick`.
    fn tick(&mut self, now_ms: u64) {
        let outcome = self.reg.tick(now_ms, &self.cfg);
        for st in &outcome.closed {
            if let Some(id) = st.message_id {
                let ctx = EpisodeRenderCtx { now_ms };
                let _green = render_episode_recovered(st, &ctx);
                self.log.push(Net::Edit {
                    id,
                    green_close: true,
                });
            }
        }
        for st in &outcome.expired {
            if let Some(id) = st.message_id {
                self.log.push(Net::Edit {
                    id,
                    green_close: false,
                });
            }
        }
    }

    fn new_bubbles(&self) -> Vec<&Net> {
        self.log
            .iter()
            .filter(|n| matches!(n, Net::NewBubble { .. }))
            .collect()
    }

    fn green_closes(&self) -> usize {
        self.log
            .iter()
            .filter(|n| {
                matches!(
                    n,
                    Net::Edit {
                        green_close: true,
                        ..
                    }
                )
            })
            .count()
    }
}

/// Drives events + the 10s ticker in strict timestamp order.
fn run_storm(shell: &mut Shell, events: &[(u64, EpisodeRole, Severity, u32)], end_ms: u64) {
    let mut tick_at = T0 + 10 * SEC;
    let mut idx = 0;
    let mut now = T0;
    while now <= end_ms {
        // Fire all events due before the next tick.
        while idx < events.len() && events[idx].0 <= tick_at {
            let (t, role, sev, att) = events[idx];
            shell.event(role, sev, att, t);
            idx += 1;
        }
        shell.tick(tick_at);
        now = tick_at;
        tick_at += 10 * SEC;
    }
}

/// THE storm: disconnect at t=60k, reconnect at t=60k+5, k=0..30 (30 min),
/// then 5 minutes of quiet for the stability close.
#[test]
fn refute_storm_30min_pairs_yields_one_bubble_one_green_close() {
    let mut shell = Shell::new();
    let mut events = Vec::new();
    for k in 0..30_u64 {
        events.push((
            T0 + k * 60 * SEC,
            EpisodeRole::Open,
            Severity::High, // in-market WebSocketDisconnected
            0,
        ));
        events.push((
            T0 + k * 60 * SEC + 5 * SEC,
            EpisodeRole::Resolve,
            Severity::Medium, // WebSocketReconnected
            k as u32 + 1,
        ));
    }
    let end = T0 + 35 * 60 * SEC;
    run_storm(&mut shell, &events, end);

    let bubbles = shell.new_bubbles();
    assert_eq!(
        bubbles.len(),
        1,
        "storm must produce exactly ONE bubble, got {:?}",
        shell.log
    );
    assert!(
        matches!(bubbles[0], Net::NewBubble { sms: true, .. }),
        "HIGH first page must carry the SNS-SMS leg"
    );
    assert_eq!(shell.green_closes(), 1, "exactly one green close");
    assert_eq!(shell.ignored_high, 0, "no High event ignored");
    assert!(
        shell.log.iter().all(|n| !matches!(n, Net::LegacySend)),
        "no legacy leak"
    );
    // Every edit targets the ONE bubble id.
    let first_id = match bubbles[0] {
        Net::NewBubble { id, .. } => *id,
        _ => unreachable!(),
    };
    for n in &shell.log {
        if let Net::Edit { id, .. } = n {
            assert_eq!(*id, first_id, "all edits target the single bubble");
        }
    }
    // The LAST network action is the green close.
    assert!(
        matches!(
            shell.log.last(),
            Some(Net::Edit {
                green_close: true,
                ..
            })
        ),
        "storm must end with the green close, log tail: {:?}",
        shell.log.last()
    );
    // Edit volume sanity (Telegram tolerance ~1 msg/sec/chat): ≤ 2/minute.
    let edits = shell
        .log
        .iter()
        .filter(|n| matches!(n, Net::Edit { .. }))
        .count();
    assert!(edits <= 61, "edit volume bounded, got {edits}");
}

/// Adversarial timing: INSTANT reconnect (+1s) and pairs every 61s so the
/// 60s stability window fires MID-storm → close/reopen cycles. Still must
/// be ONE bubble (closes + reopens are edits of the same message id).
#[test]
fn refute_storm_midstorm_close_reopen_cycles_still_one_bubble() {
    let mut shell = Shell::new();
    let mut events = Vec::new();
    for k in 0..30_u64 {
        events.push((T0 + k * 61 * SEC, EpisodeRole::Open, Severity::High, 0));
        events.push((
            T0 + k * 61 * SEC + SEC,
            EpisodeRole::Resolve,
            Severity::Medium,
            k as u32 + 1,
        ));
    }
    let end = T0 + 35 * 60 * SEC;
    run_storm(&mut shell, &events, end);

    let bubbles = shell.new_bubbles();
    assert_eq!(
        bubbles.len(),
        1,
        "mid-storm close/reopen must NOT mint extra bubbles, log: {:?}",
        shell
            .log
            .iter()
            .filter(|n| matches!(n, Net::NewBubble { .. } | Net::LegacySend))
            .collect::<Vec<_>>()
    );
    assert_eq!(shell.ignored_high, 0);
    assert!(
        matches!(
            shell.log.last(),
            Some(Net::Edit {
                green_close: true,
                ..
            })
        ),
        "ends green"
    );
}

/// Drop hunt A: a fresh HIGH disconnect 119s AFTER the green close folds
/// into the closed bubble as an EDIT (reopen) — delivered, never dropped —
/// and at 121s it opens a FRESH page with the SMS leg.
#[test]
fn refute_high_after_green_close_reopen_vs_fresh_page_boundary() {
    let mut shell = Shell::new();
    let events = vec![
        (T0, EpisodeRole::Open, Severity::High, 0),
        (T0 + 5 * SEC, EpisodeRole::Resolve, Severity::Medium, 1),
    ];
    // Quiet through the stability window → green close at ~T0+70s tick.
    run_storm(&mut shell, &events, T0 + 80 * SEC);
    assert_eq!(shell.green_closes(), 1, "closed green first");
    let close_at = T0 + 70 * SEC; // tick that promoted the close

    // HIGH disconnect inside the 120s reopen window → EDIT of the SAME
    // bubble (design-intended flap fold; edit != suppress).
    shell.event(EpisodeRole::Open, Severity::High, 0, close_at + 119 * SEC);
    assert_eq!(
        shell.new_bubbles().len(),
        1,
        "119s flap folds — no new bubble"
    );
    assert!(
        matches!(
            shell.log.last(),
            Some(Net::Edit {
                green_close: false,
                ..
            })
        ),
        "flap reverted the bubble via edit"
    );
    assert_eq!(shell.ignored_high, 0, "the HIGH flap was NOT ignored");

    // Control: a second registry where the HIGH lands 121s after close →
    // FRESH first page with SMS.
    let mut shell2 = Shell::new();
    run_storm(
        &mut shell2,
        &[
            (T0, EpisodeRole::Open, Severity::High, 0),
            (T0 + 5 * SEC, EpisodeRole::Resolve, Severity::Medium, 1),
        ],
        T0 + 80 * SEC,
    );
    shell2.event(EpisodeRole::Open, Severity::High, 0, close_at + 121 * SEC);
    assert_eq!(
        shell2.new_bubbles().len(),
        2,
        "past the reopen window a HIGH opens a fresh page"
    );
    assert!(
        matches!(shell2.log.last(), Some(Net::NewBubble { sms: true, .. })),
        "fresh page carries the SMS leg"
    );
}

/// Drop hunt B: task-inversion race — the reconnect of a pair is applied
/// BEFORE its disconnect (both are independently spawned tasks). The
/// Resolve must route SendLegacy (delivered, not dropped) and the HIGH
/// disconnect still opens the first page with SMS.
#[test]
fn refute_inverted_pair_never_drops_high_but_leaks_one_legacy_line() {
    let mut shell = Shell::new();
    // Reconnect applied first (no state, no tombstone).
    shell.event(EpisodeRole::Resolve, Severity::Medium, 1, T0);
    // Then the disconnect.
    shell.event(EpisodeRole::Open, Severity::High, 0, T0 + 1);
    let legacy = shell
        .log
        .iter()
        .filter(|n| matches!(n, Net::LegacySend))
        .count();
    assert_eq!(legacy, 1, "inverted Resolve routes the legacy lane (sent)");
    assert_eq!(shell.new_bubbles().len(), 1, "HIGH still pages first");
    assert!(matches!(
        shell.log.last(),
        Some(Net::NewBubble { sms: true, .. })
    ));
    assert_eq!(shell.ignored_high, 0);
}

/// Drop hunt C: sub-20s HIGH bursts inside Down fold as EditThrottled —
/// the counters advance and the NEXT eligible edit carries them; the
/// first page already paged HIGH. Nothing is lost from the record.
#[test]
fn refute_throttled_high_burst_counters_survive_to_next_edit() {
    let mut shell = Shell::new();
    shell.event(EpisodeRole::Open, Severity::High, 0, T0);
    // 10 HIGH drops inside the 20s throttle window — all folded.
    for i in 1..=10_u64 {
        shell.event(EpisodeRole::Open, Severity::High, 0, T0 + i * SEC);
    }
    assert_eq!(shell.new_bubbles().len(), 1);
    // First eligible edit after the throttle: counters carry all 11 drops.
    shell.event(EpisodeRole::Open, Severity::High, 0, T0 + 25 * SEC);
    let snap = shell.reg.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].occurrences, 12, "every folded drop is counted");
    assert!(
        matches!(shell.log.last(), Some(Net::Edit { .. })),
        "post-throttle edit carried the folded counters"
    );
}
