//! Virtual input for the headless e2e (`e2e/headless.sh`, which has no input devices): a keyboard
//! over zwp-virtual-keyboard-v1 and a pointer over wlr-virtual-pointer-v1, driven by steps on the
//! command line, in order:
//!
//! - `abs X Y W H`: the pointer to (X, Y) of a W x H output layout
//! - `move DX DY`: relative pointer motion
//! - `button CODE down|up`: a Linux button code (272 is BTN_LEFT)
//! - `wheel N`: N wheel notches down (negative: up)
//! - `super down|up`: the left Super key with its modifier (virtual keyboards send modifiers
//!   themselves)
//! - `key CODE down|up`: a Linux key code (1 is Escape, 28 Enter), the modifiers unchanged
//! - `sleep MS`
//!
//! Every step is flushed and round-tripped, so river has it before the next one.

mod vk {
    #![allow(dead_code, non_upper_case_globals, non_camel_case_types, clippy::all)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "../../protocol/upstream/virtual-keyboard-unstable-v1.xml"
        );
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!(
        "../../protocol/upstream/virtual-keyboard-unstable-v1.xml"
    );
}

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use vk::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use vk::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use xkbcommon::xkb;

/// KEY_LEFTMETA.
const KEY_LEFTMETA: u32 = 125;
/// Mod4 in the "us" keymap's modifier mask.
const MOD4: u32 = 1 << 6;

struct S;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S {
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

delegate_noop!(S: ignore wl_seat::WlSeat);
delegate_noop!(S: ZwpVirtualKeyboardManagerV1);
delegate_noop!(S: ZwpVirtualKeyboardV1);
delegate_noop!(S: ZwlrVirtualPointerManagerV1);
delegate_noop!(S: ZwlrVirtualPointerV1);

fn keymap_fd(text: &str) -> Result<(File, u32)> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    // SAFETY: a valid NUL-terminated name and no flags.
    let fd = unsafe { libc::memfd_create(c"vinput-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create: {}", std::io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor this function owns.
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.write_all(&bytes)?;
    Ok((f, bytes.len() as u32))
}

fn num<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, what: &str) -> Result<T> {
    let s = args.next().with_context(|| format!("missing {what}"))?;
    s.parse().ok().with_context(|| format!("bad {what}: {s}"))
}

fn pressed(args: &mut impl Iterator<Item = String>) -> Result<bool> {
    match args.next().as_deref() {
        Some("down") => Ok(true),
        Some("up") => Ok(false),
        other => bail!("expected down or up, got {other:?}"),
    }
}

fn main() -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<S>(&conn)?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ())?;
    let kbm: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let pm: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ())?;
    let kb = kbm.create_virtual_keyboard(&seat, &qh, ());
    let ptr = pm.create_virtual_pointer(Some(&seat), &qh, ());

    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::Keymap::new_from_names(&ctx, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .context("no \"us\" keymap")?;
    let (file, size) = keymap_fd(&keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))?;
    kb.keymap(xkb::KEYMAP_FORMAT_TEXT_V1, file.as_fd(), size);
    queue.roundtrip(&mut S)?;

    let start = Instant::now();
    let time = || start.elapsed().as_millis() as u32;
    let mut args = std::env::args().skip(1);
    while let Some(step) = args.next() {
        match step.as_str() {
            "abs" => {
                let (x, y): (u32, u32) = (num(&mut args, "x")?, num(&mut args, "y")?);
                let (w, h): (u32, u32) = (num(&mut args, "width")?, num(&mut args, "height")?);
                ptr.motion_absolute(time(), x, y, w, h);
                ptr.frame();
            }
            "move" => {
                let (dx, dy): (f64, f64) = (num(&mut args, "dx")?, num(&mut args, "dy")?);
                ptr.motion(time(), dx, dy);
                ptr.frame();
            }
            "button" => {
                let code: u32 = num(&mut args, "button")?;
                let state = if pressed(&mut args)? {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                ptr.button(time(), code, state);
                ptr.frame();
            }
            "wheel" => {
                let n: i32 = num(&mut args, "notches")?;
                ptr.axis_source(wl_pointer::AxisSource::Wheel);
                ptr.axis_discrete(
                    time(),
                    wl_pointer::Axis::VerticalScroll,
                    15. * f64::from(n),
                    n,
                );
                ptr.frame();
            }
            "super" => {
                let down = pressed(&mut args)?;
                kb.key(time(), KEY_LEFTMETA, u32::from(down));
                kb.modifiers(if down { MOD4 } else { 0 }, 0, 0, 0);
            }
            "key" => {
                let code: u32 = num(&mut args, "key")?;
                let down = pressed(&mut args)?;
                kb.key(time(), code, u32::from(down));
            }
            "sleep" => std::thread::sleep(Duration::from_millis(num(&mut args, "ms")?)),
            other => bail!("unknown step {other}"),
        }
        queue.roundtrip(&mut S)?;
    }
    ptr.destroy();
    kb.destroy();
    queue.roundtrip(&mut S)?;
    Ok(())
}
