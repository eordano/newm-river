//! A Wayland test client for the e2e (`newm/e2e/session.sh`), solid colours only:
//!
//! - `window --app-id A [--title T] [--size WxH] [--color RRGGBB] [--min WxH] [--max WxH]
//!   [--fullscreen] [--dialog CHILD_APP_ID [--dialog-size WxH]] [--map-delay MS]
//!   [--retitle T [--retitle-after MS]]`: an xdg toplevel (930x430 where the compositor leaves
//!   the size to it: 8x4 tiles of the e2e configs; size caps make it look like
//!   Picture-in-Picture; `--fullscreen` asks for fullscreen; `--dialog` opens a fixed-size child
//!   toplevel with the window as its parent once the window is drawn; `--map-delay` waits before
//!   creating the window; `--retitle` sets a new title once the window is drawn and
//!   `--retitle-after` (300) more milliseconds passed, as a terminal does once tmux attaches).
//! - `layer --namespace N [--anchor top|bottom|left|right] [--thickness PX] [--exclusive PX]
//!   [--color RRGGBB] [--keyboard none|exclusive|on-demand] [--layer top|bottom]`: a layer-shell
//!   surface along one edge, in the top layer unless asked, with an exclusive zone and the
//!   keyboard interactivity asked.
//! - window and layer both take `--log-keys`: prints `kb-enter SERIAL`, `kb-leave` and
//!   `kb-key KEY` lines as the surface gets them. `--log-pinch` prints `pinch-begin FINGERS`,
//!   `pinch-update SCALE` and `pinch-end` lines for the touchpad pinches it gets over
//!   pointer-gestures-v1. A window's `--token-out FILE` asks for an
//!   xdg-activation token with the serial of its first keyboard enter and writes it to FILE;
//!   `--token-after TRIGGER` asks for it only once TRIGGER exists, still with the serial of the
//!   last keyboard enter (a token made after the user moved on: stale);
//!   `--activate-from FILE` waits for FILE once drawn and activates the window with its token
//!   (one client's token activating another's window, as a link or a notification does).
//! - `png PATH WxH RRGGBB`: writes a solid PNG (a placeholder icon).
//! - `pixel FILE.ppm X Y`: prints the pixel of a `grim -t ppm` shot as `R G B`.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::os::fd::{AsFd as _, FromRawFd as _};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_pinch_v1, zwp_pointer_gestures_v1,
};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

struct Surface {
    wl: wl_surface::WlSurface,
    /// The size it draws when the compositor leaves the choice to it.
    own: (i32, i32),
    size: (i32, i32),
    color: u32,
    buffer: Option<wl_buffer::WlBuffer>,
}

struct App {
    shm: wl_shm::WlShm,
    surfaces: Vec<Surface>,
    closed: bool,
    /// `--log-keys`: keyboard events go to stdout.
    log_keys: bool,
    seat: Option<wl_seat::WlSeat>,
    /// `--log-pinch`: the pointer-gestures global, whose pinches go to stdout.
    gestures: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1>,
    /// `--token-out`: the activation global and where the token goes, once asked.
    token_out: Option<(xdg_activation_v1::XdgActivationV1, String)>,
    token_asked: bool,
    /// `--token-after`: the token waits for this file; the serial of the last keyboard enter.
    token_after: Option<String>,
    last_enter: Option<u32>,
}

impl App {
    /// Asks for an activation token with `serial`, once (`--token-out`).
    fn ask_token(&mut self, serial: u32, qh: &QueueHandle<Self>) {
        if self.token_asked {
            return;
        }
        if let (Some((activation, _)), Some(seat), Some(s)) =
            (&self.token_out, &self.seat, self.surfaces.first())
        {
            let token = activation.get_activation_token(qh, ());
            token.set_serial(serial, seat);
            token.set_surface(&s.wl);
            token.commit();
            self.token_asked = true;
        }
    }

    fn say(&self, line: &str) {
        if self.log_keys {
            println!("{line}");
            let _ = std::io::stdout().flush();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        app: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Keyboard) {
            seat.get_keyboard(qh, ());
        }
        if let Some(gestures) = &app.gestures
            && caps.contains(wl_seat::Capability::Pointer)
        {
            let pointer = seat.get_pointer(qh, ());
            gestures.get_pinch_gesture(&pointer, qh, ());
        }
    }
}

impl Dispatch<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
        event: zwp_pointer_gesture_pinch_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_pointer_gesture_pinch_v1::Event;
        let line = match event {
            Event::Begin { fingers, .. } => format!("pinch-begin {fingers}"),
            Event::Update { scale, .. } => format!("pinch-update {scale:.3}"),
            Event::End { cancelled, .. } => format!("pinch-end {cancelled}"),
            _ => return,
        };
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
}

delegate_noop!(App: ignore wl_pointer::WlPointer);
delegate_noop!(App: ignore zwp_pointer_gestures_v1::ZwpPointerGesturesV1);

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { serial, .. } => {
                app.say(&format!("kb-enter {serial}"));
                app.last_enter = Some(serial);
                if app.token_after.is_none() {
                    app.ask_token(serial, qh);
                }
            }
            wl_keyboard::Event::Leave { .. } => app.say("kb-leave"),
            wl_keyboard::Event::Key {
                key,
                state: wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => app.say(&format!("kb-key {key}")),
            _ => {}
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event
            && let Some((_, path)) = &app.token_out
            && let Err(e) = std::fs::write(path, token)
        {
            eprintln!("e2e_client: {path}: {e}");
        }
    }
}

delegate_noop!(App: ignore xdg_activation_v1::XdgActivationV1);

impl App {
    fn draw(&mut self, k: usize, qh: &QueueHandle<Self>) -> Result<()> {
        let s = &mut self.surfaces[k];
        let (w, h) = s.size;
        let bytes: Vec<u8> = s.color.to_le_bytes().repeat((w * h) as usize);
        // SAFETY: memfd_create takes a NUL-terminated name and flags; a returned fd is ours.
        let fd = unsafe { libc::memfd_create(c"e2e-client".as_ptr(), libc::MFD_CLOEXEC) };
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
        s.wl.attach(Some(&buffer), 0, 0);
        s.wl.damage(0, 0, w, h);
        s.wl.commit();
        if let Some(old) = s.buffer.replace(buffer) {
            old.destroy();
        }
        Ok(())
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
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

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
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

impl Dispatch<xdg_surface::XdgSurface, usize> for App {
    fn event(
        app: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        k: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Err(e) = app.draw(*k, qh) {
                eprintln!("e2e_client: {e}");
                app.closed = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        k: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                let s = &mut app.surfaces[*k];
                s.size = (
                    if width > 0 { width } else { s.own.0 },
                    if height > 0 { height } else { s.own.1 },
                );
            }
            xdg_toplevel::Event::Close => app.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for App {
    fn event(
        app: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        k: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                surface.ack_configure(serial);
                let s = &mut app.surfaces[*k];
                s.size = (
                    if width > 0 { width as i32 } else { s.own.0 },
                    if height > 0 { height as i32 } else { s.own.1 },
                );
                if let Err(e) = app.draw(*k, qh) {
                    eprintln!("e2e_client: {e}");
                    app.closed = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => app.closed = true,
            _ => {}
        }
    }
}

fn opt(args: &[String], name: &str) -> Option<String> {
    let k = args.iter().position(|a| a == name)?;
    args.get(k + 1).cloned()
}

fn size(s: &str) -> Result<(i32, i32)> {
    let (w, h) = s.split_once('x').context("a size is WxH")?;
    Ok((w.parse()?, h.parse()?))
}

fn color(s: &str) -> Result<u32> {
    Ok(0xff00_0000 | u32::from_str_radix(s.trim_start_matches('#'), 16)?)
}

fn toplevel(
    app: &mut App,
    compositor: &wl_compositor::WlCompositor,
    base: &xdg_wm_base::XdgWmBase,
    qh: &QueueHandle<App>,
    app_id: &str,
    own: (i32, i32),
    color: u32,
) -> xdg_toplevel::XdgToplevel {
    let k = app.surfaces.len();
    let wl = compositor.create_surface(qh, ());
    let xdg = base.get_xdg_surface(&wl, qh, k);
    let top = xdg.get_toplevel(qh, k);
    top.set_app_id(app_id.into());
    top.set_title(app_id.into());
    app.surfaces.push(Surface {
        wl,
        own,
        size: own,
        color,
        buffer: None,
    });
    top
}

fn run(args: &[String]) -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let log_keys = args.iter().any(|a| a == "--log-keys");
    let token_path = opt(args, "--token-out");
    let activate_path = opt(args, "--activate-from");
    let activation: Option<xdg_activation_v1::XdgActivationV1> =
        if token_path.is_some() || activate_path.is_some() {
            Some(globals.bind(&qh, 1..=1, ())?)
        } else {
            None
        };
    let gestures: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1> =
        if args.iter().any(|a| a == "--log-pinch") {
            Some(globals.bind(&qh, 1..=3, ())?)
        } else {
            None
        };
    let seat: Option<wl_seat::WlSeat> = if log_keys || token_path.is_some() || gestures.is_some() {
        Some(globals.bind(&qh, 1..=7, ())?)
    } else {
        None
    };
    let mut app = App {
        shm,
        surfaces: Vec::new(),
        closed: false,
        log_keys,
        seat,
        gestures,
        token_out: token_path.and_then(|p| activation.clone().map(|a| (a, p))),
        token_asked: false,
        token_after: opt(args, "--token-after"),
        last_enter: None,
    };
    let paint = color(&opt(args, "--color").unwrap_or_else(|| "ff0000".into()))?;
    match args.first().map(String::as_str) {
        Some("window") => {
            let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;
            let app_id = opt(args, "--app-id").context("--app-id")?;
            let own = opt(args, "--size").map_or(Ok((930, 430)), |s| size(&s))?;
            if let Some(ms) = opt(args, "--map-delay") {
                std::thread::sleep(Duration::from_millis(ms.parse()?));
            }
            let top = toplevel(&mut app, &compositor, &base, &qh, &app_id, own, paint);
            if let Some(title) = opt(args, "--title") {
                top.set_title(title);
            }
            if let Some(min) = opt(args, "--min") {
                let (w, h) = size(&min)?;
                top.set_min_size(w, h);
            }
            if let Some(max) = opt(args, "--max") {
                let (w, h) = size(&max)?;
                top.set_max_size(w, h);
            }
            if args.iter().any(|a| a == "--fullscreen") {
                top.set_fullscreen(None);
            }
            app.surfaces[0].wl.commit();
            if let Some(child) = opt(args, "--dialog") {
                while app.surfaces[0].buffer.is_none() && !app.closed {
                    queue.blocking_dispatch(&mut app)?;
                }
                let own = opt(args, "--dialog-size").map_or(Ok((300, 200)), |s| size(&s))?;
                let dialog = toplevel(&mut app, &compositor, &base, &qh, &child, own, paint);
                dialog.set_parent(Some(&top));
                dialog.set_min_size(own.0, own.1);
                dialog.set_max_size(own.0, own.1);
                app.surfaces[1].wl.commit();
            }
            if let (Some(path), Some(activation)) = (&activate_path, &activation) {
                while app.surfaces[0].buffer.is_none() && !app.closed {
                    queue.blocking_dispatch(&mut app)?;
                }
                let token = loop {
                    queue.roundtrip(&mut app)?;
                    match std::fs::read_to_string(path) {
                        Ok(t) if !t.is_empty() => break t,
                        _ => std::thread::sleep(Duration::from_millis(50)),
                    }
                };
                activation.activate(token, &app.surfaces[0].wl);
                conn.flush()?;
                app.say("activated");
            }
            if let Some(title) = opt(args, "--retitle") {
                while app.surfaces[0].buffer.is_none() && !app.closed {
                    queue.blocking_dispatch(&mut app)?;
                }
                let ms = opt(args, "--retitle-after").map_or(Ok(300), |t| t.parse())?;
                std::thread::sleep(Duration::from_millis(ms));
                top.set_title(title);
                conn.flush()?;
            }
        }
        Some("layer") => {
            let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
            use zwlr_layer_surface_v1::Anchor;
            let thickness: i32 = opt(args, "--thickness").map_or(Ok(40), |t| t.parse())?;
            let exclusive: i32 = opt(args, "--exclusive").map_or(Ok(thickness), |t| t.parse())?;
            let edge = opt(args, "--anchor").unwrap_or_else(|| "top".into());
            let (anchor, own) = match edge.as_str() {
                "top" => (Anchor::Top | Anchor::Left | Anchor::Right, (0, thickness)),
                "bottom" => (
                    Anchor::Bottom | Anchor::Left | Anchor::Right,
                    (0, thickness),
                ),
                "left" => (Anchor::Left | Anchor::Top | Anchor::Bottom, (thickness, 0)),
                "right" => (Anchor::Right | Anchor::Top | Anchor::Bottom, (thickness, 0)),
                other => bail!("no anchor {other}"),
            };
            let wl = compositor.create_surface(&qh, ());
            let namespace = opt(args, "--namespace").unwrap_or_else(|| "e2e".into());
            let which = match opt(args, "--layer").as_deref() {
                None | Some("top") => zwlr_layer_shell_v1::Layer::Top,
                Some("bottom") => zwlr_layer_shell_v1::Layer::Bottom,
                Some(other) => bail!("no layer {other}"),
            };
            let layer = shell.get_layer_surface(&wl, None, which, namespace, &qh, 0);
            layer.set_anchor(anchor);
            layer.set_size(own.0 as u32, own.1 as u32);
            layer.set_exclusive_zone(exclusive);
            use zwlr_layer_surface_v1::KeyboardInteractivity;
            match opt(args, "--keyboard").as_deref() {
                None | Some("none") => {}
                Some("exclusive") => {
                    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive)
                }
                Some("on-demand") => {
                    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand)
                }
                Some(other) => bail!("no keyboard interactivity {other}"),
            }
            wl.commit();
            app.surfaces.push(Surface {
                wl,
                own: (own.0.max(1), own.1.max(1)),
                size: own,
                color: paint,
                buffer: None,
            });
        }
        _ => bail!("e2e_client window|layer|png|pixel ..."),
    }
    if let Some(trigger) = app.token_after.clone() {
        while !app.closed && !app.token_asked {
            queue.roundtrip(&mut app)?;
            if std::path::Path::new(&trigger).exists()
                && let Some(serial) = app.last_enter
            {
                app.ask_token(serial, &qh);
                conn.flush()?;
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    while !app.closed {
        queue.blocking_dispatch(&mut app)?;
    }
    Ok(())
}

fn png(args: &[String]) -> Result<()> {
    let [path, dims, rgb] = args else {
        bail!("png PATH WxH RRGGBB");
    };
    let (w, h) = size(dims)?;
    let c = color(rgb)?.to_be_bytes();
    let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&[c[1], c[2], c[3]].repeat((w * h) as usize))?;
    Ok(())
}

struct Clicker;

delegate_noop!(Clicker: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(Clicker: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Clicker {
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

/// A left click at (X, Y) of a W x H layout through a wlr virtual pointer.
fn click(args: &[String]) -> Result<()> {
    let [x, y, w, h] = args else {
        bail!("click X Y W H");
    };
    let [x, y, w, h]: [u32; 4] = [x.parse()?, y.parse()?, w.parse()?, h.parse()?];
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Clicker>(&conn)?;
    let qh = queue.handle();
    let manager: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 =
        globals.bind(&qh, 1..=2, ())?;
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    pointer.motion_absolute(0, x, y, w, h);
    pointer.frame();
    queue.roundtrip(&mut Clicker)?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    let presses = [
        (60, wl_pointer::ButtonState::Pressed),
        (120, wl_pointer::ButtonState::Released),
    ];
    for (t, state) in presses {
        pointer.button(t, 272, state);
        pointer.frame();
        queue.roundtrip(&mut Clicker)?;
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    pointer.destroy();
    queue.roundtrip(&mut Clicker)?;
    Ok(())
}

fn pixel(args: &[String]) -> Result<()> {
    let [path, x, y] = args else {
        bail!("pixel FILE.ppm X Y");
    };
    let (x, y): (usize, usize) = (x.parse()?, y.parse()?);
    let data = std::fs::read(path)?;
    // P6, width, height, maxval, then one whitespace byte before the pixels.
    let mut fields = Vec::new();
    let mut k = 0;
    while fields.len() < 4 {
        while data.get(k).context("short header")?.is_ascii_whitespace() {
            k += 1;
        }
        let start = k;
        while !data.get(k).context("short header")?.is_ascii_whitespace() {
            k += 1;
        }
        fields.push(std::str::from_utf8(&data[start..k])?.to_owned());
    }
    if fields[0] != "P6" {
        bail!("not a binary PPM");
    }
    let w: usize = fields[1].parse()?;
    let at = k + 1 + (y * w + x) * 3;
    let p = data.get(at..at + 3).context("pixel outside the image")?;
    println!("{} {} {}", p[0], p[1], p[2]);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("png") => png(&args[1..]),
        Some("pixel") => pixel(&args[1..]),
        Some("click") => click(&args[1..]),
        _ => run(&args),
    }
}
