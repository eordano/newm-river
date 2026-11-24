//! A Wayland test client for `newm/e2e/inhibit.sh`: one solid xdg toplevel (app id
//! `e2e-inhibit`) that asks keyboard-shortcuts-inhibit-unstable-v1 to inhibit the compositor's
//! shortcuts for its surface, as a VM viewer or a remote desktop does. It prints one line per
//! event, flushed: `inhibitor active` / `inhibitor inactive`, `focus in` / `focus out`, and
//! `key CODE down|up` for every key the compositor sends it (Linux key codes).

use std::fs::File;
use std::io::Write as _;
use std::os::fd::{AsFd as _, FromRawFd as _};

use anyhow::{bail, Context as _, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager_v1,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct Probe {
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    size: (i32, i32),
    buffer: Option<wl_buffer::WlBuffer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    closed: bool,
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

impl Probe {
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let (w, h) = self.size;
        let bytes: Vec<u8> = 0xff30_60c0_u32.to_le_bytes().repeat((w * h) as usize);
        // SAFETY: memfd_create takes a NUL-terminated name and flags; a returned fd is ours.
        let fd = unsafe { libc::memfd_create(c"inhibit-probe".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh, owned file descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes)?;
        let pool = self
            .shm
            .create_pool(file.as_fd(), bytes.len() as i32, qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, w, h);
        self.surface.commit();
        if let Some(old) = self.buffer.replace(buffer) {
            old.destroy();
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn)?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=3, ())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=5, ())?;
    let manager: manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("zwp_keyboard_shortcuts_inhibit_manager_v1 missing")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg = base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("e2e-inhibit".into());
    toplevel.set_title("inhibit probe".into());
    surface.commit();
    let _inhibitor = manager.inhibit_shortcuts(&surface, &seat, &qh, ());

    let mut probe = Probe {
        shm,
        surface,
        size: (640, 400),
        buffer: None,
        keyboard: None,
        closed: false,
    };
    while !probe.closed {
        queue.blocking_dispatch(&mut probe)?;
    }
    Ok(())
}

delegate_noop!(Probe: ignore wl_compositor::WlCompositor);
delegate_noop!(Probe: ignore wl_surface::WlSurface);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
delegate_noop!(Probe: ignore manager_v1::ZwpKeyboardShortcutsInhibitManagerV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
    fn event(
        probe: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Err(e) = probe.draw(qh) {
                eprintln!("inhibit_probe: {e}");
                probe.closed = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                probe.size = (width, height);
            }
            xdg_toplevel::Event::Close => probe.closed = true,
            _ => {}
        }
    }
}

/// A headless river has a keyboard only while a virtual keyboard exists: take one when the seat
/// gains it (wl_seat.get_keyboard before that is a protocol error) and let it go when the seat
/// loses it (wlroots makes the old wl_keyboard inert), so each vinput run reaches a live one.
impl Dispatch<wl_seat::WlSeat, ()> for Probe {
    fn event(
        probe: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            let has = caps.contains(wl_seat::Capability::Keyboard);
            if has && probe.keyboard.is_none() {
                probe.keyboard = Some(seat.get_keyboard(qh, ()));
            } else if !has && let Some(keyboard) = probe.keyboard.take() {
                keyboard.release();
            }
        }
    }
}

impl Dispatch<inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            inhibitor_v1::Event::Active => say("inhibitor active"),
            inhibitor_v1::Event::Inactive => say("inhibitor inactive"),
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => say("focus in"),
            wl_keyboard::Event::Leave { .. } => say("focus out"),
            wl_keyboard::Event::Key { key, state, .. } => {
                let state = match state {
                    WEnum::Value(wl_keyboard::KeyState::Pressed) => "down",
                    _ => "up",
                };
                say(&format!("key {key} {state}"));
            }
            _ => {}
        }
    }
}
