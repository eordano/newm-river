//! The kernel's power supply events, for `on-battery`: one `NETLINK_KOBJECT_UEVENT` socket bound
//! to the kernel's group, open only while the background is animated with `on-battery` not
//! "animated". The main loop polls its fd; each `power_supply` event re-reads the supplies
//! (`newm_synth::power::on_battery`), so nothing polls the battery on a timer.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{bail, Result};

/// The kernel's own uevent multicast group (udev's rebroadcast is group 2).
const KERNEL_GROUP: u32 = 1;

pub struct Uevents {
    fd: OwnedFd,
}

impl Uevents {
    pub fn open() -> Result<Self> {
        // SAFETY: socket returns a new fd or -1.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            bail!("netlink socket: {}", std::io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_nl is plain data; all zeros is a valid start.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = KERNEL_GROUP;
        // SAFETY: addr is a valid sockaddr_nl of the size given.
        let r = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            bail!("netlink bind: {}", std::io::Error::last_os_error());
        }
        Ok(Uevents { fd })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Reads every queued message; returns whether one was a power supply's.
    pub fn drain(&self) -> bool {
        let mut buf = [0u8; 8192];
        let mut power = false;
        loop {
            // SAFETY: buf is writable for its length.
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                // EAGAIN (drained), or an error such as ENOBUFS after an overrun: then events
                // were lost, so re-read the supplies to be sure.
                if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock
                {
                    power = true;
                }
                return power;
            }
            power |= is_power_supply(&buf[..n as usize]);
        }
    }
}

/// Whether a kernel uevent (`action@devpath` then `KEY=value` fields, each NUL-terminated) is a
/// power supply's.
pub fn is_power_supply(msg: &[u8]) -> bool {
    msg.split(|&b| b == 0)
        .skip(1)
        .any(|field| field == b"SUBSYSTEM=power_supply")
}
