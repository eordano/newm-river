//! The Wayland side: river's window-management protocol and its companions, kept thin. Events
//! go into the [`Desktop`] model; in a manage sequence the model's decisions become window
//! management requests (sizes, focus, fullscreen, bindings), in a render sequence rendering
//! requests (positions, clip boxes, borders, glow, stacking).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use newm_canvas::Id;
use niri_config::live::LiveOptions;
use niri_config::{Action, Config, Trigger};
use niri_ipc::{Event, Reply, Request, Response};
use tracing::{debug, info, warn};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

use crate::glow;
use crate::input::InputState;
use crate::ipc::{Incoming, Server};
use crate::model::{ActivateSource, Desktop, Effect, Frame, OutputId, Rect};
use crate::placeholder::RestoreWindow;
use crate::pointer_wl::{self, PointerIo};
use crate::protocols::layer_shell::river_layer_shell_output_v1::{self, RiverLayerShellOutputV1};
use crate::protocols::layer_shell::river_layer_shell_seat_v1::{self, RiverLayerShellSeatV1};
use crate::protocols::layer_shell::river_layer_shell_v1::RiverLayerShellV1;
use crate::protocols::newm_hints::newm_river_hints_v1::{self, NewmRiverHintsV1};
use crate::protocols::newm_render::newm_river_render_v1::NewmRiverRenderV1;
use crate::protocols::newm_render::newm_river_window_render_v1::NewmRiverWindowRenderV1;
use crate::protocols::newm_shortcuts::newm_river_shortcuts_v1::NewmRiverShortcutsV1;
use crate::protocols::window_management::river_decoration_v1::RiverDecorationV1;
use crate::protocols::window_management::river_node_v1::RiverNodeV1;
use crate::protocols::window_management::river_output_v1::{self, RiverOutputV1};
use crate::protocols::window_management::river_seat_v1::{self, RiverSeatV1};
use crate::protocols::window_management::river_window_manager_v1::{self, RiverWindowManagerV1};
use crate::protocols::window_management::river_window_v1::{self, RiverWindowV1};
use crate::protocols::xkb_bindings::river_xkb_binding_v1::{self, RiverXkbBindingV1};
use crate::protocols::xkb_bindings::river_xkb_bindings_v1::RiverXkbBindingsV1;
use crate::render::{self, RenderExt};
use crate::{config, model, outputs, screencast, screenshot, session, shell};

mod kde;
mod synth;
mod view;

/// The glow's decoration surface and the buffer it shows.
struct Glow {
    surface: wl_surface::WlSurface,
    decoration: RiverDecorationV1,
    buffer: Option<wl_buffer::WlBuffer>,
    /// The window size the buffer is for; None: cleared.
    key: Option<(i32, i32)>,
}

struct WindowObj {
    proxy: RiverWindowV1,
    node: RiverNodeV1,
    shown: bool,
    configured: bool,
    border: Option<(i32, [f32; 4])>,
    glow: Option<Glow>,
    /// Where and at what scale it was last drawn: where its close animation starts.
    last: Option<((i32, i32), f64)>,
}

/// A closed window's last frame (render v2 snapshot) shrinking to half its size about its
/// middle and fading out, as niri's `window-close` draws it.
struct Closing {
    snap: crate::render::Snapshot,
    start: Instant,
    pos: (i32, i32),
    size: (f64, f64),
}

struct OutputObj {
    proxy: RiverOutputV1,
    id: OutputId,
    wl: Option<wl_output::WlOutput>,
    layer: Option<RiverLayerShellOutputV1>,
}

struct SeatObj {
    proxy: RiverSeatV1,
    bindings: Vec<RiverXkbBindingV1>,
    enabled: bool,
    cursor_set: bool,
    layer: Option<RiverLayerShellSeatV1>,
}

/// Whether a layer surface (bar launcher, fuzzel) holds the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerFocus {
    None,
    Exclusive,
    NonExclusive,
}

/// What subscribers last heard, to send only changes.
#[derive(Default)]
struct Snapshot {
    windows: HashMap<u64, String>,
    focus: Option<u64>,
    workspaces: Vec<niri_ipc::Workspace>,
    overview: bool,
}

/// How long a closed window's snapshot outlives its animation, at most.
const FRAME_SLACK: std::time::Duration = std::time::Duration::from_millis(100);

pub struct State {
    pub(crate) wm: RiverWindowManagerV1,
    pub(crate) desktop: Desktop,
    pub(crate) config: Config,
    /// The live settings' overrides on top of the config file, set over IPC.
    live: LiveOptions,
    /// The displays the live settings were last sent with (`outputs::displays`).
    displays: Vec<niri_config::live::Display>,
    pub(crate) input: InputState,
    globals: GlobalList,
    windows: HashMap<ObjectId, Id>,
    objs: HashMap<Id, WindowObj>,
    outputs: HashMap<ObjectId, OutputObj>,
    wl_outputs: HashMap<ObjectId, OutputId>,
    seats: HashMap<ObjectId, SeatObj>,
    next_window: Id,
    next_output: OutputId,
    pub(crate) pending: Vec<(Action, Option<Sender<Reply>>)>,
    /// The `pick-window` request waiting for the pointer's pick (a newer pick cancels it).
    pick_reply: Option<Sender<Reply>>,
    binding_actions: HashMap<ObjectId, Action>,
    layer_focus: LayerFocus,
    /// river sent focus_non_exclusive in this manage sequence: a focus request now would keep
    /// the layer surface from getting the keyboard (river-layer-shell-v1), so none is sent.
    layer_focus_fresh: bool,
    layer_shell: Option<RiverLayerShellV1>,
    default_output: Option<OutputId>,
    pub(crate) xkb_bindings: Option<RiverXkbBindingsV1>,
    /// newm-river fork: marks bindings an app inhibiting shortcuts cannot hold back.
    shortcuts: Option<NewmRiverShortcutsV1>,
    /// System bell rings, modal dialogs and activation requests (`newm_river_hints_v1`); absent
    /// on an upstream river.
    _hints: Option<NewmRiverHintsV1>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    env: Vec<(String, Option<String>)>,
    subscribers: Vec<SyncSender<Event>>,
    snapshot: Snapshot,
    render_ext: RenderExt,
    pub(crate) pointer: PointerIo,
    placeholders: shell::Placeholders,
    resize_box: shell::ResizeBox,
    minimap: shell::Minimap,
    /// Saves and restores the session; None unless river runs on a TTY.
    session: Option<session::Saver>,
    /// Started after a crash: the next manage sequence puts the saved spots back for the windows
    /// river hands over (`supervise.rs`, [`session::Saver::adopt`]).
    adopting: bool,
    view: view::ViewState,
    /// Applies the config's `output` blocks over wlr-output-management.
    pub(crate) output_config: outputs::OutputConfig,
    pub(crate) running: bool,
    /// river reported the session locked (ext-session-lock): ScreenShot2 refuses captures.
    pub(crate) locked: bool,
    /// Watches the config file for a hot reload.
    watcher: crate::reload::Watcher,
    /// Closed windows' close animations.
    closing: Vec<Closing>,
    /// The user quit: river ends the session and closes the connection.
    quitting: bool,
    /// Spectacle's screencasts (zkde_screencast relayed by river).
    pub(crate) casts: screencast::Casts,
    /// The KDE services' window-manager side: KDE apps' keys, D-Bus inhibitions, KRunner.
    kde: kde::KdeWm,
    /// newm's synth background, one background-layer surface per output while on.
    synth: synth::Synth,
}

/// Starts a command with newm-river's environment and reaps it when it exits.
pub fn spawn(cmd: &[String], env: &[(String, Option<String>)]) {
    spawn_in(cmd, None, env);
}

/// [`spawn`] in the directory `cwd`.
fn spawn_in(cmd: &[String], cwd: Option<&Path>, env: &[(String, Option<String>)]) {
    let Some((program, args)) = cmd.split_first() else {
        return;
    };
    let mut c = Command::new(program);
    c.args(args).stdin(Stdio::null());
    if let Some(cwd) = cwd {
        c.current_dir(cwd);
    }
    for (k, v) in env {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    match c.spawn() {
        Ok(mut child) => {
            let _ = thread::Builder::new()
                .name("reap".into())
                .spawn(move || child.wait());
        }
        Err(e) => warn!("spawning {cmd:?}: {e}"),
    }
}

fn spawn_sh(cmd: &str, env: &[(String, Option<String>)]) {
    spawn(&["sh".into(), "-c".into(), cmd.into()], env);
}

/// An anonymous file holding `bytes`, for a keymap or a shm pool.
pub fn memfd(name: &std::ffi::CStr, bytes: &[u8]) -> Result<File> {
    // SAFETY: memfd_create takes a NUL-terminated name and returns a new fd or -1.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create: {}", std::io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor nothing else owns.
    let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.write_all(bytes)?;
    Ok(file)
}

/// A premultiplied colour channel as river's 32-bit fraction.
fn channel(v: f32) -> u32 {
    (f64::from(v.clamp(0., 1.)) * f64::from(u32::MAX)).round() as u32
}

impl State {
    /// Records river's session_locked / session_unlocked, and puts it in the scene ScreenShot2
    /// reads at once rather than at the next render sequence.
    fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        self.casts.set_locked(locked);
        if let Ok(mut scene) = self.view.scene().lock() {
            scene.locked = locked;
        }
    }

    fn obj_id(&self, proxy: &RiverWindowV1) -> Option<Id> {
        self.windows.get(&proxy.id()).copied()
    }

    // ---- manage --------------------------------------------------------------------------

    fn manage(&mut self, qh: &QueueHandle<Self>) {
        let now = Instant::now();
        self.view.pointer_reported();
        take_focus_back(self);
        if self.adopting
            && let Some(s) = &mut self.session
            && (s.adopt_ready(&self.desktop) || !self.desktop.holding_new_windows(now))
        {
            self.adopting = false;
            s.adopt(now, &mut self.desktop, &self.config);
            self.desktop.release_new_windows();
        }
        self.desktop.manage(now);
        pointer_wl::manage(self, qh);
        for (action, reply) in std::mem::take(&mut self.pending) {
            debug!("action {action:?}");
            for effect in self.desktop.act(action, now) {
                self.effect(effect);
            }
            if let Some(reply) = reply {
                let _ = reply.send(Ok(Response::Handled));
            }
        }
        self.view_manage(qh);
        self.configure_new_windows();
        for (id, size) in self.desktop.proposals() {
            if let Some(obj) = self.objs.get(&id) {
                obj.proxy.propose_dimensions(size.0, size.1);
                self.desktop.set_asked(id, size);
            }
        }
        // An on-demand layer surface holds the keyboard only until the window manager moves the
        // focus (a binding, IPC, a click): any focus change takes it back.
        if self.desktop.focus_dirty
            && self.layer_focus != LayerFocus::Exclusive
            && !self.layer_focus_fresh
        {
            self.desktop.focus_dirty = false;
            self.layer_focus = LayerFocus::None;
            let focus = self.desktop.focus().and_then(|f| self.objs.get(&f));
            for seat in self.seats.values() {
                match focus {
                    Some(obj) => seat.proxy.focus_window(&obj.proxy),
                    None => seat.proxy.clear_focus(),
                }
            }
        }
        self.layer_focus_fresh = false;
        self.manage_seats(qh);
        // Layer surfaces that name no output (fuzzel, mako) open on the active output.
        let active = self.desktop.active_output();
        if self.default_output != active
            && let Some(o) = self.outputs.values().find(|o| Some(o.id) == active)
            && let Some(layer) = &o.layer
        {
            layer.set_default();
            self.default_output = Some(o.id);
        }
        self.wm.manage_finish();
    }

    /// Tells a newly placed window how it is decorated, tiled and bounded.
    fn configure_new_windows(&mut self) {
        let prefer_ssd = self.config.prefer_no_csd;
        for (id, obj) in &mut self.objs {
            let Some(w) = self.desktop.window(*id) else {
                continue;
            };
            if obj.configured || w.place.is_none() {
                continue;
            }
            obj.configured = true;
            if prefer_ssd {
                obj.proxy.use_ssd();
            } else {
                obj.proxy.use_csd();
            }
            let edges = if w.is_tiled() {
                river_window_v1::Edges::all()
            } else {
                river_window_v1::Edges::empty()
            };
            obj.proxy.set_tiled(edges);
            if obj.proxy.version() >= 4
                && let Some((bw, bh)) = self.desktop.bounds(*id)
            {
                obj.proxy.set_dimension_bounds(bw, bh);
            }
        }
    }

    fn manage_seats(&mut self, qh: &QueueHandle<Self>) {
        let theme = self.config.cursor.xcursor_theme.clone();
        let size = u32::from(self.config.cursor.xcursor_size);
        let unbound = self.seats.values().any(|s| s.bindings.is_empty());
        if unbound {
            self.bind_keys(qh);
        }
        for seat in self.seats.values_mut() {
            if !seat.enabled {
                seat.enabled = true;
                for b in &seat.bindings {
                    b.enable();
                }
            }
            if !seat.cursor_set && !theme.is_empty() && seat.proxy.version() >= 2 {
                seat.cursor_set = true;
                seat.proxy.set_xcursor_theme(theme.clone(), size);
            }
        }
        self.kde_manage(qh);
    }

    /// Creates the config's key bindings on every seat that has none yet.
    fn bind_keys(&mut self, qh: &QueueHandle<Self>) {
        let Some(xkb) = &self.xkb_bindings else {
            return;
        };
        let mut skipped = Vec::new();
        for seat in self.seats.values_mut() {
            if !seat.bindings.is_empty() {
                continue;
            }
            for bind in &self.config.binds.0 {
                let Some(action) = bind.press_action() else {
                    continue;
                };
                let Trigger::Keysym(sym) = bind.key.trigger else {
                    if crate::pointer::is_axis_trigger(bind.key.trigger) {
                        continue;
                    }
                    skipped.push(format!("{:?}", bind.key.trigger));
                    continue;
                };
                let mods = river_seat_v1::Modifiers::from_bits_truncate(config::river_modifiers(
                    bind.key.modifiers,
                ));
                let b = xkb.get_xkb_binding(&seat.proxy, sym.raw(), mods, qh, ());
                if !bind.allow_inhibiting
                    && let Some(shortcuts) = &self.shortcuts
                {
                    shortcuts.set_inhibitable(&b, 0);
                }
                self.binding_actions.insert(b.id(), action.clone());
                seat.bindings.push(b);
            }
            seat.enabled = false;
        }
        if !skipped.is_empty() {
            info!("bindings on {skipped:?}: not bound (only keys and scrolling are)");
        }
    }

    fn effect(&mut self, effect: Effect) {
        match effect {
            Effect::Spawn(cmd) => spawn(&cmd, &self.env),
            Effect::SpawnSh(cmd) => spawn_sh(&cmd, &self.env),
            Effect::SpawnIn { command, cwd, env } => {
                let mut all = self.env.clone();
                all.extend(env.into_iter().map(|(k, v)| (k, Some(v))));
                spawn_in(&command, cwd.as_deref(), &all);
            }
            Effect::Close(id) => {
                if let Some(obj) = self.objs.get(&id) {
                    obj.proxy.close();
                }
            }
            Effect::Quit => {
                info!("quit");
                self.quitting = true;
                if let Some(s) = &mut self.session {
                    s.save_now(&self.desktop, &self.config);
                }
                if self.wm.version() >= 4 {
                    self.wm.exit_session();
                } else {
                    self.running = false;
                }
            }
            Effect::Fullscreen(id, on) => {
                let Some(obj) = self.objs.get(&id) else {
                    return;
                };
                if on {
                    let out = self.desktop.window(id).and_then(|w| w.output);
                    if let Some(o) = self.outputs.values().find(|o| Some(o.id) == out) {
                        obj.proxy.fullscreen(&o.proxy);
                        obj.proxy.inform_fullscreen();
                    }
                } else {
                    obj.proxy.exit_fullscreen();
                    obj.proxy.inform_not_fullscreen();
                }
            }
            Effect::ConfirmQuit => self.confirm_quit(),
            Effect::ToggleShortcutsInhibit => {
                if let Some(shortcuts) = &self.shortcuts {
                    for seat in self.seats.values() {
                        shortcuts.toggle_inhibit(&seat.proxy);
                    }
                }
            }
            Effect::ToggleSynthBackground => self.synth_toggle(),
            Effect::SetSynthBackground(mode) => self.synth_set_mode(mode),
            Effect::Unsupported(what) => info!("not supported yet: {what}"),
        }
    }

    /// The config file changed: what parses is applied live (binds, canvas config, rules,
    /// input, the layout's look, animations); what does not shows the error bar and keeps the
    /// config running.
    fn reload_config(&mut self, qh: &QueueHandle<Self>, now: Instant) {
        let path = self.watcher.path().to_path_buf();
        match config::load(&path) {
            Ok(c) => {
                info!("config reloaded from {}", path.display());
                config::log_unsupported(&c);
                // Live overrides stay on top of every config file load.
                let c = self.live.loaded(c);
                self.apply_config(c, qh, now);
                self.send_options_changed();
            }
            Err(e) => {
                warn!("config {}: {e:?}", path.display());
                self.show_config_error(now);
            }
        }
        self.wm.manage_dirty();
    }

    fn apply_config(&mut self, config: Config, qh: &QueueHandle<Self>, now: Instant) {
        let State {
            seats,
            binding_actions,
            objs,
            ..
        } = self;
        let cursor_changed = config.cursor != self.config.cursor;
        for seat in seats.values_mut() {
            // The cursor theme goes to river once per seat: a new size or theme sends it again.
            seat.cursor_set &= !cursor_changed;
            for b in seat.bindings.drain(..) {
                binding_actions.remove(&b.id());
                b.destroy();
            }
            seat.enabled = false;
        }
        for obj in objs.values_mut() {
            obj.border = None;
            if let Some(g) = &mut obj.glow {
                g.key = None;
            }
        }
        self.desktop
            .reconfigure(config::settings(&config), config.window_rules.clone(), now);
        self.desktop.set_view_settings(view::view_settings(&config));
        self.input.reconfigure(&config, qh);
        self.pointer.reconfigure(&config);
        self.output_config.reconfigure(qh, &config);
        let synth_mode_changed = config.synth_background.mode != self.config.synth_background.mode;
        let synth = config.synth_background.clone();
        self.config = config;
        self.synth_config(&synth, synth_mode_changed);
        self.kde_reserve();
    }

    // ---- render --------------------------------------------------------------------------

    /// Window `id` closed: its last frame stays drawn for the close animation (render v2 only).
    fn start_close(&mut self, id: Id, now: Instant, qh: &QueueHandle<Self>) {
        let d = self.desktop.settings_view().close;
        let (Some(obj), Some(size)) = (
            self.objs.get(&id),
            self.desktop.window(id).and_then(|w| w.size),
        ) else {
            return;
        };
        let Some((pos, scale)) = obj.last.filter(|_| obj.shown && !d.is_zero()) else {
            return;
        };
        if let Some(snap) = self.render_ext.snapshot(id, d + FRAME_SLACK, qh) {
            let size = (f64::from(size.0) * scale, f64::from(size.1) * scale);
            self.closing.push(Closing {
                snap,
                start: now,
                pos,
                size,
            });
        }
    }

    /// Moves every close animation on to `now`; a finished one drops its snapshot.
    fn render_closing(&mut self, now: Instant) {
        let d = self.desktop.settings_view().close;
        self.closing.retain(|c| {
            let t =
                now.saturating_duration_since(c.start).as_secs_f64() / d.as_secs_f64().max(1e-9);
            if t >= 1. || c.snap.expired() {
                return false;
            }
            let u = 1. - t;
            let p = 1. - u * u;
            let k = 1. - p / 2.;
            let shift = |v: f64| (v * (1. - k) / 2.).round() as i32;
            c.snap
                .set_position(c.pos.0 + shift(c.size.0), c.pos.1 + shift(c.size.1));
            c.snap.set_scale(k);
            c.snap.set_opacity(1. - p);
            true
        });
    }

    /// Whether a close animation runs: more render sequences are due.
    pub(crate) fn closing(&self) -> bool {
        !self.closing.is_empty()
    }

    fn render(&mut self, qh: &QueueHandle<Self>) {
        let frames = self.desktop.render(Instant::now());
        self.render_closing(Instant::now());
        for f in &frames {
            self.render_window(f, qh);
        }
        self.placeholders.render(
            &self.desktop.placeholder_frames(),
            shell::color(&self.config),
            &self.wm,
            (self.compositor.as_ref(), self.shm.as_ref()),
            qh,
        );
        self.resize_box.render(
            self.desktop.resize_box_frame(),
            &self.wm,
            (self.compositor.as_ref(), self.shm.as_ref()),
            qh,
        );
        let minimap = self.desktop.minimap_frame().map(|f| {
            // Painted in the pixels of the output it is on.
            let heads = self.output_config.heads();
            let scale = heads.iter().find(|h| h.name == f.output).map(|h| h.scale);
            (f, scale.unwrap_or(1.))
        });
        self.minimap.render(
            minimap,
            &self.wm,
            (self.compositor.as_ref(), self.shm.as_ref()),
            self.viewporter().as_ref(),
            qh,
        );
        self.view_render(&frames, qh);
        self.wm.render_finish();
    }

    fn render_window(&mut self, f: &Frame, qh: &QueueHandle<Self>) {
        let size = self.desktop.window(f.id).and_then(|w| w.size);
        let glow_style = self.desktop.settings.glow;
        let radius = self.desktop.settings.corner_radius;
        let (compositor, shm) = (self.compositor.clone(), self.shm.clone());
        let Some(obj) = self.objs.get_mut(&f.id) else {
            return;
        };
        let visible = f.fullscreen || f.clip.is_some();
        if visible != obj.shown {
            debug!(
                "window {} {}: at {:?}, clip {:?}, scale {:.3}, opacity {:.2}",
                f.id,
                if visible { "shown" } else { "hidden" },
                f.pos,
                f.clip,
                f.scale,
                f.opacity
            );
            obj.shown = visible;
            if visible {
                obj.proxy.show();
            } else {
                obj.proxy.hide();
            }
        }
        if !f.fullscreen {
            obj.node.set_position(f.pos.0, f.pos.1);
            if let Some(c) = f.clip
                && obj.proxy.version() >= 2
            {
                obj.proxy.set_clip_box(c.x, c.y, c.w, c.h);
            }
        }
        if f.floating || f.fullscreen {
            obj.node.place_top();
        }
        if obj.border != Some(f.border) {
            obj.border = Some(f.border);
            let (width, c) = f.border;
            if width > 0 && c[3] > 0. {
                obj.proxy.set_borders(
                    river_window_v1::Edges::all(),
                    width,
                    channel(c[0]),
                    channel(c[1]),
                    channel(c[2]),
                    channel(c[3]),
                );
            } else {
                obj.proxy
                    .set_borders(river_window_v1::Edges::empty(), 0, 0, 0, 0, 0);
            }
        }
        let behind = self.desktop.window(f.id).is_none_or(|w| w.rules.behind);
        let glowing = f.focused && behind && visible && !f.fullscreen;
        // River draws the shadow itself since render v2 (scenefx): no software glow then.
        let effects = self.render_ext.supports_effects();
        let software_glow = glow_style.filter(|_| !effects);
        if let (Some(style), Some(compositor), Some(shm)) = (software_glow, compositor, shm) {
            let key = size.filter(|_| glowing);
            let glow = obj.glow.get_or_insert_with(|| {
                let surface = compositor.create_surface(qh, ());
                let decoration = obj.proxy.get_decoration_below(&surface, qh, ());
                Glow {
                    surface,
                    decoration,
                    buffer: None,
                    key: None,
                }
            });
            if glow.key != key {
                glow.key = key;
                let old = glow.buffer.take();
                match key {
                    Some(size) => {
                        let b = glow::render(&style, size, f.border.0, radius);
                        match shm_buffer(&shm, &b, qh) {
                            Ok(buffer) => {
                                glow.decoration.set_offset(b.offset.0, b.offset.1);
                                glow.decoration.sync_next_commit();
                                glow.surface.attach(Some(&buffer), 0, 0);
                                glow.surface.damage(0, 0, b.width, b.height);
                                glow.surface.commit();
                                glow.buffer = Some(buffer);
                            }
                            Err(e) => warn!("glow buffer: {e}"),
                        }
                    }
                    None => {
                        glow.decoration.sync_next_commit();
                        glow.surface.attach(None, 0, 0);
                        glow.surface.commit();
                    }
                }
                if let Some(old) = old {
                    old.destroy();
                }
            }
        }
        obj.last = Some((f.pos, f.scale));
        let proxy = obj.proxy.clone();
        self.render_ext.window(f.id, &proxy, f.scale, f.opacity, qh);
        if effects {
            let shadow = glow_style
                .filter(|_| glowing)
                .map(|g| render::Shadow::from_glow(&g));
            let corner_radius = radius.max(0.).round() as u32;
            self.render_ext.effects(
                f.id,
                render::Effects {
                    corner_radius,
                    shadow,
                },
            );
        }
    }

    // ---- IPC -----------------------------------------------------------------------------

    /// Answers a live settings request: lists, sets, resets or exports the live options. A set
    /// or reset applies the config file with the overrides through [`State::apply_config`], as
    /// a config file reload does.
    fn newm_options_request(&mut self, request: Request, qh: &QueueHandle<Self>) -> Reply {
        self.report_displays();
        match request {
            Request::NewmOptions => return Ok(Response::NewmOptions(self.live.options())),
            Request::NewmExport => {
                let kdl = self
                    .live
                    .export(concat!("newm-river ", env!("CARGO_PKG_VERSION")));
                return Ok(Response::NewmExport { kdl });
            }
            Request::NewmSetOption { key, value } => self.live.set(&key, value)?,
            Request::NewmResetOption { key } => self.live.reset(&key)?,
            Request::NewmResetAll => self.live.reset_all(),
            _ => return Err("not a live settings request".into()),
        }
        // A display whose scale nothing sets is asked for its own: river keeps the last one.
        let config = crate::outputs::with_own_scales(self.live.effective(), &self.live.options());
        self.apply_config(config, qh, Instant::now());
        self.wm.manage_dirty();
        self.send_options_changed();
        Ok(Response::Handled)
    }

    /// Tells the live options which displays are connected and the scale each is drawn at, so
    /// each has its scale setting.
    fn report_displays(&mut self) {
        let heads = self.output_config.heads();
        self.displays = crate::outputs::displays(&heads, &self.config);
        self.live.set_displays(self.displays.clone());
    }

    /// river described its outputs again: when a display came, went or changed its scale, every
    /// subscriber gets the live settings with each display's scale.
    pub(crate) fn displays_changed(&mut self) {
        let heads = self.output_config.heads();
        if crate::outputs::displays(&heads, &self.config) != self.displays {
            self.send_options_changed();
        }
    }

    /// Answers a block request: places a block on the active canvas and launches its windows,
    /// reports which windows took a block's slots, or parks the active canvas's windows.
    fn newm_block_request(&mut self, request: Request) -> Reply {
        let now = Instant::now();
        let response = match request {
            Request::NewmPlaceBlock(block) => {
                if block.windows.iter().any(|w| w.command.is_empty()) {
                    return Err("bad block".into());
                }
                let slots = block
                    .windows
                    .iter()
                    .map(|w| RestoreWindow {
                        app_id: w.app_id.clone(),
                        title: None,
                        rect: newm_canvas::Rect::new(w.rect.0, w.rect.1, w.rect.2, w.rect.3),
                    })
                    .collect();
                let (id, origin) = self.desktop.place_block(
                    slots,
                    block.anchor,
                    block.focus,
                    block.fly,
                    session::RESTORE_LIFETIME,
                    now,
                )?;
                for w in block.windows {
                    let l = session::Launch {
                        command: w.command,
                        cwd: w.cwd.map(Into::into),
                    };
                    self.effect(session::launch(&l, w.env.into_iter().collect()));
                }
                Response::NewmBlock { id, origin }
            }
            Request::NewmBlockStatus { id } => {
                let (windows, done) = self
                    .desktop
                    .block_status(id)
                    .ok_or_else(|| String::from("no such block"))?;
                return Ok(Response::NewmBlockStatus { id, windows, done });
            }
            Request::NewmParkWindows => {
                self.desktop.park_windows(now);
                Response::Handled
            }
            _ => return Err("not a block request".into()),
        };
        self.wm.manage_dirty();
        Ok(response)
    }

    /// Sends every subscriber the live settings as they are now.
    fn send_options_changed(&mut self) {
        self.report_displays();
        let event = Event::NewmOptionsChanged(self.live.options());
        self.subscribers
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }

    fn ipc(&mut self, incoming: Incoming, qh: &QueueHandle<Self>) {
        match incoming {
            Incoming::Subscribe(tx) => {
                if self.subscribers.is_empty() {
                    self.snapshot = Snapshot::default();
                    self.emit();
                }
                self.report_displays();
                let events = [
                    Event::WorkspacesChanged {
                        workspaces: self.desktop.ipc_workspaces(),
                    },
                    Event::WindowsChanged {
                        windows: self.desktop.ipc_windows(),
                    },
                    Event::WindowFocusChanged {
                        id: self.desktop.focus(),
                    },
                    Event::OverviewOpenedOrClosed {
                        is_open: self.desktop.overview_open(),
                    },
                    Event::NewmOptionsChanged(self.live.options()),
                ];
                if events.into_iter().all(|e| tx.try_send(e).is_ok()) {
                    self.subscribers.push(tx);
                }
            }
            Incoming::Request(request, reply) => {
                if matches!(
                    request,
                    Request::NewmOptions
                        | Request::NewmSetOption { .. }
                        | Request::NewmResetOption { .. }
                        | Request::NewmResetAll
                        | Request::NewmExport
                ) {
                    let _ = reply.send(self.newm_options_request(request, qh));
                    return;
                }
                if matches!(
                    request,
                    Request::NewmPlaceBlock(_)
                        | Request::NewmBlockStatus { .. }
                        | Request::NewmParkWindows
                ) {
                    let _ = reply.send(self.newm_block_request(request));
                    return;
                }
                let d = &self.desktop;
                let ipc_output = |o: &model::Output| {
                    let mut out = Desktop::ipc_output(o);
                    self.output_config.fill_ipc(&mut out);
                    out
                };
                let r: Reply = match request {
                    // The version with the compositor's name, so a client can tell which
                    // niri-compatible desktop it talks to.
                    Request::Version => Ok(Response::Version(
                        concat!(env!("CARGO_PKG_VERSION"), " (newm-river)").into(),
                    )),
                    Request::Outputs => Ok(Response::Outputs(
                        d.outputs()
                            .iter()
                            .map(|o| (o.name.clone(), ipc_output(o)))
                            .collect(),
                    )),
                    Request::Workspaces => Ok(Response::Workspaces(d.ipc_workspaces())),
                    Request::Windows => Ok(Response::Windows(d.ipc_windows())),
                    Request::FocusedOutput => Ok(Response::FocusedOutput(
                        d.active_output()
                            .and_then(|id| d.output(id))
                            .map(ipc_output),
                    )),
                    Request::FocusedWindow => Ok(Response::FocusedWindow(
                        d.focus().and_then(|f| d.window(f)).map(|w| d.ipc_window(w)),
                    )),
                    Request::OverviewState => Ok(Response::OverviewState(niri_ipc::Overview {
                        is_open: d.overview_open(),
                    })),
                    Request::Action(action) => {
                        self.pending.push((action.into(), Some(reply)));
                        self.wm.manage_dirty();
                        return;
                    }
                    Request::PickWindow if self.locked => Ok(Response::PickedWindow(None)),
                    Request::PickWindow => {
                        if let Some(old) = self.pick_reply.replace(reply) {
                            let _ = old.send(Ok(Response::PickedWindow(None)));
                        }
                        self.pointer.start_pick();
                        self.wm.manage_dirty();
                        return;
                    }
                    Request::ReturnError => Err("example compositor error".into()),
                    other => Err(format!("newm-river does not answer {other:?}")),
                };
                let _ = reply.send(r);
            }
        }
    }

    /// Sends subscribers what changed since they last heard.
    fn emit(&mut self) {
        if self.subscribers.is_empty() {
            return;
        }
        let mut events = Vec::new();
        let workspaces = self.desktop.ipc_workspaces();
        if workspaces != self.snapshot.workspaces {
            self.snapshot.workspaces = workspaces.clone();
            events.push(Event::WorkspacesChanged { workspaces });
        }
        let mut seen = HashMap::new();
        for w in self.desktop.ipc_windows() {
            let text = serde_json::to_string(&w).unwrap_or_default();
            if self.snapshot.windows.get(&w.id) != Some(&text) {
                seen.insert(w.id, text);
                events.push(Event::WindowOpenedOrChanged { window: w });
            } else {
                seen.insert(w.id, text);
            }
        }
        for id in self.snapshot.windows.keys() {
            if !seen.contains_key(id) {
                events.push(Event::WindowClosed { id: *id });
            }
        }
        self.snapshot.windows = seen;
        let focus = self.desktop.focus();
        if focus != self.snapshot.focus {
            self.snapshot.focus = focus;
            events.push(Event::WindowFocusChanged { id: focus });
        }
        let overview = self.desktop.overview_open();
        if overview != self.snapshot.overview {
            self.snapshot.overview = overview;
            events.push(Event::OverviewOpenedOrClosed { is_open: overview });
        }
        if events.is_empty() {
            return;
        }
        // A full queue drops the client (ipc::EVENT_QUEUE), a gone one too.
        self.subscribers
            .retain(|tx| events.iter().all(|e| tx.try_send(e.clone()).is_ok()));
    }

    fn add_output(&mut self, proxy: RiverOutputV1, qh: &QueueHandle<Self>) {
        let id = self.next_output;
        self.next_output += 1;
        self.desktop.add_output(id);
        let layer = self
            .layer_shell
            .as_ref()
            .map(|l| l.get_output(&proxy, qh, ()));
        self.outputs.insert(
            proxy.id(),
            OutputObj {
                proxy,
                id,
                wl: None,
                layer,
            },
        );
    }

    /// The outputs whose `wl_output` is bound, by id.
    fn wl_outputs_by_id(&self) -> Vec<(OutputId, wl_output::WlOutput)> {
        self.outputs
            .values()
            .filter_map(|o| Some((o.id, o.wl.clone()?)))
            .collect()
    }

    fn output_of(&self, key: &ObjectId) -> Option<OutputId> {
        self.outputs.get(key).map(|o| o.id)
    }
}

pub(crate) fn shm_buffer(
    shm: &wl_shm::WlShm,
    b: &glow::Buffer,
    qh: &QueueHandle<State>,
) -> Result<wl_buffer::WlBuffer> {
    let file = memfd(c"newm-river-glow", &b.pixels)?;
    let pool = shm.create_pool(file.as_fd(), b.pixels.len() as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        b.width,
        b.height,
        b.width * 4,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore RiverNodeV1);
delegate_noop!(State: ignore RiverDecorationV1);
delegate_noop!(State: ignore RiverXkbBindingsV1);
delegate_noop!(State: ignore RiverLayerShellV1);
delegate_noop!(State: ignore NewmRiverRenderV1);
delegate_noop!(State: ignore NewmRiverShortcutsV1);
delegate_noop!(State: ignore NewmRiverWindowRenderV1);

impl Dispatch<NewmRiverHintsV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &NewmRiverHintsV1,
        event: newm_river_hints_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            newm_river_hints_v1::Event::Bell { window } => {
                if let Some(id) = state.obj_id(&window)
                    && state.desktop.on_bell(id)
                {
                    debug!("window {id} rang the bell: urgent");
                    state.wm.manage_dirty();
                }
            }
            newm_river_hints_v1::Event::Activate { window, source } => {
                let source = match source {
                    WEnum::Value(newm_river_hints_v1::Source::Token) => ActivateSource::Token,
                    WEnum::Value(newm_river_hints_v1::Source::UrgentOnly) => {
                        ActivateSource::UrgentOnly
                    }
                    WEnum::Value(newm_river_hints_v1::Source::ForeignToplevel) => {
                        ActivateSource::ForeignToplevel
                    }
                    _ => return,
                };
                if let Some(id) = state.obj_id(&window) {
                    let focus = state.desktop.on_activate(id, source);
                    debug!("window {id} activation ({source:?}): focus {focus}");
                    if focus {
                        state.pending.push((Action::FocusWindow(id), None));
                    }
                    state.wm.manage_dirty();
                }
            }
            newm_river_hints_v1::Event::Modal { window, modal } => {
                if let Some(id) = state.obj_id(&window) {
                    debug!("window {id} modal: {modal}");
                    if state.desktop.set_modal(id, modal != 0, Instant::now()) {
                        state.wm.manage_dirty();
                    }
                }
            }
        }
    }
}

impl Dispatch<RiverWindowManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &RiverWindowManagerV1,
        event: river_window_manager_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_manager_v1::Event;
        match event {
            Event::Unavailable => {
                warn!("river refused window management: another window manager runs");
                state.running = false;
            }
            Event::Finished => state.running = false,
            Event::SessionLocked => state.set_locked(true),
            Event::SessionUnlocked => state.set_locked(false),
            Event::ManageStart => {
                state.manage(qh);
                state.emit();
            }
            Event::RenderStart => {
                state.render(qh);
                state.emit();
            }
            Event::Window { id } => {
                let node = id.get_node(qh, ());
                let wid = state.next_window;
                state.next_window += 1;
                state.desktop.add_window(wid);
                state.windows.insert(id.id(), wid);
                state.objs.insert(
                    wid,
                    WindowObj {
                        proxy: id,
                        node,
                        shown: true,
                        configured: false,
                        border: None,
                        glow: None,
                        last: None,
                    },
                );
            }
            Event::Output { id } => state.add_output(id, qh),
            Event::Seat { id } => {
                state.pointer.add_seat(&id);
                let layer = state.layer_shell.as_ref().map(|l| l.get_seat(&id, qh, ()));
                state.seats.insert(
                    id.id(),
                    SeatObj {
                        proxy: id,
                        bindings: Vec::new(),
                        enabled: false,
                        cursor_set: false,
                        layer,
                    },
                );
            }
        }
    }

    event_created_child!(State, RiverWindowManagerV1, [
        river_window_manager_v1::EVT_WINDOW_OPCODE => (RiverWindowV1, ()),
        river_window_manager_v1::EVT_OUTPUT_OPCODE => (RiverOutputV1, ()),
        river_window_manager_v1::EVT_SEAT_OPCODE => (RiverSeatV1, ()),
    ]);
}

impl Dispatch<RiverWindowV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverWindowV1,
        event: river_window_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_v1::Event;
        let Some(id) = state.obj_id(proxy) else {
            return;
        };
        let now = Instant::now();
        match event {
            Event::Closed => {
                state.start_close(id, now, qh);
                state.desktop.remove_window(id, now);
                state.windows.remove(&proxy.id());
                state.render_ext.remove(id);
                state.view.forget(id);
                if let Some(obj) = state.objs.remove(&id) {
                    if let Some(glow) = obj.glow {
                        glow.decoration.destroy();
                        glow.surface.destroy();
                        if let Some(b) = glow.buffer {
                            b.destroy();
                        }
                    }
                    obj.node.destroy();
                    obj.proxy.destroy();
                }
            }
            Event::DimensionsHint {
                min_width,
                min_height,
                max_width,
                max_height,
            } => state
                .desktop
                .set_hints(id, (min_width, min_height), (max_width, max_height)),
            Event::Dimensions { width, height } => {
                if state.desktop.on_dimensions(id, width, height) {
                    state.wm.manage_dirty();
                }
            }
            Event::AppId { app_id } => {
                if let Some(w) = state.desktop.window_mut(id) {
                    w.app_id = app_id;
                }
            }
            Event::Title { title } => {
                if let Some(w) = state.desktop.window_mut(id) {
                    w.title = title;
                }
                // During a restore, a window may trade spots once its title arrives.
                if state.desktop.retitled(id, now) {
                    state.wm.manage_dirty();
                }
            }
            Event::Parent { parent } => {
                let parent = parent.and_then(|p| state.obj_id(&p));
                if let Some(w) = state.desktop.window_mut(id) {
                    w.parent = parent;
                }
            }
            Event::Identifier { identifier } => {
                state.view.identifiers.insert(id, identifier);
            }
            Event::UnreliablePid { unreliable_pid } => {
                if let Some(w) = state.desktop.window_mut(id) {
                    w.pid = Some(unreliable_pid);
                }
            }
            Event::FullscreenRequested { .. } => {
                state.pending.push((Action::FullscreenWindowById(id), None));
            }
            Event::ExitFullscreenRequested
                if state.desktop.window(id).is_some_and(|w| w.fullscreen) =>
            {
                state.pending.push((Action::FullscreenWindowById(id), None));
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverOutputV1,
        event: river_output_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_output_v1::Event;
        let key = proxy.id();
        let Some(id) = state.output_of(&key) else {
            return;
        };
        match event {
            Event::Removed => {
                state.desktop.remove_output(id);
                if state.default_output == Some(id) {
                    state.default_output = None;
                }
                if let Some(o) = state.outputs.remove(&key) {
                    if let Some(l) = o.layer {
                        l.destroy();
                    }
                    if let Some(wl) = o.wl {
                        state.wl_outputs.remove(&wl.id());
                        if wl.version() >= 3 {
                            wl.release();
                        }
                    }
                    o.proxy.destroy();
                }
            }
            Event::WlOutput { name } => {
                let version = state
                    .globals
                    .contents()
                    .with_list(|l| l.iter().find(|g| g.name == name).map(|g| g.version));
                if let Some(v) = version.filter(|&v| v >= 4) {
                    let wl: wl_output::WlOutput =
                        state.globals.registry().bind(name, v.min(4), qh, ());
                    state.wl_outputs.insert(wl.id(), id);
                    if let Some(o) = state.outputs.get_mut(&key) {
                        o.wl = Some(wl);
                    }
                }
            }
            Event::Position { x, y } => {
                debug!("output {id} at {x},{y}");
                let area = state.desktop.output(id).map(|o| o.area).unwrap_or_default();
                state
                    .desktop
                    .set_output_area(id, Rect::new(x, y, area.w, area.h));
            }
            Event::Dimensions { width, height } => {
                debug!("output {id} is {width}x{height}");
                let area = state.desktop.output(id).map(|o| o.area).unwrap_or_default();
                state
                    .desktop
                    .set_output_area(id, Rect::new(area.x, area.y, width, height));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { flags, refresh, .. } = &event
            && flags
                .into_result()
                .is_ok_and(|f| f.contains(wl_output::Mode::Current))
        {
            state.synth.output_mode(proxy.id(), *refresh);
        }
        if let wl_output::Event::Name { name } = event
            && let Some(&id) = state.wl_outputs.get(&proxy.id())
        {
            let first = state
                .config
                .outputs
                .0
                .iter()
                .any(|o| o.focus_at_startup && o.name.eq_ignore_ascii_case(&name));
            state.desktop.set_output_name(id, name);
            if first && state.desktop.focus().is_none() {
                state.desktop.focus_output(id, Instant::now());
            }
        }
    }
}

impl Dispatch<RiverSeatV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverSeatV1,
        event: river_seat_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_seat_v1::Event;
        pointer_wl::seat_event(state, proxy, &event);
        match event {
            Event::Removed => {
                if let Some(seat) = state.seats.remove(&proxy.id()) {
                    for b in seat.bindings {
                        state.binding_actions.remove(&b.id());
                        b.destroy();
                    }
                    if let Some(l) = seat.layer {
                        l.destroy();
                    }
                }
                proxy.destroy();
            }
            Event::PointerPosition { x, y } => state.view.pointer = Some((x, y)),
            Event::WindowInteraction { window } => {
                if let Some(id) = state.obj_id(&window) {
                    state.pending.push((Action::FocusWindow(id), None));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverXkbBindingV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let river_xkb_binding_v1::Event::Pressed = event
            && let Some(action) = state.binding_actions.get(&proxy.id())
        {
            state.pending.push((action.clone(), None));
            state.pointer.key_pressed();
        }
    }
}

impl Dispatch<RiverLayerShellOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverLayerShellOutputV1,
        event: river_layer_shell_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let river_layer_shell_output_v1::Event::NonExclusiveArea {
            x,
            y,
            width,
            height,
        } = event;
        let id = state
            .outputs
            .values()
            .find(|o| o.layer.as_ref() == Some(proxy))
            .map(|o| o.id);
        if let Some(id) = id {
            state
                .desktop
                .set_output_usable(id, Rect::new(x, y, width, height));
        }
    }
}

impl Dispatch<RiverLayerShellSeatV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &RiverLayerShellSeatV1,
        event: river_layer_shell_seat_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_layer_shell_seat_v1::Event;
        match event {
            Event::FocusExclusive => {
                state.layer_focus = LayerFocus::Exclusive;
                state.desktop.focus_dirty = false;
            }
            Event::FocusNonExclusive => {
                state.layer_focus = LayerFocus::NonExclusive;
                state.layer_focus_fresh = true;
                state.desktop.focus_dirty = false;
            }
            Event::FocusNone => {
                state.layer_focus = LayerFocus::None;
                state.desktop.focus_dirty = true;
            }
        }
    }
}

/// Window interactions and bindings arrive while a layer surface holds the keyboard
/// non-exclusively: a click on a window takes the focus back.
fn take_focus_back(state: &mut State) {
    if state.layer_focus == LayerFocus::NonExclusive
        && !state.layer_focus_fresh
        && state
            .pending
            .iter()
            .any(|(a, _)| matches!(a, Action::FocusWindow(_)))
    {
        // The window manager's own focus may not change (the layer surface took the keyboard,
        // not its focus), so the focus goes to river again even for the window it already names.
        state.desktop.focus_dirty = true;
    }
}

/// Runs newm-river until river ends the session.
/// How long a window manager started after a crash holds the running session's windows back
/// while it learns the outputs the saved canvases name.
const ADOPT_WAIT: Duration = Duration::from_secs(3);

pub fn run(config_path: &Path) -> Result<()> {
    let config = config::load_or_default(config_path);
    config::log_unsupported(&config);
    let live = LiveOptions::new(config.clone(), config::LIVE_SKIP);
    let server = Server::start()?;
    info!("IPC socket {}", server.path.display());
    let env = config::environment(&config, &server.path);

    let conn = Connection::connect_to_env().context("no Wayland display: run inside river")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let wm: RiverWindowManagerV1 = globals
        .bind(&qh, 1..=6, ())
        .context("river_window_manager_v1 missing: is this river 0.4 or newer?")?;
    let xkb_bindings = globals.bind(&qh, 1..=3, ()).ok();
    if xkb_bindings.is_none() {
        warn!("river_xkb_bindings_v1 missing: no key bindings");
    }
    let layer_shell = globals.bind(&qh, 1..=1, ()).ok();
    let compositor = globals.bind(&qh, 1..=6, ()).ok();
    let shm = globals.bind(&qh, 1..=1, ()).ok();
    let input = InputState::bind(&globals, &qh, &config);
    let render: Option<NewmRiverRenderV1> = globals.bind(&qh, 1..=2, ()).ok();
    let shortcuts: Option<NewmRiverShortcutsV1> = globals.bind(&qh, 1..=1, ()).ok();
    let hints: Option<NewmRiverHintsV1> = globals.bind(&qh, 1..=2, ()).ok();
    let pointer = PointerIo::bind(&globals, &qh, &config);
    let view = view::ViewState::new(&config, &globals, &qh)?;
    let output_config = outputs::OutputConfig::bind(&globals, &qh);
    let casts = screencast::Casts::new(&globals, &qh, view.scene());
    let kde = kde::KdeWm::new(&globals, &qh);

    let mut state = State {
        wm,
        desktop: Desktop::new(
            config::settings(&config),
            config.window_rules.clone(),
            Instant::now(),
        ),
        input,
        globals,
        windows: HashMap::new(),
        objs: HashMap::new(),
        outputs: HashMap::new(),
        wl_outputs: HashMap::new(),
        seats: HashMap::new(),
        next_window: 1,
        next_output: 1,
        pending: Vec::new(),
        pick_reply: None,
        binding_actions: HashMap::new(),
        layer_focus: LayerFocus::None,
        layer_focus_fresh: false,
        layer_shell,
        default_output: None,
        xkb_bindings,
        shortcuts,
        _hints: hints,
        compositor,
        shm,
        env,
        subscribers: Vec::new(),
        snapshot: Snapshot::default(),
        render_ext: RenderExt::new(render),
        pointer,
        placeholders: shell::Placeholders::default(),
        resize_box: shell::ResizeBox::default(),
        minimap: shell::Minimap::default(),
        session: None,
        adopting: false,
        view,
        output_config,
        running: true,
        locked: false,
        watcher: crate::reload::Watcher::new(config_path, Instant::now()),
        closing: Vec::new(),
        quitting: false,
        casts,
        kde,
        synth: synth::Synth::new(&config.synth_background),
        config,
        live,
        displays: Vec::new(),
    };

    state
        .desktop
        .set_view_settings(view::view_settings(&state.config));
    let runner = state
        .kde
        .bridge
        .as_ref()
        .map(|b| crate::kde::WindowsRunner::new(b.wm.clone()));
    screenshot::serve(state.view.scene(), state.view.waker(), runner);
    if let Some(bridge) = &state.kde.bridge {
        crate::kde::serve(bridge, state.view.scene());
    }
    state.kde_reserve();

    // After a crash the startup programs still run, and the windows are the session's own.
    let restart = crate::supervise::restart_count();
    if restart == 0 {
        for s in &state.config.spawn_at_startup {
            spawn(&s.command, &state.env);
        }
        for s in &state.config.spawn_sh_at_startup {
            spawn_sh(&s.command, &state.env);
        }
    } else {
        warn!("taking over the running session after a crash (restart {restart})");
    }
    let now = Instant::now();
    state.session = session::saver(conn.backend().poll_fd(), &state.config, now);
    if restart > 0
        && let Some(s) = state.session.take()
    {
        state.session = Some(s.adopting());
        state.adopting = true;
        state.desktop.hold_new_windows(now + ADOPT_WAIT);
    }

    // After a quit river closes the connection: that error is the session ending, and the exit
    // stays clean so the supervisor (supervise.rs) does not take it for a crash.
    let res = (|| -> Result<()> {
        while state.running {
            conn.flush()?;
            queue.dispatch_pending(&mut state)?;
            if !state.running {
                break;
            }
            conn.flush()?;
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let deadline = [
                state.desktop.deadline(),
                state.session.as_ref().map(|s| s.deadline()),
                state.view_deadline(),
                state.pointer.deadline(),
                Some(state.watcher.deadline()),
                state.synth.deadline(),
            ];
            let timeout = deadline.into_iter().flatten().min().map_or(-1, |d| {
                d.saturating_duration_since(Instant::now()).as_millis() as i32 + 1
            });
            let mut fds = [
                libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: server.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: state.view.wake_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: state.casts.wake_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: state.kde.wake_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                // -1 while the synth background is off: poll skips it.
                libc::pollfd {
                    fd: state.synth.wake_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                // -1 unless the synth background listens to sound.
                libc::pollfd {
                    fd: state.synth.sound_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                // -1 unless the synth background watches the battery.
                libc::pollfd {
                    fd: state.synth.power_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: fds is a valid array of pollfd for its length.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                drop(guard);
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if fds[0].revents != 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            } else {
                drop(guard);
            }
            if fds[1].revents != 0 {
                server.drain_wake();
            }
            if fds[3].revents != 0 {
                state.casts.dispatch();
            }
            if fds[5].revents != 0 {
                state.synth_painted(&qh);
            }
            if fds[7].revents != 0 {
                state.synth.power_event();
            }
            while let Ok(incoming) = server.rx.try_recv() {
                state.ipc(incoming, &qh);
            }
            state.kde_messages();
            let now = Instant::now();
            if state.watcher.poll(now) {
                state.reload_config(&qh, now);
            }
            server.write_floating_follow(state.desktop.floating_follow());
            if state.desktop.tick(now) | state.pointer.tick(now) | state.view_tick(now) {
                state.wm.manage_dirty();
            }
            state.synth_tick(&qh, now);
            if let Some(s) = &mut state.session {
                let restoring = state.desktop.placeholders.is_restoring();
                let launches = s.poll(Instant::now(), &mut state.desktop, &state.config);
                if !launches.is_empty() || restoring != state.desktop.placeholders.is_restoring() {
                    state.wm.manage_dirty();
                }
                for l in launches {
                    state.effect(l);
                }
            }
        }
        Ok(())
    })();
    if state.quitting
        && let Err(e) = &res
    {
        info!("river ended the session after the quit ({e})");
        return Ok(());
    }
    res?;
    info!("river ended the session");
    Ok(())
}

/// `newm-river validate`: the config loads and gives the model its settings.
pub fn validate(path: &Path) -> Result<()> {
    let config = config::load(path)?;
    let _ = model::Desktop::new(
        config::settings(&config),
        config.window_rules,
        Instant::now(),
    );
    Ok(())
}
