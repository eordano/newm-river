//! The window manager's own surfaces and services beside the windows:
//! - the overview's dim: shell surfaces showing one translucent black pixel stretched over
//!   the bands outside the screen area, above the windows and taking no input;
//! - the Alt+Tab switcher: its key bindings, Alt's release through `modifiers_watch`,
//!   and the panel of thumbnails (captured off the main thread) and app icons;
//! - the toplevel identifiers, the pointer and the scene ScreenShot2 picks its targets from.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use newm_canvas::Id;
use niri_config::Config;
use niri_ipc::Response;
use tracing::{debug, warn};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{wl_buffer, wl_region, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::{shm_buffer, State};
use crate::capture::{Capturer, Source};
use crate::draw::Pixmap;
use crate::model::{Desktop, Frame, Rect, ViewSettings};
use crate::protocols::window_management::river_node_v1::RiverNodeV1;
use crate::protocols::window_management::river_seat_v1;
use crate::protocols::window_management::river_shell_surface_v1::RiverShellSurfaceV1;
use crate::protocols::xkb_bindings::river_xkb_binding_v1::{self, RiverXkbBindingV1};
use crate::screenshot::{Picked, Scene, SceneOutput, SceneWindow};
use crate::switcher::{self, Step, Switcher};
use crate::{glow, icons};

mod overlay;

/// The most a thumbnail keeps of a window, scaled down.
const THUMB: (i32, i32) = (480, 300);
const THUMB_TIMEOUT: Duration = Duration::from_millis(500);
/// Frames while a camera moves.
const FRAME: Duration = Duration::from_millis(16);

const KEY_TAB: u32 = 0xff09;
const KEY_ISO_LEFT_TAB: u32 = 0xfe20;
const KEY_ESCAPE: u32 = 0xff1b;

/// A switcher key, carried by its binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Key {
    Next,
    Prev,
    Cancel,
}

/// A shell surface and its node.
struct Surface {
    wl: wl_surface::WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    viewport: Option<WpViewport>,
    buffer: Option<wl_buffer::WlBuffer>,
    mapped: bool,
    size: (i32, i32),
}

impl Surface {
    /// Unmaps it in this render sequence.
    fn unmap(&mut self) {
        if self.mapped {
            self.shell.sync_next_commit();
            self.wl.attach(None, 0, 0);
            self.wl.commit();
            self.mapped = false;
        }
    }
}

/// The switcher bindings of one seat.
struct SeatKeys {
    bindings: Vec<RiverXkbBindingV1>,
    cancel: Vec<RiverXkbBindingV1>,
}

pub(super) struct ViewState {
    /// `river_window_v1.identifier` per window: its ext-foreign-toplevel-list identifier.
    pub(super) identifiers: HashMap<Id, String>,
    /// The pointer in the global logical space, as of the last manage sequence.
    pub(super) pointer: Option<(i32, i32)>,
    scene: Arc<Mutex<Scene>>,
    viewporter: Option<WpViewporter>,
    pixels: Option<WpSinglePixelBufferManagerV1>,
    dim_buffer: Option<(u32, wl_buffer::WlBuffer)>,
    bands: Vec<Surface>,
    /// Alt+Tab is bound (`recent-windows` on).
    recent: bool,
    seat_keys: HashMap<ObjectId, SeatKeys>,
    pub(super) keys: Vec<Key>,
    alt_released: bool,
    cancel: bool,
    switcher: Option<Switcher>,
    switcher_output: Option<u32>,
    panel: Option<Surface>,
    panel_dirty: bool,
    thumbs: HashMap<Id, Pixmap>,
    icons: HashMap<String, Option<Pixmap>>,
    generation: u64,
    thumb_tx: Sender<(u64, Id, Pixmap)>,
    thumb_rx: Receiver<(u64, Id, Pixmap)>,
    wake: UnixStream,
    waker: UnixStream,
    last_frame: Instant,
    overlay: overlay::Overlay,
    /// The last fresh pointer a ScreenShot2 capture asked for, seen by the loop.
    pointer_seen: u64,
    /// The ask the pointer answers: `pointer_seen` at the start of the last manage sequence.
    pointer_fresh: u64,
    /// The last pick `queryWindowInfo` asked for that the pointer started.
    pick_started: u64,
}

/// The camera and overview settings from the config.
pub(super) fn view_settings(config: &Config) -> ViewSettings {
    let a = &config.animations;
    let slow = |ms: u64| {
        if a.off {
            Duration::ZERO
        } else {
            Duration::from_millis(ms).mul_f64(a.slowdown.clamp(0., 20.))
        }
    };
    // An easing animation takes its own duration; a spring, newm-river's curve's.
    let anim = |anim: &niri_config::animations::Animation, spring_ms: u64| {
        if anim.off {
            return Duration::ZERO;
        }
        match anim.kind {
            niri_config::animations::Kind::Easing(e) => slow(u64::from(e.duration_ms)),
            niri_config::animations::Kind::Spring(_) => slow(spring_ms),
        }
    };
    ViewSettings {
        dim: config.layout.canvas.overview_dim,
        center_on_switch: config.layout.canvas.center_on_switch,
        duration: slow(250),
        movement: anim(&a.window_movement.0, 250),
        open: anim(&a.window_open.anim, 150),
        close: anim(&a.window_close.anim, 150),
    }
}

impl ViewState {
    pub(super) fn new(
        config: &Config,
        globals: &GlobalList,
        qh: &QueueHandle<State>,
    ) -> std::io::Result<Self> {
        let (wake, waker) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        waker.set_nonblocking(true)?;
        let (thumb_tx, thumb_rx) = channel();
        let viewporter = globals.bind(qh, 1..=1, ()).ok();
        let pixels = globals.bind(qh, 1..=1, ()).ok();
        if viewporter.is_none() || pixels.is_none() {
            warn!("wp_viewporter or wp_single_pixel_buffer_manager_v1 missing: no overview dim");
        }
        Ok(Self {
            identifiers: HashMap::new(),
            pointer: None,
            scene: Arc::default(),
            viewporter,
            pixels,
            dim_buffer: None,
            bands: Vec::new(),
            recent: config.recent_windows.on,
            seat_keys: HashMap::new(),
            keys: Vec::new(),
            alt_released: false,
            cancel: false,
            switcher: None,
            switcher_output: None,
            panel: None,
            panel_dirty: false,
            thumbs: HashMap::new(),
            icons: HashMap::new(),
            generation: 0,
            thumb_tx,
            thumb_rx,
            wake,
            waker,
            last_frame: Instant::now(),
            overlay: overlay::Overlay::default(),
            pointer_seen: 0,
            pointer_fresh: 0,
            pick_started: 0,
        })
    }

    pub(super) fn scene(&self) -> Arc<Mutex<Scene>> {
        self.scene.clone()
    }

    /// A handle that wakes the loop, for ScreenShot2's pointer asks.
    pub(super) fn waker(&self) -> Option<UnixStream> {
        self.waker.try_clone().ok()
    }

    /// A manage sequence starts: river sent the pointer before it.
    pub(super) fn pointer_reported(&mut self) {
        self.pointer_fresh = self.pointer_seen;
    }

    /// The fd the poll loop watches for thumbnails.
    pub(super) fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// A window closed.
    pub(super) fn forget(&mut self, id: Id) {
        self.identifiers.remove(&id);
        self.thumbs.remove(&id);
        if let Some(s) = &mut self.switcher {
            self.panel_dirty = true;
            if !s.remove(id) {
                self.cancel = true;
            }
        }
    }
}

impl State {
    /// When the poll loop must wake for the shell: the next animation frame, or the switcher's
    /// panel coming due.
    pub(super) fn view_deadline(&self) -> Option<Instant> {
        let frame =
            (self.desktop.animating() || self.closing()).then(|| self.view.last_frame + FRAME);
        let panel = self
            .view
            .switcher
            .as_ref()
            .filter(|_| !self.view.panel.as_ref().is_some_and(|p| p.mapped))
            .map(|s| s.shows_at(switcher::OPEN_DELAY));
        frame
            .into_iter()
            .chain(panel)
            .chain(self.overlay_deadline())
            .min()
    }

    /// After a poll: takes the thumbnails that came and says whether a manage sequence is due.
    pub(super) fn view_tick(&mut self, now: Instant) -> bool {
        let mut buf = [0u8; 64];
        while matches!((&self.view.wake).read(&mut buf), Ok(n) if n > 0) {}
        let mut dirty = false;
        let (asked, pick) = self
            .view
            .scene
            .lock()
            .map_or((0, 0), |s| (s.pointer_asked, s.pick_asked));
        if asked > self.view.pointer_seen {
            self.view.pointer_seen = asked;
            dirty = true;
        }
        if pick > self.view.pick_started {
            self.view.pick_started = pick;
            self.pointer.start_pick();
            dirty = true;
        }
        if let Some(at) = self.desktop.picked.take() {
            let picked = match at {
                Some((x, y)) => match self.desktop.window_at(x, y) {
                    Some(id) => Picked::Window(id),
                    None => self
                        .desktop
                        .output_at(x, y)
                        .and_then(|o| self.desktop.output(o))
                        .map_or(Picked::Nothing, |o| Picked::Output(o.name.clone())),
                },
                None => Picked::Nothing,
            };
            if let Some(reply) = self.pick_reply.take() {
                let window = match &picked {
                    Picked::Window(id) => {
                        self.desktop.window(*id).map(|w| self.desktop.ipc_window(w))
                    }
                    _ => None,
                };
                let _ = reply.send(Ok(Response::PickedWindow(window)));
            }
            if let Ok(mut s) = self.view.scene.lock() {
                s.picked = Some((self.view.pick_started, picked));
            }
        }
        while let Ok((generation, id, pixmap)) = self.view.thumb_rx.try_recv() {
            if generation == self.view.generation {
                self.view.thumbs.insert(id, pixmap);
                self.view.panel_dirty = true;
                dirty = true;
            }
        }
        dirty || self.view_deadline().is_some_and(|d| now >= d)
    }

    // ---- manage --------------------------------------------------------------------------

    /// The switcher's part of a manage sequence: bindings for new seats, keys pressed, Alt
    /// released. Runs before the focus is sent, so a confirmed window gets it in this sequence.
    pub(super) fn view_manage(&mut self, qh: &QueueHandle<State>) {
        let now = Instant::now();
        self.overlay_manage(qh);
        self.bind_switch_keys(qh);
        for key in std::mem::take(&mut self.view.keys) {
            match (key, &mut self.view.switcher) {
                (Key::Cancel, Some(_)) => self.view.cancel = true,
                (Key::Cancel, None) => {}
                (Key::Next, Some(s)) => {
                    s.step(Step::Next);
                    self.view.panel_dirty = true;
                }
                (Key::Prev, Some(s)) => {
                    s.step(Step::Prev);
                    self.view.panel_dirty = true;
                }
                (Key::Next, None) => self.open_switcher(Step::Next, now),
                (Key::Prev, None) => self.open_switcher(Step::Prev, now),
            }
        }
        if std::mem::take(&mut self.view.cancel) {
            self.close_switcher(None, now);
        }
        if std::mem::take(&mut self.view.alt_released) {
            let choice = self.view.switcher.as_ref().map(Switcher::selected);
            if choice.is_some() {
                self.close_switcher(choice, now);
            }
        }
    }

    fn bind_switch_keys(&mut self, qh: &QueueHandle<State>) {
        let gone: Vec<ObjectId> = self
            .view
            .seat_keys
            .keys()
            .filter(|k| !self.seats.contains_key(k))
            .cloned()
            .collect();
        for k in gone {
            if let Some(sk) = self.view.seat_keys.remove(&k) {
                for b in sk.bindings.into_iter().chain(sk.cancel) {
                    b.destroy();
                }
            }
        }
        let (Some(xkb), true) = (&self.xkb_bindings, self.view.recent) else {
            return;
        };
        use river_seat_v1::Modifiers as M;
        for (key, seat) in &self.seats {
            if self.view.seat_keys.contains_key(key) {
                continue;
            }
            let bind = |sym: u32, mods: M, k: Key| {
                let b = xkb.get_xkb_binding(&seat.proxy, sym, mods, qh, k);
                if k != Key::Cancel {
                    b.enable();
                }
                b
            };
            let bindings = vec![
                bind(KEY_TAB, M::Mod1, Key::Next),
                bind(KEY_TAB, M::Mod1 | M::Shift, Key::Prev),
                bind(KEY_ISO_LEFT_TAB, M::Mod1 | M::Shift, Key::Prev),
            ];
            let cancel = vec![bind(KEY_ESCAPE, M::Mod1, Key::Cancel)];
            self.view
                .seat_keys
                .insert(key.clone(), SeatKeys { bindings, cancel });
        }
    }

    /// Alt was released: the seat's xkb-bindings object (`pointer_wl.rs`, which watches Super,
    /// Shift, Ctrl and Alt; river allows one per seat) saw it.
    pub(crate) fn alt_released(&mut self) {
        self.view.alt_released = true;
    }

    /// Takes Alt+Escape while the switcher is open, and lets it go after.
    fn switcher_keys(&self, open: bool) {
        for sk in self.view.seat_keys.values() {
            for b in &sk.cancel {
                if open {
                    b.enable();
                } else {
                    b.disable();
                }
            }
        }
    }

    fn open_switcher(&mut self, step: Step, now: Instant) {
        let Some(s) = Switcher::open(self.desktop.mru_windows(), step, now) else {
            return;
        };
        self.switcher_keys(true);
        self.view.generation += 1;
        self.view.switcher_output = self.desktop.active_output();
        let mut jobs = Vec::new();
        for &id in s.list() {
            if let Some(app) = self.desktop.window(id).and_then(|w| w.app_id.clone())
                && !self.view.icons.contains_key(&app)
            {
                let icon = icons::app_icon(&app, 64);
                self.view.icons.insert(app, icon);
            }
            if let Some(ident) = self.view.identifiers.get(&id) {
                jobs.push((id, ident.clone()));
            }
        }
        self.view.switcher = Some(s);
        self.view.panel_dirty = true;
        let (tx, generation) = (self.view.thumb_tx.clone(), self.view.generation);
        let Ok(waker) = self.view.waker.try_clone() else {
            return;
        };
        let res = thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || thumbnails(jobs, generation, &tx, waker));
        if let Err(e) = res {
            warn!("thumbnail thread: {e}");
        }
    }

    fn close_switcher(&mut self, choice: Option<Id>, now: Instant) {
        if self.view.switcher.take().is_none() {
            return;
        }
        self.switcher_keys(false);
        if let Some(id) = choice {
            debug!("switch to {id}");
            self.desktop.switch_to(id, now);
        }
    }

    // ---- render --------------------------------------------------------------------------

    /// The view's part of a render sequence, after the windows: the scene for ScreenShot2, the
    /// dim bands above the windows and the switcher's panel above them.
    pub(super) fn view_render(&mut self, frames: &[Frame], qh: &QueueHandle<State>) {
        let now = Instant::now();
        self.view.last_frame = now;
        let mut scene = scene(
            &self.desktop,
            frames,
            &self.view.identifiers,
            self.view.pointer,
        );
        scene.locked = self.locked;
        scene.pointer_seq = self.view.pointer_fresh;
        if let Ok(mut s) = self.view.scene.lock() {
            scene.pointer_asked = s.pointer_asked;
            scene.pick_asked = s.pick_asked;
            scene.picked = s.picked.take();
            *s = scene;
        }
        self.render_dim(qh);
        self.render_panel(qh, now);
        self.render_overlays(qh, now);
    }

    fn new_surface(&self, qh: &QueueHandle<State>, scaled: bool) -> Option<Surface> {
        let compositor = self.compositor.as_ref()?;
        let viewporter = if scaled {
            Some(self.view.viewporter.as_ref()?)
        } else {
            None
        };
        let wl = compositor.create_surface(qh, ());
        let shell = self.wm.get_shell_surface(&wl, qh, ());
        let node = shell.get_node(qh, ());
        let region = compositor.create_region(qh, ());
        wl.set_input_region(Some(&region));
        region.destroy();
        let viewport = viewporter.map(|v| v.get_viewport(&wl, qh, ()));
        Some(Surface {
            wl,
            shell,
            node,
            viewport,
            buffer: None,
            mapped: false,
            size: (0, 0),
        })
    }

    fn render_dim(&mut self, qh: &QueueHandle<State>) {
        let (dim, bands) = self.desktop.dim();
        if !bands.is_empty()
            && let (Some(pixels), Some(_)) = (&self.view.pixels, &self.view.viewporter)
        {
            let alpha = (dim.clamp(0., 1.) * f64::from(u32::MAX)) as u32;
            if self.view.dim_buffer.as_ref().map(|(a, _)| *a) != Some(alpha) {
                let buffer = pixels.create_u32_rgba_buffer(0, 0, 0, alpha, qh, ());
                if let Some((_, old)) = self.view.dim_buffer.replace((alpha, buffer)) {
                    // The dim fades with the overview: mapped bands take the new buffer.
                    for s in &mut self.view.bands {
                        s.size = (0, 0);
                    }
                    old.destroy();
                }
            }
            while self.view.bands.len() < bands.len() {
                let Some(s) = self.new_surface(qh, true) else {
                    break;
                };
                self.view.bands.push(s);
            }
        }
        let buffer = self.view.dim_buffer.as_ref().map(|(_, b)| b.clone());
        for (k, s) in self.view.bands.iter_mut().enumerate() {
            let (Some(r), Some(buffer)) = (bands.get(k), &buffer) else {
                s.unmap();
                continue;
            };
            s.node.set_position(r.x, r.y);
            s.node.place_top();
            if !s.mapped || s.size != (r.w, r.h) {
                if let Some(v) = &s.viewport {
                    v.set_destination(r.w, r.h);
                }
                s.shell.sync_next_commit();
                s.wl.attach(Some(buffer), 0, 0);
                s.wl.damage_buffer(0, 0, 1, 1);
                s.wl.commit();
                s.mapped = true;
                s.size = (r.w, r.h);
            }
        }
    }

    fn render_panel(&mut self, qh: &QueueHandle<State>, now: Instant) {
        let due = self
            .view
            .switcher
            .as_ref()
            .is_some_and(|s| now >= s.shows_at(switcher::OPEN_DELAY));
        if !due {
            if let Some(p) = &mut self.view.panel {
                p.unmap();
            }
            return;
        }
        let Some(area) = self
            .view
            .switcher_output
            .and_then(|o| self.desktop.output(o))
            .or_else(|| self.desktop.outputs().first())
            .map(|o| o.area)
        else {
            return;
        };
        if self.view.panel.is_none() {
            self.view.panel = self.new_surface(qh, false);
        }
        let mapped = self.view.panel.as_ref().is_some_and(|p| p.mapped);
        if self.view.panel_dirty || !mapped {
            self.view.panel_dirty = false;
            let (Some(pixmap), Some(shm)) = (self.draw_panel(area), self.shm.clone()) else {
                return;
            };
            let b = glow::Buffer {
                width: pixmap.width,
                height: pixmap.height,
                offset: (0, 0),
                pixels: pixmap.to_bytes(),
            };
            let buffer = match shm_buffer(&shm, &b, qh) {
                Ok(buffer) => buffer,
                Err(e) => {
                    warn!("switcher buffer: {e}");
                    return;
                }
            };
            let Some(p) = &mut self.view.panel else {
                return;
            };
            p.shell.sync_next_commit();
            p.wl.attach(Some(&buffer), 0, 0);
            p.wl.damage_buffer(0, 0, b.width, b.height);
            p.wl.commit();
            p.mapped = true;
            p.size = (b.width, b.height);
            if let Some(old) = p.buffer.replace(buffer) {
                old.destroy();
            }
        }
        if let Some(p) = &self.view.panel {
            p.node.set_position(
                area.x + (area.w - p.size.0) / 2,
                area.y + (area.h - p.size.1) / 2,
            );
            p.node.place_top();
        }
    }

    fn draw_panel(&self, area: Rect) -> Option<Pixmap> {
        let s = self.view.switcher.as_ref()?;
        let sizes: Vec<(i32, i32)> = s
            .list()
            .iter()
            .map(|&id| {
                self.desktop
                    .window(id)
                    .and_then(|w| w.size)
                    .unwrap_or((16, 10))
            })
            .collect();
        let layout = switcher::layout(&sizes, (area.w, area.h));
        let thumbs: Vec<Option<&Pixmap>> =
            s.list().iter().map(|id| self.view.thumbs.get(id)).collect();
        let icons: Vec<Option<&Pixmap>> = s
            .list()
            .iter()
            .map(|&id| {
                let app = self.desktop.window(id)?.app_id.as_ref()?;
                self.view.icons.get(app)?.as_ref()
            })
            .collect();
        let accent = self.desktop.settings.border.active;
        let style = switcher::Style {
            highlight: if accent[3] > 0. {
                [
                    accent[0] / accent[3],
                    accent[1] / accent[3],
                    accent[2] / accent[3],
                    1.,
                ]
            } else {
                switcher::Style::default().highlight
            },
            ..switcher::Style::default()
        };
        Some(switcher::draw(&layout, s.index(), &thumbs, &icons, &style))
    }
}

/// Captures each window and sends its thumbnail, waking the poll loop for each.
fn thumbnails(
    jobs: Vec<(Id, String)>,
    generation: u64,
    tx: &Sender<(u64, Id, Pixmap)>,
    mut waker: UnixStream,
) {
    if jobs.is_empty() {
        return;
    }
    let mut cap = match Capturer::connect() {
        Ok(c) => c,
        Err(e) => {
            warn!("thumbnails: {e:#}");
            return;
        }
    };
    for (id, ident) in jobs {
        match cap.capture(Source::Toplevel(&ident), false, THUMB_TIMEOUT) {
            Ok(img) => {
                let p = Pixmap::from_rgba(&img).scaled_to_fit(THUMB);
                if tx.send((generation, id, p)).is_err() {
                    return;
                }
                let _ = waker.write(&[1]);
            }
            Err(e) => debug!("thumbnail of {id}: {e:#}"),
        }
    }
}

/// The scene a render sequence's frames show: shown windows top first, at their drawn size.
fn scene(
    desktop: &Desktop,
    frames: &[Frame],
    identifiers: &HashMap<Id, String>,
    pointer: Option<(i32, i32)>,
) -> Scene {
    let windows = frames
        .iter()
        .rev()
        .filter(|f| f.fullscreen || f.clip.is_some())
        .filter_map(|f| {
            let w = desktop.window(f.id)?;
            let rect = if f.fullscreen {
                desktop.output(w.output?)?.area
            } else {
                let (sw, sh) = w.size?;
                Rect::new(
                    f.pos.0,
                    f.pos.1,
                    (f64::from(sw) * f.scale).round() as i32,
                    (f64::from(sh) * f.scale).round() as i32,
                )
            };
            Some(SceneWindow {
                id: f.id,
                identifier: identifiers.get(&f.id).cloned(),
                rect,
                title: w.title.clone(),
                app_id: w.app_id.clone(),
            })
        })
        .collect();
    Scene {
        outputs: desktop
            .outputs()
            .iter()
            .map(|o| SceneOutput {
                name: o.name.clone(),
                area: o.area,
            })
            .collect(),
        windows,
        focus: desktop.focus(),
        active_output: desktop
            .active_output()
            .and_then(|o| desktop.output(o))
            .map(|o| o.name.clone()),
        pointer,
        locked: false,
        pointer_asked: 0,
        pointer_seq: 0,
        pick_asked: 0,
        picked: None,
    }
}

impl Dispatch<RiverXkbBindingV1, Key> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        key: &Key,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let river_xkb_binding_v1::Event::Pressed = event {
            state.view.keys.push(*key);
        }
    }
}

delegate_noop!(State: ignore wl_region::WlRegion);
impl State {
    /// The viewporter, for a surface shown at another size than its buffer's.
    pub(crate) fn viewporter(&self) -> Option<WpViewporter> {
        self.view.viewporter.clone()
    }
}

delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpSinglePixelBufferManagerV1);
