//! NIC receive interrupt coalescing: read it, turn it off for a bounded trial,
//! put it back (owner card, Fri 13 Nov 2026: "Yes, measure 1 day
//! (Recommended)" — "About 0–20 µs per packet faster, slightly more CPU.
//! Applied outside market hours; easy to undo.").
//!
//! The ENA network card on the box can hold received packets for a few
//! microseconds (adaptive moderation, `rx-usecs` about 20 by default) before it
//! raises an interrupt, so several packets share one interrupt. With
//! `rx-usecs 0` and adaptive moderation off, every packet raises its own
//! interrupt: lower delay before the kernel sees the packet, more interrupts
//! and a little more CPU on the interrupt core.
//!
//! The setting is changed through the ethtool ioctl (`SIOCETHTOOL` with
//! `ETHTOOL_GCOALESCE` / `ETHTOOL_SCOALESCE`), the same call the `ethtool -C`
//! tool makes, so no shell and no external program is involved. It is called
//! once per boot by `tickvault host-tuning apply` (a separate root process that
//! runs before the trading app), never by the trading app itself.
//!
//! Everything that decides WHAT to write is pure and tested here
//! ([`EthtoolCoalesce`], [`trial_active`], [`plan_rx_coalesce`],
//! [`SavedRx`]); only [`get_coalesce`] / [`set_coalesce`] touch the kernel.
//! Where there is no such interface or no such ioctl (a Mac, a container, a
//! driver without interrupt moderation), the calls return the OS error number
//! and the caller logs a "skipped" line; nothing panics and nothing is fatal.
//!
//! Cold path: once per boot. Not O(1)-relevant.

use chrono::NaiveDate;

/// Number of `u32` fields in the kernel's `struct ethtool_coalesce`
/// (`include/uapi/linux/ethtool.h`): `cmd` plus 22 settings.
pub const ETHTOOL_COALESCE_FIELDS: usize = 23;
/// Its size in bytes: 23 × 4 = 92.
pub const ETHTOOL_COALESCE_BYTES: usize = ETHTOOL_COALESCE_FIELDS * 4;
/// `ETHTOOL_GCOALESCE`: read the coalescing settings.
pub const ETHTOOL_GCOALESCE: u32 = 0x0000_000e;
/// `ETHTOOL_SCOALESCE`: write the coalescing settings.
pub const ETHTOOL_SCOALESCE: u32 = 0x0000_000f;

/// Field index of `cmd`.
const F_CMD: usize = 0;
/// Field index of `rx_coalesce_usecs` (byte offset 4).
const F_RX_USECS: usize = 1;
/// Field index of `use_adaptive_rx_coalesce` (byte offset 40).
const F_ADAPTIVE_RX: usize = 10;

/// The trial's target: no receive delay at all.
pub const TARGET_RX_USECS: u32 = 0;

/// The kernel's `struct ethtool_coalesce`, as its 23 native-endian `u32`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthtoolCoalesce(pub [u32; ETHTOOL_COALESCE_FIELDS]);

impl EthtoolCoalesce {
    /// A request buffer for `cmd` with every setting zero (what a GET sends).
    #[must_use]
    pub fn request(cmd: u32) -> Self {
        let mut fields = [0_u32; ETHTOOL_COALESCE_FIELDS];
        fields[F_CMD] = cmd;
        Self(fields)
    }

    /// The bytes the kernel reads, in native byte order.
    #[must_use]
    pub fn encode(&self) -> [u8; ETHTOOL_COALESCE_BYTES] {
        let mut out = [0_u8; ETHTOOL_COALESCE_BYTES];
        for (chunk, field) in out.chunks_exact_mut(4).zip(self.0) {
            chunk.copy_from_slice(&field.to_ne_bytes());
        }
        out
    }

    /// The struct the kernel wrote back, from native-order bytes.
    #[must_use]
    pub fn decode(bytes: &[u8; ETHTOOL_COALESCE_BYTES]) -> Self {
        let mut fields = [0_u32; ETHTOOL_COALESCE_FIELDS];
        for (field, chunk) in fields.iter_mut().zip(bytes.chunks_exact(4)) {
            let mut word = [0_u8; 4];
            word.copy_from_slice(chunk);
            *field = u32::from_ne_bytes(word);
        }
        Self(fields)
    }

    /// `rx-usecs`.
    #[must_use]
    pub fn rx_usecs(&self) -> u32 {
        self.0[F_RX_USECS]
    }

    /// `adaptive-rx` (any non-zero value is on).
    #[must_use]
    pub fn adaptive_rx(&self) -> bool {
        self.0[F_ADAPTIVE_RX] != 0
    }

    /// The receive half as a [`SavedRx`] (for the before-values file).
    #[must_use]
    pub fn rx(&self) -> SavedRx {
        SavedRx {
            rx_usecs: self.rx_usecs(),
            adaptive_rx: self.adaptive_rx(),
        }
    }

    /// A SET request built from this struct (the values just read), with only
    /// `rx-usecs` and `adaptive-rx` replaced. Every transmit and rate field is
    /// written back unchanged, so the trial never touches transmit.
    #[must_use]
    pub fn with_rx(&self, rx: SavedRx) -> Self {
        let mut fields = self.0;
        fields[F_CMD] = ETHTOOL_SCOALESCE;
        fields[F_RX_USECS] = rx.rx_usecs;
        fields[F_ADAPTIVE_RX] = u32::from(rx.adaptive_rx);
        Self(fields)
    }
}

/// The receive coalescing values worth keeping across the trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedRx {
    /// `rx-usecs`.
    pub rx_usecs: u32,
    /// `adaptive-rx`.
    pub adaptive_rx: bool,
}

/// The trial target: `rx-usecs 0`, `adaptive-rx off`.
pub const TRIAL_TARGET: SavedRx = SavedRx {
    rx_usecs: TARGET_RX_USECS,
    adaptive_rx: false,
};

impl SavedRx {
    /// `rx-usecs=20 adaptive-rx=on`, the words `ethtool -c` uses.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "rx-usecs={} adaptive-rx={}",
            self.rx_usecs,
            if self.adaptive_rx { "on" } else { "off" }
        )
    }
}

/// The before-values file: which interface, and its receive values before the
/// trial changed them. Written once, so a re-run (a deploy restarting the unit
/// while the trial is on) cannot record the trial's own values as "before".
#[must_use]
pub fn before_file_text(iface: &str, rx: SavedRx) -> String {
    format!(
        "iface={iface}\nrx_usecs={}\nadaptive_rx={}\n",
        rx.rx_usecs,
        u8::from(rx.adaptive_rx)
    )
}

/// Parses [`before_file_text`]. `None` for anything incomplete or malformed,
/// which the caller treats as "nothing saved" (it never restores a guess).
#[must_use]
pub fn parse_before_file(text: &str) -> Option<(String, SavedRx)> {
    let mut iface = None;
    let mut rx_usecs = None;
    let mut adaptive_rx = None;
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        match key.trim() {
            "iface" => iface = Some(value.trim().to_string()).filter(|v| !v.is_empty()),
            "rx_usecs" => rx_usecs = Some(value.trim().parse::<u32>().ok()?),
            "adaptive_rx" => {
                adaptive_rx = Some(match value.trim() {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                });
            }
            _ => return None,
        }
    }
    Some((
        iface?,
        SavedRx {
            rx_usecs: rx_usecs?,
            adaptive_rx: adaptive_rx?,
        },
    ))
}

/// Is the trial on today? Only when the switch is `off` (coalescing off) AND
/// `until_ist` is a valid date that `today_ist` has not passed. Any other
/// switch value, a missing date or an unreadable date reads as NOT on, so the
/// trial can never run unbounded.
#[must_use]
pub fn trial_active(switch: &str, until_ist: &str, today_ist: NaiveDate) -> bool {
    switch.trim() == "off"
        && NaiveDate::parse_from_str(until_ist.trim(), "%Y-%m-%d")
            .is_ok_and(|until| today_ist <= until)
}

/// What `host-tuning apply` does to the receive coalescing this boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalescePlan {
    /// Trial on, values not yet at target: write this.
    Set(EthtoolCoalesce),
    /// Trial on, values already at target: write nothing.
    AlreadyTarget,
    /// Trial over, values saved: write the saved values back.
    Restore(EthtoolCoalesce),
    /// Trial over, values already equal the saved ones: write nothing, drop
    /// the saved file.
    AlreadyRestored,
    /// Trial off and nothing saved: leave the card alone.
    Untouched,
}

/// Decide what to write. `current` is what GET just read; `saved` is the
/// before-values file (`None` when there is none or it is unreadable).
#[must_use]
pub fn plan_rx_coalesce(
    trial: bool,
    current: &EthtoolCoalesce,
    saved: Option<SavedRx>,
) -> CoalescePlan {
    match (trial, saved) {
        (true, _) if current.rx() == TRIAL_TARGET => CoalescePlan::AlreadyTarget,
        (true, _) => CoalescePlan::Set(current.with_rx(TRIAL_TARGET)),
        (false, Some(rx)) if current.rx() == rx => CoalescePlan::AlreadyRestored,
        (false, Some(rx)) => CoalescePlan::Restore(current.with_rx(rx)),
        (false, None) => CoalescePlan::Untouched,
    }
}

/// True when a GET after the write shows exactly `want` on the receive side.
#[must_use]
pub fn verify_rx(after: &EthtoolCoalesce, want: SavedRx) -> bool {
    after.rx() == want
}

/// True for the error numbers that mean "this host has no such interface or
/// the driver does not support it" (expected on a Mac, in a container, or on
/// a NIC without interrupt moderation): logged as skipped, never as a fault.
#[must_use]
pub fn errno_means_unsupported(errno: i32) -> bool {
    [
        libc::ENODEV,
        libc::EOPNOTSUPP,
        libc::ENOTTY,
        libc::EPERM,
        libc::EACCES,
        libc::EAFNOSUPPORT,
        libc::EINVAL,
    ]
    .contains(&errno)
}

/// `ETHTOOL_GCOALESCE` on `iface`. `Err` carries the OS error number.
pub fn get_coalesce(iface: &str) -> Result<EthtoolCoalesce, i32> {
    let mut buf = EthtoolCoalesce::request(ETHTOOL_GCOALESCE).encode();
    ethtool_ioctl(iface, &mut buf)?;
    Ok(EthtoolCoalesce::decode(&buf))
}

/// `ETHTOOL_SCOALESCE` on `iface` with `value` (its `cmd` is forced to SET).
pub fn set_coalesce(iface: &str, value: &EthtoolCoalesce) -> Result<(), i32> {
    let mut fields = value.0;
    fields[F_CMD] = ETHTOOL_SCOALESCE;
    let mut buf = EthtoolCoalesce(fields).encode();
    ethtool_ioctl(iface, &mut buf)
}

#[cfg(target_os = "linux")]
fn ethtool_ioctl(iface: &str, buf: &mut [u8; ETHTOOL_COALESCE_BYTES]) -> Result<(), i32> {
    ioctl_call::ethtool(iface, buf)
}

/// No ethtool ioctl off Linux (a Mac dev box): always "not supported".
#[cfg(not(target_os = "linux"))]
fn ethtool_ioctl(_iface: &str, _buf: &mut [u8; ETHTOOL_COALESCE_BYTES]) -> Result<(), i32> {
    Err(libc::EOPNOTSUPP)
}

/// The one ethtool ioctl, and nothing else that needs `unsafe`.
///
/// The crate root denies `unsafe_code` outside tests (audit CR-5); this module
/// is a production opt-out listed in `crates/core/tests/unsafe_code_lint_guard.rs`.
/// Every block carries a SAFETY note.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // APPROVED: the ethtool coalescing ioctl; name length checked, buffer sized to the kernel struct, see SAFETY
mod ioctl_call {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// `SIOCETHTOOL` in the request type this libc's `ioctl` takes (`c_ulong`
    /// on glibc, `c_int` on musl); 0x8946 fits both.
    const SIOCETHTOOL: libc::Ioctl = 0x8946;

    fn last_errno() -> i32 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    }

    /// Runs `SIOCETHTOOL` on `iface` with `buf` as the ethtool struct. The
    /// kernel reads `buf` and, for a GET, writes the answer back into it.
    pub(super) fn ethtool(
        iface: &str,
        buf: &mut [u8; super::ETHTOOL_COALESCE_BYTES],
    ) -> Result<(), i32> {
        let name = iface.as_bytes();
        // The name must fit with its NUL terminator, and hold no NUL itself.
        if name.is_empty() || name.len() >= libc::IFNAMSIZ || name.contains(&0) {
            return Err(libc::EINVAL);
        }
        // SAFETY: `socket` takes three integer arguments and touches no memory of ours.
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) }; // SAFETY: see above
        if raw < 0 {
            return Err(last_errno());
        }
        // SAFETY: `raw` is a descriptor `socket` just returned to us, open and owned by nothing else; `OwnedFd` closes it once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) }; // SAFETY: see above
        // SAFETY: `ifreq` is a C struct of a char array and a union of integers, arrays and pointers; all-zero is a valid value of every member.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() }; // SAFETY: see above
        for (dst, src) in ifr.ifr_name.iter_mut().zip(name) {
            *dst = *src as libc::c_char;
        }
        ifr.ifr_ifru.ifru_data = buf.as_mut_ptr().cast::<libc::c_char>();
        // SAFETY: `fd` is an open socket; `ifr` is a valid `ifreq` with a NUL-terminated name (checked above, rest zeroed) whose data pointer points at `buf`, which is exactly the 92-byte `struct ethtool_coalesce` the kernel reads and writes for these commands, and which outlives the call.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), SIOCETHTOOL, &mut ifr) }; // SAFETY: see above
        if rc < 0 { Err(last_errno()) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ena_default() -> EthtoolCoalesce {
        // Values shaped like ENA's defaults: rx 20 µs adaptive on, tx 64 µs.
        let mut f = [0_u32; ETHTOOL_COALESCE_FIELDS];
        f[F_CMD] = ETHTOOL_GCOALESCE;
        f[F_RX_USECS] = 20;
        f[5] = 64; // tx_coalesce_usecs
        f[F_ADAPTIVE_RX] = 1;
        f[12] = 7; // pkt_rate_low
        f[22] = 9; // rate_sample_interval
        EthtoolCoalesce(f)
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date") // APPROVED: test
    }

    #[test]
    fn test_struct_round_trips_with_rx_usecs_and_adaptive_rx_offsets() {
        assert_eq!(ETHTOOL_COALESCE_BYTES, 92);
        let v = ena_default();
        let bytes = v.encode();
        assert_eq!(bytes.len(), 92);
        assert_eq!(EthtoolCoalesce::decode(&bytes), v);
        // rx_coalesce_usecs sits at byte 4, use_adaptive_rx_coalesce at byte 40.
        assert_eq!(bytes[4..8], 20_u32.to_ne_bytes());
        assert_eq!(bytes[40..44], 1_u32.to_ne_bytes());
        assert_eq!(bytes[0..4], ETHTOOL_GCOALESCE.to_ne_bytes());
        let back = EthtoolCoalesce::decode(&bytes);
        assert_eq!(back.rx_usecs(), 20);
        assert!(back.adaptive_rx());
        assert!(!EthtoolCoalesce::request(ETHTOOL_GCOALESCE).adaptive_rx());
    }

    #[test]
    fn test_request_and_command_numbers() {
        assert_eq!(ETHTOOL_GCOALESCE, 0x0e);
        assert_eq!(ETHTOOL_SCOALESCE, 0x0f);
        let r = EthtoolCoalesce::request(ETHTOOL_GCOALESCE);
        assert_eq!(r.0[0], ETHTOOL_GCOALESCE);
        assert!(r.0[1..].iter().all(|f| *f == 0));
    }

    #[test]
    fn test_with_rx_set_changes_only_the_two_rx_fields() {
        let before = ena_default();
        let CoalescePlan::Set(set) = plan_rx_coalesce(true, &before, None) else {
            panic!("trial on must plan a Set"); // APPROVED: test
        };
        assert_eq!(set.0[F_CMD], ETHTOOL_SCOALESCE);
        assert_eq!(set.rx(), TRIAL_TARGET);
        for i in 0..ETHTOOL_COALESCE_FIELDS {
            if ![F_CMD, F_RX_USECS, F_ADAPTIVE_RX].contains(&i) {
                assert_eq!(
                    set.0[i], before.0[i],
                    "field {i} must be written back unchanged"
                );
            }
        }
    }

    #[test]
    fn test_plan_rx_coalesce_matrix() {
        let before = ena_default();
        let target = before.with_rx(TRIAL_TARGET);
        // Trial on and already at target: never written again.
        assert_eq!(
            plan_rx_coalesce(true, &target, Some(before.rx())),
            CoalescePlan::AlreadyTarget
        );
        // Trial over with a saved file: restore exactly the saved values.
        let CoalescePlan::Restore(r) = plan_rx_coalesce(false, &target, Some(before.rx())) else {
            panic!("trial over with saved values must restore"); // APPROVED: test
        };
        assert_eq!(r.rx(), before.rx());
        assert_eq!(r.0[5], before.0[5], "transmit kept on restore");
        // A reboot already reset the driver: nothing to write.
        assert_eq!(
            plan_rx_coalesce(false, &before, Some(before.rx())),
            CoalescePlan::AlreadyRestored
        );
        // Trial over, nothing saved: hands off.
        assert_eq!(
            plan_rx_coalesce(false, &target, None),
            CoalescePlan::Untouched
        );
        assert_eq!(
            plan_rx_coalesce(false, &before, None),
            CoalescePlan::Untouched
        );
    }

    #[test]
    fn test_trial_active_is_bounded_by_its_date() {
        assert!(trial_active("off", "2026-11-16", d("2026-11-13")));
        assert!(trial_active(" off ", "2026-11-16", d("2026-11-16")));
        assert!(!trial_active("off", "2026-11-16", d("2026-11-17")));
        // Any other switch value, or no / bad date, is never on.
        assert!(!trial_active("keep", "2026-11-16", d("2026-11-13")));
        assert!(!trial_active("on", "2026-11-16", d("2026-11-13")));
        assert!(!trial_active("", "2026-11-16", d("2026-11-13")));
        assert!(!trial_active("off", "", d("2026-11-13")));
        assert!(!trial_active("off", "16-11-2026", d("2026-11-13")));
    }

    #[test]
    fn test_verify_rx() {
        let before = ena_default();
        assert!(verify_rx(&before.with_rx(TRIAL_TARGET), TRIAL_TARGET));
        assert!(
            !verify_rx(&before, TRIAL_TARGET),
            "rx-usecs 20 adaptive on is not the target"
        );
        let mut half = before.with_rx(TRIAL_TARGET);
        half.0[F_ADAPTIVE_RX] = 1;
        assert!(!verify_rx(&half, TRIAL_TARGET), "adaptive left on fails");
        let mut half = before.with_rx(TRIAL_TARGET);
        half.0[F_RX_USECS] = 3;
        assert!(!verify_rx(&half, TRIAL_TARGET), "a non-zero rx-usecs fails");
    }

    #[test]
    fn test_before_file_text_and_parse_before_file_round_trip() {
        let rx = ena_default().rx();
        let text = before_file_text("ens34", rx);
        assert_eq!(text, "iface=ens34\nrx_usecs=20\nadaptive_rx=1\n");
        assert_eq!(parse_before_file(&text), Some(("ens34".to_string(), rx)));
        for bad in [
            "",
            "iface=ens34\nrx_usecs=20\n",
            "iface=\nrx_usecs=20\nadaptive_rx=1\n",
            "iface=ens34\nrx_usecs=x\nadaptive_rx=1\n",
            "iface=ens34\nrx_usecs=20\nadaptive_rx=2\n",
            "iface=ens34\nrx_usecs=20\nadaptive_rx=1\nother=1\n",
            "garbage",
        ] {
            assert_eq!(parse_before_file(bad), None, "must refuse {bad:?}");
        }
    }

    #[test]
    fn test_describe() {
        assert_eq!(ena_default().rx().describe(), "rx-usecs=20 adaptive-rx=on");
        assert_eq!(TRIAL_TARGET.describe(), "rx-usecs=0 adaptive-rx=off");
    }

    #[test]
    fn test_errno_means_unsupported_classification() {
        for e in [libc::ENODEV, libc::EOPNOTSUPP, libc::ENOTTY, libc::EPERM] {
            assert!(errno_means_unsupported(e), "{e} is an expected absence");
        }
        assert!(!errno_means_unsupported(libc::EIO));
        assert!(!errno_means_unsupported(libc::EBUSY));
    }

    #[test]
    fn test_get_coalesce_and_set_coalesce_missing_interface_is_an_errno() {
        let err = get_coalesce("tvnosuch0").expect_err("no such interface"); // APPROVED: test
        assert!(errno_means_unsupported(err), "got errno {err}");
        let err = set_coalesce("tvnosuch0", &ena_default()).expect_err("no such interface"); // APPROVED: test
        assert!(errno_means_unsupported(err), "got errno {err}");
        // Names the kernel cannot hold are refused before any syscall.
        assert!(get_coalesce("").is_err());
        assert!(get_coalesce("a-name-far-too-long-for-ifnamsiz").is_err());
        assert!(get_coalesce("bad\0name").is_err());
    }

    /// The loopback interface exists on every Linux host and has no interrupt
    /// moderation: the ioctl reaches the driver and answers "not supported"
    /// (or, on a kernel that fills zeros, a struct). Either way, no panic and
    /// nothing written.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_loopback_get_reaches_the_kernel() {
        match get_coalesce("lo") {
            Ok(v) => assert_eq!(v.0[F_CMD], ETHTOOL_GCOALESCE),
            Err(e) => assert!(errno_means_unsupported(e), "got errno {e}"),
        }
    }
}
