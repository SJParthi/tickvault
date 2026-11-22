//! `host-tuning apply`: the NIC receive-coalescing trial (owner card, Fri
//! 13 Nov 2026: "Yes, measure 1 day (Recommended)" — "About 0–20 µs per
//! packet faster, slightly more CPU. Applied outside market hours; easy to
//! undo.").
//!
//! While the trial is on (`TV_NIC_RX_COALESCE=off` and today, IST, is on or
//! before `TV_NIC_RX_COALESCE_UNTIL`), the default-route NIC is set to
//! `rx-usecs 0`, `adaptive-rx off`. The values found first are saved to
//! [`NIC_COALESCE_BEFORE_PATH`] BEFORE anything is written, and only once, so a
//! re-run cannot record the trial's own values as "before". Once the trial is
//! over (the date passed, or the switch flipped to `keep`), the next run writes
//! the saved values back, reads them again, and deletes the file. An EC2
//! stop/start also resets the driver to its defaults by itself.
//!
//! Undo is one line: `TV_NIC_RX_COALESCE=keep` in
//! `/etc/tickvault/host-tuning.env` (read at each start, no reload needed),
//! then `systemctl restart tickvault-host-tuning`. Only the env-file route
//! survives a deploy: every deploy copies the repo unit over the box copy,
//! reloads systemd and restarts the unit, and deleting the unit's two lines
//! does not undo it either, because the defaults below are also `off` and the
//! same date. A quicker undo that lasts only until the next deploy:
//! `TV_NIC_RX_COALESCE=keep` in the unit, then `systemctl daemon-reload &&
//! systemctl restart tickvault-host-tuning` (without the daemon-reload systemd
//! keeps the cached unit and the restart re-applies the trial).
//!
//! Never STARTED inside market hours: a run between 09:00 and 15:45 IST
//! (any day, so a weekend Muhurat session is covered too) that would turn the
//! trial on leaves the card alone and logs why; the next run outside that
//! window applies it. A run there with the trial off (the `keep` undo, or the
//! date passed) still restores, because that is the undo. The unit runs at
//! every boot and on every deploy, and an emergency deploy can override the
//! 09:00-15:45 deploy block, so the gate is here, not in the deploy rules.
//!
//! Where the interface or the ioctl does not exist (a Mac, a container, a NIC
//! without interrupt moderation) this logs one "skipped" line and changes
//! nothing; it is never an error. Cold path: once per boot, before the app.

use tickvault_core::network::nic_coalesce::{
    self, CoalescePlan, EthtoolCoalesce, SavedRx, TRIAL_TARGET,
};

/// The before-values file (see the module header).
pub const NIC_COALESCE_BEFORE_PATH: &str = "/opt/tickvault/nic-coalesce.before";
/// The switch: `off` turns receive coalescing off for the trial; `keep` (or
/// anything else) leaves it, and restores saved values. Default `off`: the
/// owner card switched the trial on; the date below bounds it.
pub const NIC_RX_COALESCE_SWITCH_ENV: &str = "TV_NIC_RX_COALESCE";
pub const NIC_RX_COALESCE_SWITCH_DEFAULT: &str = "off";
/// Last IST day of the trial (inclusive). The next trading day after the
/// deploy of 13 Nov 2026; move it if the deploy slips.
pub const NIC_RX_COALESCE_UNTIL_ENV: &str = "TV_NIC_RX_COALESCE_UNTIL";
pub const NIC_RX_COALESCE_UNTIL_DEFAULT: &str = "2026-11-16";

/// Inside [`NIC_RX_TRIAL_BLOCK_START_SECS_IST`, `NIC_RX_TRIAL_BLOCK_END_SECS_IST`)
/// (seconds of the IST day) the trial is never started (see the module
/// header): 09:00 to 15:45 IST, the same window the deploy block uses.
pub const NIC_RX_TRIAL_BLOCK_START_SECS_IST: u32 = 9 * 3_600;
pub const NIC_RX_TRIAL_BLOCK_END_SECS_IST: u32 = 15 * 3_600 + 45 * 60;

/// The kernel and file side, so the decision can be tested without a NIC.
pub trait NicIo {
    /// `ETHTOOL_GCOALESCE`; `Err` is the OS error number.
    fn get(&self, iface: &str) -> Result<EthtoolCoalesce, i32>;
    /// `ETHTOOL_SCOALESCE`; `Err` is the OS error number.
    fn set(&self, iface: &str, value: &EthtoolCoalesce) -> Result<(), i32>;
    /// The before-values file's text, `None` when absent or unreadable.
    fn read_saved(&self) -> Option<String>;
    /// Writes the before-values file; `false` when the write failed.
    fn save(&self, text: &str) -> bool;
    /// Deletes the before-values file; `false` when that failed.
    fn remove_saved(&self) -> bool;
}

/// The real host.
pub struct HostNic;

impl NicIo for HostNic {
    fn get(&self, iface: &str) -> Result<EthtoolCoalesce, i32> {
        nic_coalesce::get_coalesce(iface)
    }
    fn set(&self, iface: &str, value: &EthtoolCoalesce) -> Result<(), i32> {
        nic_coalesce::set_coalesce(iface, value)
    }
    fn read_saved(&self) -> Option<String> {
        std::fs::read_to_string(NIC_COALESCE_BEFORE_PATH).ok()
    }
    fn save(&self, text: &str) -> bool {
        std::fs::write(NIC_COALESCE_BEFORE_PATH, text).is_ok()
    }
    fn remove_saved(&self) -> bool {
        std::fs::remove_file(NIC_COALESCE_BEFORE_PATH).is_ok()
    }
}

fn errno_text(errno: i32) -> String {
    format!(
        "errno {errno}: {}",
        std::io::Error::from_raw_os_error(errno)
    )
}

/// One step of the trial on `iface` (the default-route NIC). Returns the one
/// line to log. `until` is only for the log line.
pub fn nic_rx_coalesce_step(io: &dyn NicIo, iface: &str, trial: bool, until: &str) -> String {
    let saved_text = io.read_saved();
    let saved = saved_text
        .as_deref()
        .and_then(nic_coalesce::parse_before_file);
    if saved_text.is_some() && saved.is_none() {
        return format!(
            "host-tuning: WARNING NIC rx coalescing: {NIC_COALESCE_BEFORE_PATH} is unreadable — left in place, nothing changed"
        );
    }
    if !trial && saved.is_none() {
        return format!(
            "host-tuning: NIC {iface} rx coalescing left as the driver set it (trial off, nothing saved)"
        );
    }
    // Restore on the interface the values were saved from.
    let target_iface = match (&saved, trial) {
        (Some((name, _)), false) => name.as_str(),
        _ => iface,
    };
    let current = match io.get(target_iface) {
        Ok(c) => c,
        Err(e) if nic_coalesce::errno_means_unsupported(e) => {
            return format!(
                "host-tuning: NIC {target_iface} rx coalescing skipped — not readable here ({})",
                errno_text(e)
            );
        }
        Err(e) => {
            return format!(
                "host-tuning: WARNING NIC {target_iface} rx coalescing could not be read ({}) — nothing changed",
                errno_text(e)
            );
        }
    };
    let saved_rx: Option<SavedRx> = saved.as_ref().map(|(_, rx)| *rx);
    let before = current.rx();
    let (value, want, verb) = match nic_coalesce::plan_rx_coalesce(trial, &current, saved_rx) {
        CoalescePlan::Untouched => {
            return format!("host-tuning: NIC {target_iface} rx coalescing left as is");
        }
        CoalescePlan::AlreadyTarget => {
            return format!(
                "host-tuning: NIC {target_iface} rx coalescing already {} (trial until {until} IST)",
                TRIAL_TARGET.describe()
            );
        }
        CoalescePlan::AlreadyRestored => {
            let removed = io.remove_saved();
            return format!(
                "host-tuning: NIC {target_iface} rx coalescing already back at {} (trial over; saved file {})",
                before.describe(),
                if removed { "removed" } else { "NOT removed" }
            );
        }
        CoalescePlan::Set(v) => {
            // Save first, and only once: a crash after the write must still
            // find the true "before" values.
            if saved.is_none() && !io.save(&nic_coalesce::before_file_text(iface, before)) {
                return format!(
                    "host-tuning: WARNING NIC {iface} rx coalescing NOT changed — could not save {NIC_COALESCE_BEFORE_PATH}, so it could not be undone"
                );
            }
            (v, TRIAL_TARGET, "trial")
        }
        CoalescePlan::Restore(v) => (v, v.rx(), "restore"),
    };
    if let Err(e) = io.set(target_iface, &value) {
        return format!(
            "host-tuning: WARNING NIC {target_iface} rx coalescing {verb} refused ({}) — still {}",
            errno_text(e),
            before.describe()
        );
    }
    let after = io.get(target_iface);
    let verified = after.is_ok_and(|a| nic_coalesce::verify_rx(&a, want));
    let removed = verb == "restore" && verified && io.remove_saved();
    let tail = match (verified, verb) {
        (false, _) => "NOT verified on read-back".to_string(),
        (true, "trial") => format!("verified; trial until {until} IST"),
        (true, _) => format!(
            "verified; trial over, saved file {}",
            if removed { "removed" } else { "NOT removed" }
        ),
    };
    let warning = if verified { "" } else { "WARNING " };
    format!(
        "host-tuning: {warning}NIC {target_iface} rx coalescing {} -> {} ({tail})",
        before.describe(),
        want.describe()
    )
}

/// The trial step at `now` (IST): the date bound, then the market-hours gate
/// (a trial is never started between 09:00 and 15:45 IST; a restore is the
/// undo and still runs), then [`nic_rx_coalesce_step`]. Returns the line to
/// log.
pub fn nic_rx_coalesce_run(
    io: &dyn NicIo,
    iface: &str,
    switch: &str,
    until: &str,
    now: chrono::NaiveDateTime,
) -> String {
    use chrono::Timelike;
    let trial = nic_coalesce::trial_active(switch, until, now.date());
    let secs = now.num_seconds_from_midnight();
    if trial && (NIC_RX_TRIAL_BLOCK_START_SECS_IST..NIC_RX_TRIAL_BLOCK_END_SECS_IST).contains(&secs)
    {
        return format!(
            "host-tuning: NIC {iface} rx coalescing not touched — {} IST is inside 09:00-15:45 IST, \
             so the trial is not started mid-session; the next run outside that window applies it",
            now.format("%H:%M")
        );
    }
    nic_rx_coalesce_step(io, iface, trial, until)
}

/// The trial step against the real host: reads the switch and date from the
/// environment, now (IST) from the clock, and logs one line.
pub fn apply_nic_rx_coalesce(iface: Option<&str>, emit: &dyn Fn(&str)) {
    let switch = super::env_or(NIC_RX_COALESCE_SWITCH_ENV, NIC_RX_COALESCE_SWITCH_DEFAULT);
    let until = super::env_or(NIC_RX_COALESCE_UNTIL_ENV, NIC_RX_COALESCE_UNTIL_DEFAULT);
    let Some(iface) = iface else {
        emit("host-tuning: NIC rx coalescing skipped — no default-route interface");
        return;
    };
    emit(&nic_rx_coalesce_run(
        &HostNic,
        iface,
        &switch,
        &until,
        now_ist(),
    ));
}

/// Now in IST. The offset is a constant that always fits, so the UTC
/// fallback never runs in practice.
fn now_ist() -> chrono::NaiveDateTime {
    let now = chrono::Utc::now();
    chrono::FixedOffset::east_opt(tickvault_common::constants::IST_UTC_OFFSET_SECONDS).map_or_else(
        || now.naive_utc(),
        |ist| now.with_timezone(&ist).naive_local(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tickvault_core::network::nic_coalesce::{ETHTOOL_COALESCE_FIELDS, ETHTOOL_GCOALESCE};

    fn ena(rx_usecs: u32, adaptive: bool) -> EthtoolCoalesce {
        let mut f = [0_u32; ETHTOOL_COALESCE_FIELDS];
        f[0] = ETHTOOL_GCOALESCE;
        f[1] = rx_usecs;
        f[5] = 64;
        f[10] = u32::from(adaptive);
        EthtoolCoalesce(f)
    }

    /// A fake NIC: `set` changes what `get` returns unless `sticky` (a driver
    /// that accepts the write and ignores it).
    struct Fake {
        card: RefCell<Result<EthtoolCoalesce, i32>>,
        set_err: Option<i32>,
        sticky: bool,
        file: RefCell<Option<String>>,
        save_ok: bool,
        sets: RefCell<Vec<(String, EthtoolCoalesce)>>,
    }

    impl Fake {
        fn new(card: Result<EthtoolCoalesce, i32>, file: Option<&str>) -> Self {
            Self {
                card: RefCell::new(card),
                set_err: None,
                sticky: false,
                file: RefCell::new(file.map(str::to_string)),
                save_ok: true,
                sets: RefCell::new(Vec::new()),
            }
        }
    }

    impl NicIo for Fake {
        fn get(&self, _iface: &str) -> Result<EthtoolCoalesce, i32> {
            *self.card.borrow()
        }
        fn set(&self, iface: &str, value: &EthtoolCoalesce) -> Result<(), i32> {
            self.sets.borrow_mut().push((iface.to_string(), *value));
            if let Some(e) = self.set_err {
                return Err(e);
            }
            if !self.sticky {
                *self.card.borrow_mut() = Ok(*value);
            }
            Ok(())
        }
        fn read_saved(&self) -> Option<String> {
            self.file.borrow().clone()
        }
        fn save(&self, text: &str) -> bool {
            if self.save_ok {
                *self.file.borrow_mut() = Some(text.to_string());
            }
            self.save_ok
        }
        fn remove_saved(&self) -> bool {
            self.file.borrow_mut().take().is_some()
        }
    }

    #[test]
    fn test_nic_rx_coalesce_step_trial_saves_first_sets_and_verifies() {
        let io = Fake::new(Ok(ena(20, true)), None);
        let line = nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert_eq!(
            line,
            "host-tuning: NIC ens34 rx coalescing rx-usecs=20 adaptive-rx=on -> \
             rx-usecs=0 adaptive-rx=off (verified; trial until 2026-11-16 IST)"
        );
        assert_eq!(
            io.file.borrow().as_deref(),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n")
        );
        let sets = io.sets.borrow();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].1.0[5], 64, "transmit written back unchanged");
    }

    #[test]
    fn test_rerun_during_the_trial_keeps_the_first_before_values() {
        let io = Fake::new(
            Ok(ena(0, false)),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n"),
        );
        let line = nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert!(
            line.contains("already rx-usecs=0 adaptive-rx=off"),
            "{line}"
        );
        assert!(io.sets.borrow().is_empty(), "never written again");
        assert_eq!(
            io.file.borrow().as_deref(),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n")
        );
        // A deploy that restarts the unit on a rebooted (driver-reset) card
        // re-applies without overwriting the saved file.
        let io = Fake::new(
            Ok(ena(30, true)),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n"),
        );
        nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert_eq!(
            io.file.borrow().as_deref(),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n")
        );
    }

    /// An EC2 stop/start resets the card to its driver default, but while the
    /// trial is on (switch `off`, date not past) the boot run sets it to the
    /// trial values again. So a stop/start is NOT an undo during the trial;
    /// only the `keep` switch (or the date passing) is. Pins the runbook's
    /// "Undo early" wording (2026-11-13 review).
    #[test]
    fn test_stop_start_during_the_trial_reapplies_it_so_it_is_not_an_undo() {
        let saved = "iface=ens34\nrx_usecs=20\nadaptive_rx=1\n";
        let io = Fake::new(Ok(ena(20, true)), Some(saved));
        let line = nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert!(
            line.contains("-> rx-usecs=0 adaptive-rx=off (verified; trial until 2026-11-16 IST)"),
            "{line}"
        );
        let card = (*io.card.borrow()).expect("card readable"); // APPROVED: test
        assert_eq!((card.0[1], card.0[10]), (0, 0), "trial values back on");
        // With the switch at `keep` the same boot puts the saved values back
        // (here already the driver's) and removes the file: that IS the undo.
        let io = Fake::new(Ok(ena(20, true)), Some(saved));
        nic_rx_coalesce_step(&io, "ens34", false, "2026-11-16");
        assert!(io.sets.borrow().is_empty(), "keep never sets the trial");
        assert!(io.file.borrow().is_none());
    }

    #[test]
    fn test_trial_over_restores_saved_values_and_removes_the_file() {
        let io = Fake::new(
            Ok(ena(0, false)),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n"),
        );
        let line = nic_rx_coalesce_step(&io, "ens5", false, "2026-11-16");
        assert!(
            line.contains("rx-usecs=0 adaptive-rx=off -> rx-usecs=20 adaptive-rx=on (verified; trial over, saved file removed)"),
            "{line}"
        );
        assert_eq!(
            io.sets.borrow()[0].0,
            "ens34",
            "restored on the saved interface"
        );
        assert!(io.file.borrow().is_none());
    }

    #[test]
    fn test_trial_over_after_a_reboot_only_removes_the_file() {
        let io = Fake::new(
            Ok(ena(20, true)),
            Some("iface=ens34\nrx_usecs=20\nadaptive_rx=1\n"),
        );
        let line = nic_rx_coalesce_step(&io, "ens34", false, "2026-11-16");
        assert!(
            line.contains("already back at rx-usecs=20 adaptive-rx=on"),
            "{line}"
        );
        assert!(io.sets.borrow().is_empty());
        assert!(io.file.borrow().is_none());
    }

    #[test]
    fn test_trial_off_with_nothing_saved_never_touches_the_card() {
        let io = Fake::new(Err(libc_enodev()), None);
        let line = nic_rx_coalesce_step(&io, "ens34", false, "2026-11-16");
        assert!(line.contains("left as the driver set it"), "{line}");
        assert!(io.sets.borrow().is_empty());
    }

    #[test]
    fn test_absent_interface_or_ioctl_is_a_skipped_line() {
        let io = Fake::new(Err(libc_enodev()), None);
        let line = nic_rx_coalesce_step(&io, "en0", true, "2026-11-16");
        assert!(line.contains("skipped — not readable here"), "{line}");
        assert!(!line.contains("WARNING"), "{line}");
        assert!(
            io.file.borrow().is_none(),
            "nothing saved when nothing changed"
        );
    }

    #[test]
    fn test_a_refused_or_ignored_write_is_a_warning_and_keeps_the_file() {
        let mut io = Fake::new(Ok(ena(20, true)), None);
        io.set_err = Some(22);
        let line = nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert!(line.starts_with("host-tuning: WARNING"), "{line}");
        assert!(line.contains("trial refused"), "{line}");
        // The before file stays, so a later restore is still possible.
        assert!(io.file.borrow().is_some());

        let mut io = Fake::new(Ok(ena(20, true)), None);
        io.sticky = true;
        let line = nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert!(
            line.contains("WARNING") && line.contains("NOT verified"),
            "{line}"
        );
    }

    #[test]
    fn test_no_save_means_no_write() {
        let mut io = Fake::new(Ok(ena(20, true)), None);
        io.save_ok = false;
        let line = nic_rx_coalesce_step(&io, "ens34", true, "2026-11-16");
        assert!(line.contains("NOT changed"), "{line}");
        assert!(io.sets.borrow().is_empty());
    }

    #[test]
    fn test_unreadable_saved_file_changes_nothing() {
        let io = Fake::new(Ok(ena(0, false)), Some("garbage"));
        let line = nic_rx_coalesce_step(&io, "ens34", false, "2026-11-16");
        assert!(line.contains("unreadable"), "{line}");
        assert!(io.sets.borrow().is_empty());
        assert!(io.file.borrow().is_some());
    }

    #[test]
    fn test_defaults_bound_the_trial() {
        let until = chrono::NaiveDate::parse_from_str(NIC_RX_COALESCE_UNTIL_DEFAULT, "%Y-%m-%d")
            .expect("valid default date"); // APPROVED: test
        assert!(nic_coalesce::trial_active(
            NIC_RX_COALESCE_SWITCH_DEFAULT,
            NIC_RX_COALESCE_UNTIL_DEFAULT,
            until
        ));
        assert!(!nic_coalesce::trial_active(
            NIC_RX_COALESCE_SWITCH_DEFAULT,
            NIC_RX_COALESCE_UNTIL_DEFAULT,
            until.succ_opt().expect("next day") // APPROVED: test
        ));
    }

    #[test]
    fn test_apply_nic_rx_coalesce_without_an_interface_logs_and_returns() {
        let lines = RefCell::new(Vec::new());
        apply_nic_rx_coalesce(None, &|l| lines.borrow_mut().push(l.to_string()));
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("no default-route interface"));
        // A host with no such interface: one skipped line, never a panic.
        // (Reads the real before file path only if it exists; on a dev box or
        // CI runner it does not.)
        if !std::path::Path::new(NIC_COALESCE_BEFORE_PATH).exists() {
            let lines = RefCell::new(Vec::new());
            apply_nic_rx_coalesce(Some("tvnosuch0"), &|l| {
                lines.borrow_mut().push(l.to_string());
            });
            assert_eq!(lines.borrow().len(), 1);
            assert!(!lines.borrow()[0].is_empty());
        }
    }

    #[test]
    fn test_now_ist_is_a_plausible_date() {
        let now = now_ist();
        assert!(now.date() >= chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("date")); // APPROVED: test
    }

    fn at(date: &str, hh: u32, mm: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .expect("date") // APPROVED: test
            .and_hms_opt(hh, mm, 0)
            .expect("time") // APPROVED: test
    }

    /// A boot or an override deploy inside 09:00-15:45 IST never starts the
    /// trial (owner card: "Applied outside market hours"); before 09:00, at
    /// 15:45 and later it does. Fails without the gate in
    /// `nic_rx_coalesce_run`: the mid-session run writes the card.
    #[test]
    fn test_nic_rx_coalesce_run_never_starts_the_trial_inside_market_hours() {
        for (hh, mm) in [(9, 0), (11, 0), (15, 44)] {
            let io = Fake::new(Ok(ena(20, true)), None);
            let line =
                nic_rx_coalesce_run(&io, "ens34", "off", "2026-11-16", at("2026-11-13", hh, mm));
            assert!(
                io.sets.borrow().is_empty(),
                "{hh}:{mm} wrote the card: {line}"
            );
            assert!(io.file.borrow().is_none(), "{hh}:{mm} saved a file");
            assert!(
                line.contains("not touched") && line.contains("09:00-15:45"),
                "{line}"
            );
        }
        for (hh, mm) in [(8, 30), (8, 59), (15, 45), (16, 0)] {
            let io = Fake::new(Ok(ena(20, true)), None);
            let line =
                nic_rx_coalesce_run(&io, "ens34", "off", "2026-11-16", at("2026-11-13", hh, mm));
            assert_eq!(io.sets.borrow().len(), 1, "{hh}:{mm} must apply: {line}");
            assert!(line.contains("verified; trial until 2026-11-16"), "{line}");
        }
    }

    /// Inside market hours the `keep` undo (and a trial past its date) still
    /// restores the saved values: the gate blocks only starting the trial.
    #[test]
    fn test_nic_rx_coalesce_run_still_restores_inside_market_hours() {
        let file = nic_coalesce::before_file_text("ens34", ena(20, true).rx());
        for (switch, date) in [("keep", "2026-11-13"), ("off", "2026-11-17")] {
            let io = Fake::new(Ok(ena(0, false)), Some(&file));
            let line = nic_rx_coalesce_run(&io, "ens34", switch, "2026-11-16", at(date, 11, 0));
            assert_eq!(io.sets.borrow().len(), 1, "{switch} {date}: {line}");
            assert!(io.file.borrow().is_none(), "restored, file removed: {line}");
        }
    }

    fn libc_enodev() -> i32 {
        19
    }
}
