//! The window manager runs as a child of `newm-river`, which starts it again when it crashes.
//!
//! river keeps the session when its window manager goes away (river-window-management-v1): the
//! apps, their windows, outputs and the session lock stay, and the next window manager to bind
//! gets them all. Without this, a panic in newm-river ended the session, since the init script
//! stops the unit once its window manager exits: every app was gone. Now a crash (an exit other
//! than 0, or a signal) while river still answers starts a new window manager, told by
//! [`RESTART_ENV`] that it takes over a running session: it starts no startup programs again and
//! puts the windows back on the spots the session file saved, launching nothing. A clean exit (a
//! quit the user asked for, river ending) ends the supervisor with it; so does a signal to the
//! supervisor itself (forwarded to the child), river being gone, or [`MAX_RESTARTS`] crashes in
//! [`WINDOW`].

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{error, info, warn};

/// Set in the child: it is the window manager, not the supervisor.
pub const CHILD_ENV: &str = "NEWM_RIVER_WM_CHILD";
/// Set in a child started after a crash: how many restarts came before it (1 for the first).
pub const RESTART_ENV: &str = "NEWM_RIVER_RESTART";

/// At most this many crashes within [`WINDOW`] are restarted; the next one ends the session, so
/// a window manager that dies at once does not spin.
pub const MAX_RESTARTS: usize = 5;
pub const WINDOW: Duration = Duration::from_secs(60);

/// The restarts of the last [`WINDOW`] and the wait before the next one.
#[derive(Debug, Default)]
pub struct Restarts {
    crashes: VecDeque<Instant>,
}

impl Restarts {
    /// A crash at `now`: the wait before the restart, or None when it is one too many.
    pub fn after_crash(&mut self, now: Instant) -> Option<Duration> {
        while self
            .crashes
            .front()
            .is_some_and(|&t| now.duration_since(t) >= WINDOW)
        {
            self.crashes.pop_front();
        }
        if self.crashes.len() >= MAX_RESTARTS {
            return None;
        }
        // 0, 250, 500, 1000, 2000 ms: the first restart is immediate, a crash loop slows down.
        let wait = match self.crashes.len() {
            0 => Duration::ZERO,
            n => Duration::from_millis(250 << (n - 1).min(3)),
        };
        self.crashes.push_back(now);
        Some(wait)
    }
}

/// Whether the child's exit status is a crash (restarted) rather than an end (followed).
pub fn crashed(status: ExitStatus) -> bool {
    !status.success()
}

/// The exit code that tells the supervisor's parent how the child ended.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

static CHILD: AtomicI32 = AtomicI32::new(0);
static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn forward(sig: libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(pid, sig) };
    }
}

fn install_forwarding() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction with a handler that only touches atomics and calls kill.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = forward as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Whether river still takes Wayland clients.
fn river_answers() -> bool {
    wayland_client::Connection::connect_to_env().is_ok()
}

/// The IPC socket every window manager of this session binds: $NIRI_SOCKET, else one named after
/// the supervisor, so a restarted child keeps the path clients (the bar) already know.
fn socket_path() -> Option<PathBuf> {
    if std::env::var_os("NIRI_SOCKET").is_some_and(|p| !p.is_empty()) {
        return None;
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let display = display.replace('/', "-");
    Some(dir.join(format!("newm-river.{display}.{}.sock", std::process::id())))
}

/// Runs the window manager as a child until it ends; returns the exit code for the process.
pub fn run() -> Result<i32> {
    install_forwarding();
    let exe = std::env::current_exe().context("finding newm-river's own binary")?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let socket = socket_path();
    let mut restarts = Restarts::default();
    let mut restart = 0u32;
    loop {
        let mut cmd = Command::new(&exe);
        cmd.args(&args).env(CHILD_ENV, "1");
        if let Some(socket) = &socket {
            cmd.env("NIRI_SOCKET", socket);
        }
        if restart > 0 {
            cmd.env(RESTART_ENV, restart.to_string());
        } else {
            cmd.env_remove(RESTART_ENV);
        }
        let mut child = cmd.spawn().context("starting the window manager")?;
        CHILD.store(child.id() as i32, Ordering::SeqCst);
        let status = loop {
            match child.wait() {
                Ok(status) => break status,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("waiting for the window manager"),
            }
        };
        CHILD.store(0, Ordering::SeqCst);
        if !crashed(status) {
            return Ok(0);
        }
        if STOPPING.load(Ordering::SeqCst) {
            info!("the window manager stopped with the session ({status})");
            return Ok(exit_code(status));
        }
        if !river_answers() {
            info!("the window manager ended with river ({status})");
            return Ok(exit_code(status));
        }
        let Some(wait) = restarts.after_crash(Instant::now()) else {
            error!(
                "the window manager crashed {MAX_RESTARTS} times in {}s ({status}): ending the session",
                WINDOW.as_secs()
            );
            return Ok(exit_code(status));
        };
        restart += 1;
        warn!("the window manager crashed ({status}); restarting it (#{restart})");
        std::thread::sleep(wait);
        if STOPPING.load(Ordering::SeqCst) {
            return Ok(exit_code(status));
        }
    }
}

/// The restart count this window manager was started with (0: the session's first).
pub fn restart_count() -> u32 {
    std::env::var(RESTART_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crashes_restart_at_once_then_slower_until_too_many_in_a_minute() {
        let t = Instant::now();
        let mut r = Restarts::default();
        let waits: Vec<_> = (0..MAX_RESTARTS)
            .map(|k| r.after_crash(t + Duration::from_secs(k as u64)))
            .collect();
        assert_eq!(
            waits,
            [0, 250, 500, 1000, 2000].map(|ms| Some(Duration::from_millis(ms)))
        );
        assert_eq!(r.after_crash(t + Duration::from_secs(10)), None);
    }

    #[test]
    fn crashes_older_than_the_window_are_forgotten() {
        let t = Instant::now();
        let mut r = Restarts::default();
        for k in 0..MAX_RESTARTS {
            assert!(r.after_crash(t + Duration::from_secs(k as u64)).is_some());
        }
        // The first crash (at t) left the window: one more is allowed, after the longest wait
        // of the four still in it.
        let later = t + WINDOW + Duration::from_millis(1);
        assert_eq!(r.after_crash(later), Some(Duration::from_millis(2000)));
        assert_eq!(r.after_crash(later), None);
    }

    #[test]
    fn a_clean_exit_ends_and_a_signal_or_failure_is_a_crash() {
        assert!(!crashed(ExitStatus::from_raw(0)));
        assert!(crashed(ExitStatus::from_raw(101 << 8)));
        assert!(crashed(ExitStatus::from_raw(libc::SIGKILL)));
        assert_eq!(exit_code(ExitStatus::from_raw(101 << 8)), 101);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGKILL)), 128 + 9);
    }
}
