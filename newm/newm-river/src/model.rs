//! The window manager's decisions, free of Wayland: one newm canvas per output, the windows on
//! them, the keyboard focus, and what every key binding or IPC action does to them.
//!
//! The river layer (`river.rs`) feeds this model river's events and turns its answers into
//! requests: [`Desktop::manage`] and [`Desktop::act`] in a manage sequence, the sizes
//! [`Desktop::proposals`] asks of windows, and in a render sequence the [`Frame`]s
//! [`Desktop::render`] places them by. Geometry: a tile's cell
//! sits on its grid lines inset by half the gaps, the window inside the cell's border, all drawn
//! through a camera that scales about the canvas origin.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use newm_canvas::{Canvas, Config as CanvasConfig, Dir, Edge, Id, Rect as Tiles};
use niri_config::window_rule::{OnXdgActivate, WindowRule};
use niri_config::{Action, PresetSize};
use niri_ipc::{CanvasDirection, CanvasEdge, CanvasSide};
use serde_json::json;
use tracing::info;

use crate::block::{Block, Park};
use crate::placeholder::Placeholders;
use crate::rules::{self, Corner, OpenHint, Resolved, RuleInfo, Style};

mod block;
mod follow;
mod glide;
pub mod minimap;
mod monitors;
mod motion;
mod moves;
mod open;
mod rematch;
mod reserve;
mod resize;
mod usage;
mod view;

pub use rematch::{title_score, Slot, REMATCH_FOR, TITLE_MATCH};
pub use reserve::PlaceholderFrame;
pub use resize::ResizeBoxFrame;
pub use view::ViewSettings;

/// How long neighbours wait for a shrinking window to commit its smaller size.
/// The largest minimum size, in logical pixels, a window's tiles grow to (its app's minimum, or a
/// size it committed): a client may ask for any i32, and the canvas's tile math (areas, edges)
/// must stay in i32.
const MAX_MIN_PX: i32 = 65535;

pub const SHRINK_WAIT: Duration = Duration::from_millis(300);

/// How long after start windows match `at-startup` rules.
const STARTUP: Duration = Duration::from_secs(60);
/// How long a window of a parked canvas waits for the outputs coming back to have names, before
/// it is placed on the outputs there are.
const NAMING_WAIT: Duration = Duration::from_secs(3);
/// The parked canvases kept: the most recent outputs to leave.
const PARKED_MAX: usize = 8;

pub type OutputId = u32;

/// A canvas window before an operation: its id, tiles and position on screen.
type Before = (Id, Tiles, Option<(i32, i32)>);

/// A tiled window's minimum for its canvas, its output, and why it changes (for `app-grow`).
type MinUpdate = (Id, Option<OutputId>, (f64, f64), &'static str);

/// A rectangle in the compositor's logical coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn intersect(self, o: Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

/// The glow drawn around the focused window, from the config's `shadow`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlowStyle {
    pub softness: f64,
    pub spread: f64,
    pub offset: (f64, f64),
    /// Premultiplied.
    pub color: [f32; 4],
}

/// Layout settings the model takes from the config.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub canvas: CanvasConfig,
    pub gaps: f64,
    pub border: Style,
    pub glow: Option<GlowStyle>,
    pub default_width: Option<PresetSize>,
    pub default_height: Option<PresetSize>,
    pub corner_radius: f64,
    /// `layout.canvas.floating-follow`: how floating windows follow the camera.
    pub floating_follow: niri_ipc::FloatingFollow,
    /// `layout.canvas.pan-momentum`: the share of a Super pan's fling kept on release.
    pub pan_momentum: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            canvas: CanvasConfig::default(),
            gaps: 10.,
            border: Style {
                width: 1,
                active: [0.5, 0.9, 0.6, 1.],
                inactive: [0.; 4],
                urgent: [0.61, 0., 0., 1.],
            },
            glow: None,
            default_width: None,
            default_height: None,
            corner_radius: 0.,
            floating_follow: niri_ipc::FloatingFollow::Screen,
            pan_momentum: 1.,
        }
    }
}

/// A window's size kept along one axis when it was asked for less: it counts
/// as the window's minimum beside the one its app declares, so its span grows to hold it.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Kept {
    pub size: i32,
    pub margin: i32,
}

impl Kept {
    /// After a commit of `got` in answer to a request for `asked` (0: the app picks).
    pub fn after(self, asked: i32, got: i32) -> Self {
        if asked <= 0 {
            self
        } else if got > asked {
            let margin = got - asked;
            let size = if margin == self.margin {
                self.size
            } else {
                got
            };
            Self { size, margin }
        } else if got < self.size {
            Self::default()
        } else {
            Self { margin: 0, ..self }
        }
    }
}

/// Where a floating window goes once its size is known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    /// Centred on its parent window.
    Parent(Id),
    Corner(Corner),
    /// Centred in its output's working area.
    Centre,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    Tiled,
    Floating {
        anchor: Anchor,
        /// The window's top-left, once placed.
        pos: Option<(i32, i32)>,
    },
}

/// Where an activation request came from (newm_river_hints_v1.activate's source).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivateSource {
    /// xdg-activation-v1 with a token made from a fresh input serial.
    Token,
    /// xdg-activation-v1 with a token naming no seat or serial.
    UrgentOnly,
    /// A taskbar's wlr-foreign-toplevel-management activate.
    ForeignToplevel,
}

#[derive(Clone, Debug, Default)]
pub struct Window {
    pub id: Id,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub pid: Option<i32>,
    pub parent: Option<Id>,
    /// The app's minimum and maximum size (0: none).
    pub min: (i32, i32),
    pub max: (i32, i32),
    /// The size river last reported.
    pub size: Option<(i32, i32)>,
    /// The size last proposed.
    pub asked: Option<(i32, i32)>,
    pub kept: (Kept, Kept),
    pub output: Option<OutputId>,
    /// None until the window is placed in a manage sequence.
    pub place: Option<Place>,
    pub fullscreen: bool,
    pub rules: Resolved,
    pub focus_time: Option<Duration>,
    /// A new tiled window waiting off its canvas for its app's first commit (`model/open.rs`).
    pub opening: Option<open::Opening>,
    /// A new tiled window on its way to its cell's size, for the usage log.
    pub watch: Option<open::Watch>,
    /// The saved spot it took in a restore, while its title may still move it (see
    /// `model/rematch.rs`).
    pub slot: Option<Slot>,
    /// A floating window's size from a resize drag, asked of it until it changes (`None`: the
    /// size its app picked).
    pub resized: Option<(i32, i32)>,
    /// A floating window's resize drag by its left or top edge: its commits move it so that the
    /// opposite edge stays (`model/resize.rs`).
    pub grip: Option<resize::Grip>,
    /// The last Super+right press on it that picked edges, and when: the next is a double click
    /// within [`resize::DOUBLE_CLICK_TIME`] (niri's `last_interactive_resize_start`).
    pub resize_press: Option<(Instant, resize::Edges)>,
    /// Rang the system bell while unfocused; cleared when it takes the focus.
    pub urgent: bool,
    /// A modal dialog (xdg-dialog-v1): while it is up, focusing its parent focuses it.
    pub modal: bool,
}

impl Window {
    pub fn is_tiled(&self) -> bool {
        self.place == Some(Place::Tiled)
    }

    pub fn is_floating(&self) -> bool {
        matches!(self.place, Some(Place::Floating { .. }))
    }
}

pub struct Output {
    pub id: OutputId,
    pub name: String,
    /// The output's area in the global space.
    pub area: Rect,
    /// What layer-shell exclusive zones leave of it.
    pub usable: Option<Rect>,
    pub canvas: Canvas,
    /// The span the last window of each app closed or moved away from here had, the most recent
    /// last (`model/open.rs`).
    pub span_memory: Vec<(String, (i32, i32))>,
}

impl Output {
    pub fn work_area(&self) -> Rect {
        self.usable
            .and_then(|u| u.intersect(self.area))
            .unwrap_or(self.area)
    }
}

/// Where a window goes in a render sequence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub id: Id,
    /// Top-left of the window's content.
    pub pos: (i32, i32),
    /// The output's area relative to the window's top-left: what the window, its border and its
    /// glow are clipped to. None when no part of it is on its output: it is hidden.
    pub clip: Option<Rect>,
    pub floating: bool,
    pub fullscreen: bool,
    pub focused: bool,
    /// Border width and premultiplied colour (width 0: none).
    pub border: (i32, [f32; 4]),
    /// The camera scale the window is drawn at (1 unless zoomed).
    pub scale: f64,
    /// The opacity the window rules give it now.
    pub opacity: f32,
}

/// What an action asks of the river layer.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Spawn(Vec<String>),
    SpawnSh(String),
    /// A command started in its directory, with variables on top of newm-river's environment.
    SpawnIn {
        command: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        env: Vec<(String, String)>,
    },
    Close(Id),
    Quit,
    /// `quit` without `skip-confirmation`: the exit confirmation asks first.
    ConfirmQuit,
    Fullscreen(Id, bool),
    /// Toggle the keyboard shortcuts inhibitor of the focused surface (niri's
    /// toggle-keyboard-shortcuts-inhibit).
    ToggleShortcutsInhibit,
    /// `toggle-synth-background` (river.rs: the synth background is the window manager's own
    /// surfaces, not the model's).
    ToggleSynthBackground,
    /// `set-synth-background`.
    SetSynthBackground(niri_ipc::SynthMode),
    /// Not done by newm-river (yet): logged.
    Unsupported(String),
}

pub struct Desktop {
    pub settings: Settings,
    rules: Vec<WindowRule>,
    outputs: Vec<Output>,
    windows: BTreeMap<Id, Window>,
    focus: Option<Id>,
    /// Window ids, the most recently focused first.
    mru: Vec<Id>,
    active_output: Option<OutputId>,
    shrinking: Vec<Id>,
    held: HashMap<Id, (i32, i32)>,
    shrink_deadline: Option<Instant>,
    started: Instant,
    /// The keyboard focus changed since river was last told.
    pub focus_dirty: bool,
    /// The pointer's drag and pan, the zoom a gesture pulled back from.
    moves: moves::Moves,
    /// The plan of the canvas shown after a move (`model/minimap.rs`).
    minimap: minimap::Minimap,
    /// Tiles held for launching apps and for a restored session's windows.
    pub placeholders: Placeholders,
    /// Blocks a client placed, the most recent last (`model/block.rs`).
    blocks: Vec<Block>,
    /// The last park and its canvas, for undo to take back in one step.
    park: Option<(OutputId, Park)>,
    /// Cameras, overview and switch (`model/view.rs`).
    view: view::View,
    /// Windows gliding to new tiles and opening (`model/glide.rs`).
    glides: glide::Glides,
    /// Floating windows following the camera (`model/follow.rs`).
    follows: follow::Follows,
    /// Motion episodes for the usage log (`model/motion.rs`).
    motion: motion::Motion,
    /// A pick for `queryWindowInfo` ended: where its click was, or none when cancelled.
    pub picked: Option<Option<(i32, i32)>>,
    /// New windows stay unplaced until then ([`Self::hold_new_windows`]).
    hold_new: Option<Instant>,
    /// The next manage sequence places the held windows: those no saved spot takes open
    /// unfocused, so the saved focus and view win.
    released: bool,
    /// The canvases of outputs that left, by output name, until an output of that name comes
    /// back ([`Parked`]).
    parked: Vec<Parked>,
    /// Until then, a window waiting on a parked canvas is not placed again while an output has
    /// no name yet: the name (wl_output) comes a round trip after the output, and the output of
    /// that name takes the window back on its tiles ([`Self::set_output_name`]).
    naming_until: Option<Instant>,
}

/// The canvas of an output that left: a VT switch away (wlroots destroys every output while the
/// session is inactive and makes new ones when it comes back), a monitor turned off with
/// wlr-output-management, or a DisplayPort monitor that unplugs itself when it sleeps. The output
/// of the same name takes it back with its windows' tiles and its camera, as niri gives a
/// workspace back to the output it came from. With no output left the windows wait on it; with
/// another output left they are placed there meanwhile.
struct Parked {
    /// The output's name (its connector, e.g. DP-1).
    name: String,
    /// No output was left after it (a VT switch away, every monitor off): its windows wait for
    /// the outputs to come back with names before they are placed again.
    waiting: bool,
    canvas: Canvas,
    span_memory: Vec<(String, (i32, i32))>,
    /// Where each of its windows was: tiled on the canvas, or floating.
    places: Vec<(Id, Place)>,
}

fn dir(d: CanvasDirection) -> Dir {
    match d {
        CanvasDirection::Left => Dir::Left,
        CanvasDirection::Right => Dir::Right,
        CanvasDirection::Up => Dir::Up,
        CanvasDirection::Down => Dir::Down,
    }
}

fn edge(e: CanvasEdge) -> Edge {
    match e {
        CanvasEdge::Left => Edge::Left,
        CanvasEdge::Right => Edge::Right,
        CanvasEdge::Top => Edge::Top,
        CanvasEdge::Bottom => Edge::Bottom,
    }
}

impl Desktop {
    pub fn new(settings: Settings, rules: Vec<WindowRule>, now: Instant) -> Self {
        let follow_mode = settings.floating_follow;
        Self {
            settings,
            rules,
            outputs: Vec::new(),
            windows: BTreeMap::new(),
            focus: None,
            mru: Vec::new(),
            active_output: None,
            shrinking: Vec::new(),
            held: HashMap::new(),
            shrink_deadline: None,
            hold_new: None,
            released: false,
            started: now,
            focus_dirty: false,
            moves: moves::Moves::default(),
            minimap: minimap::Minimap::default(),
            placeholders: Placeholders::default(),
            blocks: Vec::new(),
            park: None,
            view: view::View::default(),
            glides: glide::Glides::default(),
            follows: follow::Follows::new(follow_mode),
            motion: motion::Motion::default(),
            picked: None,
            parked: Vec::new(),
            naming_until: None,
        }
    }

    /// A reloaded config: the layout look, the canvas config of every output and the window
    /// rules, resolved again for every open window.
    pub fn reconfigure(&mut self, settings: Settings, rules: Vec<WindowRule>, now: Instant) {
        let canvas = settings.canvas;
        let follow_mode = settings.floating_follow;
        self.settings = settings;
        self.rules = rules;
        self.follow_config(follow_mode);
        let ids: Vec<Id> = self
            .windows
            .values()
            .filter(|w| w.place.is_some())
            .map(|w| w.id)
            .collect();
        for id in ids {
            let w = &self.windows[&id];
            let resolved =
                rules::resolve(&self.rules, &self.rule_info(w, now), self.settings.border);
            if let Some(w) = self.windows.get_mut(&id) {
                w.rules = resolved;
            }
        }
        let outs: Vec<OutputId> = self.outputs.iter().map(|o| o.id).collect();
        for out in outs {
            let mins: Vec<(Id, (f64, f64))> = self
                .windows
                .values()
                .filter(|w| w.is_tiled() && w.output == Some(out))
                .map(|w| (w.id, self.tile_min(w)))
                .collect();
            self.logged_with(
                "app-grow",
                Some(out),
                None,
                |c| {
                    if *c.config() != canvas {
                        c.set_config(canvas);
                    }
                    let mut placed = true;
                    for (id, (min_w, min_h)) in mins {
                        if c.window(id).is_some() {
                            placed &= c.set_min(id, min_w, min_h);
                        }
                    }
                    placed
                },
                |_, &placed| json!({ "cause": "config", "pushed": !placed }),
            );
        }
        self.sync_mins();
    }

    pub fn focus(&self) -> Option<Id> {
        self.focus
    }

    pub fn window(&self, id: Id) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub fn window_mut(&mut self, id: Id) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    pub fn output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.iter().find(|o| o.id == id)
    }

    fn output_mut(&mut self, id: Option<OutputId>) -> Option<&mut Output> {
        let id = id?;
        self.outputs.iter_mut().find(|o| o.id == id)
    }

    pub fn active_output(&self) -> Option<OutputId> {
        self.active_output
            .filter(|&id| self.output(id).is_some())
            .or_else(|| self.outputs.first().map(|o| o.id))
    }

    // ---- outputs -------------------------------------------------------------------------

    pub fn add_output(&mut self, id: OutputId) {
        // The first output back after the only one left takes its canvas at once; with more
        // parked canvases each waits for the output of its name.
        let parked = (self.outputs.is_empty() && self.parked.len() == 1)
            .then(|| self.parked.pop())
            .flatten();
        self.naming_until = Some(Instant::now() + NAMING_WAIT);
        self.outputs.push(Output {
            id,
            name: format!("OUT-{id}"),
            area: Rect::default(),
            usable: None,
            canvas: Canvas::new(self.settings.canvas, 1., 1.),
            span_memory: Vec::new(),
        });
        if let Some(p) = parked {
            self.unpark(id, p);
        }
    }

    /// Output `id` takes back a parked canvas: its windows still open, on their tiles, taken
    /// from wherever they were placed meanwhile.
    fn unpark(&mut self, id: OutputId, p: Parked) {
        let Parked {
            name,
            waiting: _,
            mut canvas,
            span_memory,
            places,
        } = p;
        if self
            .output(id)
            .is_none_or(|o| !o.canvas.windows().is_empty())
        {
            info!("output {id} keeps its own windows: {name}'s parked canvas is dropped");
            return;
        }
        // Windows closed meanwhile leave the canvas they waited on.
        let gone: Vec<Id> = canvas
            .windows()
            .iter()
            .map(|w| w.id)
            .filter(|i| !self.windows.contains_key(i))
            .collect();
        for i in gone {
            canvas.remove(i);
        }
        let mut back = 0;
        for (wid, place) in places {
            let Some(w) = self.windows.get(&wid) else {
                continue;
            };
            if let Some(other) = w.output.filter(|&o| o != id)
                && let Some(o) = self.output_mut(Some(other))
            {
                o.canvas.remove(wid);
            }
            let w = self.windows.get_mut(&wid).unwrap();
            w.output = Some(id);
            w.place = Some(place);
            back += 1;
        }
        info!("output {id} takes back {name}'s parked canvas: {back} windows on their tiles");
        if let Some(o) = self.output_mut(Some(id)) {
            o.canvas = canvas;
            o.span_memory = span_memory;
        }
    }

    /// Whether an output is there to show windows; none during a VT switch away or while the
    /// only monitor is off.
    pub fn has_outputs(&self) -> bool {
        !self.outputs.is_empty()
    }

    pub fn set_output_name(&mut self, id: OutputId, name: String) {
        let Some(o) = self.output_mut(Some(id)) else {
            return;
        };
        o.name = name.clone();
        if let Some(k) = self.parked.iter().position(|p| p.name == name) {
            let p = self.parked.remove(k);
            self.unpark(id, p);
        }
    }

    /// Whether window `id` waits for the output of its parked canvas's name, which is not
    /// placed again meanwhile (see `naming_until`).
    fn awaits_its_output(&self, id: Id, now: Instant) -> bool {
        self.naming_until.is_some_and(|t| now < t)
            && self.outputs.iter().any(|o| o.name.starts_with("OUT-"))
            && self
                .parked
                .iter()
                .any(|p| p.waiting && p.places.iter().any(|&(i, _)| i == id))
    }

    pub fn set_output_area(&mut self, id: OutputId, area: Rect) {
        if let Some(o) = self.output_mut(Some(id)) {
            o.area = area;
        }
    }

    pub fn set_output_usable(&mut self, id: OutputId, usable: Rect) {
        if let Some(o) = self.output_mut(Some(id)) {
            o.usable = Some(usable);
        }
    }

    /// An output left: its windows move to the first output left, or, when it was the last one,
    /// wait on its canvas for the next output ([`Parked`]).
    pub fn remove_output(&mut self, id: OutputId) {
        let Some(k) = self.outputs.iter().position(|o| o.id == id) else {
            return;
        };
        let o = self.outputs.remove(k);
        self.drop_placeholders(id);
        let mut places = Vec::new();
        for w in self.windows.values_mut() {
            if w.output == Some(id) {
                if let Some(place) = w.place {
                    places.push((w.id, place));
                }
                w.output = None;
                w.place = None;
            }
        }
        if self.outputs.is_empty() {
            info!(
                "no output left: {} windows wait on {}'s canvas",
                places.len(),
                o.name
            );
        } else {
            info!(
                "{} left: {} windows go to another output until it comes back",
                o.name,
                places.len()
            );
        }
        self.parked.retain(|p| p.name != o.name);
        if self.parked.len() >= PARKED_MAX {
            self.parked.remove(0);
        }
        self.parked.push(Parked {
            name: o.name,
            waiting: false,
            canvas: o.canvas,
            span_memory: o.span_memory,
            places,
        });
        if self.outputs.is_empty() {
            for p in &mut self.parked {
                p.waiting = true;
            }
        }
    }

    fn inset(&self) -> f64 {
        (self.settings.gaps / 2.).round()
    }

    /// Gives every canvas the screen its output's working area leaves, less the gaps.
    fn sync_screens(&mut self) {
        let inset = self.inset();
        for o in &mut self.outputs {
            let wa = o.work_area();
            let screen = (
                (f64::from(wa.w) - inset * 2.).max(1.),
                (f64::from(wa.h) - inset * 2.).max(1.),
            );
            if o.canvas.screen() != screen {
                o.canvas.set_screen(screen.0, screen.1);
            }
        }
    }

    // ---- geometry ------------------------------------------------------------------------

    /// A rectangle's cell at 1:1, relative to the canvas origin: both sides on their grid line,
    /// inset by half the gaps.
    fn cell(&self, canvas: &Canvas, r: Tiles) -> (f64, f64, f64, f64) {
        let (tw, th) = canvas.base_tile();
        let ex = |k: i32| (f64::from(k) * tw).round();
        let ey = |k: i32| (f64::from(k) * th).round();
        let inset = self.inset();
        let (x0, x1, y0, y1) = (ex(r.i), ex(r.right()), ey(r.j), ey(r.bottom()));
        (
            x0 + inset,
            y0 + inset,
            (x1 - x0 - inset * 2.).max(1.),
            (y1 - y0 - inset * 2.).max(1.),
        )
    }

    /// The size a tiled window is asked to take: its cell less its border.
    fn tiled_size(&self, o: &Output, w: &Window, r: Tiles) -> (i32, i32) {
        let (_, _, cw, ch) = self.cell(&o.canvas, r);
        let b = f64::from(w.rules.border.width * 2);
        (
            (cw - b).floor().max(1.) as i32,
            (ch - b).floor().max(1.) as i32,
        )
    }

    /// The top-left of a tiled window's content through the camera.
    fn tiled_pos(&self, o: &Output, w: &Window, r: Tiles) -> (i32, i32) {
        let wa = o.work_area();
        let inset = self.inset();
        let (cx, cy, _, _) = self.cell(&o.canvas, r);
        let cam = self.camera(o);
        let (s, (vx, vy)) = (cam.scale, cam.view);
        let b = f64::from(w.rules.border.width);
        (
            (f64::from(wa.x) + inset + (cx + b) * s - vx).round() as i32,
            (f64::from(wa.y) + inset + (cy + b) * s - vy).round() as i32,
        )
    }

    /// The minimum the canvas keeps for a window: the larger of its app's minimum and the size it
    /// kept, with its border, the gaps and a pixel for rounding.
    fn tile_min(&self, w: &Window) -> (f64, f64) {
        let pad = f64::from(w.rules.border.width) * 2. + self.inset() * 2. + 1.;
        let cap = |px: i32| f64::from(px.clamp(1, MAX_MIN_PX));
        (
            cap(w.min.0.max(w.kept.0.size)) + pad,
            cap(w.min.1.max(w.kept.1.size)) + pad,
        )
    }

    /// The window's content rectangle on screen now, if it has one.
    pub fn window_rect(&self, id: Id) -> Option<Rect> {
        let w = self.windows.get(&id)?;
        let o = self.output(w.output?)?;
        if w.fullscreen {
            return Some(o.area);
        }
        let g = self.glide_offset(id);
        let pos = match w.place? {
            Place::Tiled => {
                let r = self.shown_tiles(o, id)?;
                match self.held.get(&id) {
                    Some(&p) => p,
                    None => {
                        let p = self.tiled_pos(o, w, r);
                        let s = self.camera(o).scale;
                        (
                            p.0 + (g.0 * s).round() as i32,
                            p.1 + (g.1 * s).round() as i32,
                        )
                    }
                }
            }
            Place::Floating { pos, .. } => {
                let p = pos?;
                (p.0 + g.0.round() as i32, p.1 + g.1.round() as i32)
            }
        };
        let pos = self.moves.offset(id, pos);
        let size = w.size.or_else(|| match w.place? {
            Place::Tiled => Some(self.tiled_size(o, w, o.canvas.rect(id)?)),
            Place::Floating { .. } => None,
        })?;
        Some(Rect::new(pos.0, pos.1, size.0, size.1))
    }

    /// Where window `w` with content rectangle `rect` is drawn now and at what scale, before its
    /// open animation: a tiled one through the camera, a floating one through what it follows
    /// (`floating-follow`) or else through the overview's camera, a fullscreen one as is.
    fn drawn(&self, w: &Window, o: &Output, rect: Rect) -> (Rect, f64) {
        if w.fullscreen {
            (rect, 1.)
        } else if w.is_tiled() {
            (rect, self.camera(o).scale)
        } else if let Some(m) = self.follow_map(w.id) {
            (m.rect(rect), m.scale)
        } else {
            let (pos, scale) = self.floating_through_camera(o, (rect.x, rect.y));
            (Rect::new(pos.0, pos.1, rect.w, rect.h), scale)
        }
    }

    /// Whether a window's committed size fits the cell its tiles give it now.
    fn fits_cell(&self, id: Id) -> bool {
        let Some(w) = self.windows.get(&id) else {
            return true;
        };
        let (Some(o), Some(size)) = (w.output.and_then(|o| self.output(o)), w.size) else {
            return true;
        };
        let Some(r) = o.canvas.rect(id) else {
            return true;
        };
        let want = self.tiled_size(o, w, r);
        size.0 <= want.0 && size.1 <= want.1
    }

    // ---- windows -------------------------------------------------------------------------

    pub fn add_window(&mut self, id: Id) {
        self.windows.insert(
            id,
            Window {
                id,
                ..Window::default()
            },
        );
    }

    fn rule_info<'a>(&self, w: &'a Window, now: Instant) -> RuleInfo<'a> {
        RuleInfo {
            app_id: w.app_id.as_deref(),
            title: w.title.as_deref(),
            is_focused: self.focus == Some(w.id),
            is_floating: w.is_floating(),
            at_startup: now.duration_since(self.started) < STARTUP,
            is_urgent: w.urgent,
        }
    }

    /// The span a new tiled window asks for: its rules' default size, else the canvas's, along
    /// each axis; one tile on an axis nothing sizes.
    fn wanted_span(&self, canvas: &Canvas, r: &Resolved) -> Option<(i32, i32)> {
        let width = r.default_width.unwrap_or(self.settings.default_width);
        let height = r.default_height.unwrap_or(self.settings.default_height);
        (width.is_some() || height.is_some()).then(|| {
            (
                width.map_or(1, |w| open::preset_span(canvas, w, true)),
                height.map_or(1, |h| open::preset_span(canvas, h, false)),
            )
        })
    }

    /// Places a new window by its rules and hints, on the active output.
    fn place(&mut self, id: Id, now: Instant) {
        let Some(out) = self.active_output() else {
            return;
        };
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        let info = self.rule_info(w, now);
        let resolved = rules::resolve(&self.rules, &info, self.settings.border);
        let size = self.output(out).map(|o| (o.area.w, o.area.h));
        let hint = OpenHint::compute(w.parent.is_some(), w.min, w.max, size);
        let overlay = resolved.open_floating.is_none() && hint == OpenHint::Overlay;
        let floating = resolved.open_floating.unwrap_or(hint != OpenHint::Tiled);
        let parent = w.parent.filter(|p| self.windows.contains_key(p));
        // A window of the running session no saved spot took (`hold_new_windows`) keeps the saved
        // focus and view.
        let focus = resolved.open_focused.unwrap_or(!overlay) && !self.released;
        let decision = if overlay {
            "overlay"
        } else if !floating {
            "tiled"
        } else if w.parent.is_some() {
            "dialog"
        } else {
            "popup-like"
        };
        if usage::TELEMETRY {
            let float_line = usage::float_line(w, decision, resolved.open_floating.is_some(), size);
            self.emit(float_line);
        }
        let claimed = if floating {
            None
        } else {
            self.claim_placeholder(id, now)
        };
        let in_block = claimed.as_ref().is_some_and(|c| self.is_block_slot(c.0));
        if let Some(c) = &claimed {
            self.block_slot_taken(c.0, id);
        }
        // A block's window waits for the block to be complete to take the focus.
        let focus = focus && !in_block;
        let out = claimed.as_ref().map_or(out, |c| c.1);
        let place = if floating {
            let anchor = match (resolved.floating_position, parent) {
                (Some(c), _) => Anchor::Corner(c),
                (None, _) if overlay => Anchor::Corner(Corner::overlay(self.settings.gaps)),
                (None, Some(p)) => Anchor::Parent(p),
                (None, None) => Anchor::Centre,
            };
            Place::Floating { anchor, pos: None }
        } else {
            Place::Tiled
        };
        let w = self.windows.get_mut(&id).unwrap();
        w.rules = resolved;
        w.output = Some(out);
        w.place = Some(place);
        if claimed.is_none() {
            // Placed again after its output left, a restored window holds no saved spot.
            w.slot = None;
        }
        if place == Place::Tiled {
            let w = &self.windows[&id];
            let (min_w, min_h) = self.tile_min(w);
            if let Some((p, _, restore)) = claimed {
                let fields = json!({
                    "size_source": "placeholder",
                    "app_px": null,
                    "since_last_open_ms": usage::since_last_open(),
                    "restore": restore,
                });
                self.logged_with(
                    "open",
                    Some(out),
                    Some(id),
                    |c| {
                        c.rename(p, id);
                        c.set_min(id, min_w, min_h)
                    },
                    |d, _| {
                        let span = d.output(out).and_then(|o| o.canvas.rect(id));
                        let mut fields = fields;
                        fields["span"] = json!(span.map(|r| [r.w, r.h]));
                        fields["focused"] = json!(focus);
                        fields
                    },
                );
            } else {
                let rule = Self::rule_span(&self.output(out).unwrap().canvas, w);
                self.windows.get_mut(&id).unwrap().opening = Some(open::Opening { rule });
            }
        }
        if focus {
            self.focus_window(id, now);
        } else if self.focus.is_none() {
            self.focus_dirty = true;
        }
        self.settle_blocks(now);
    }

    /// A window closed: it leaves its canvas, and when it had the focus the most recently used
    /// window left takes it.
    pub fn remove_window(&mut self, id: Id, now: Instant) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        // A modal dialog gives the focus back to its parent.
        let back_to = w.modal.then_some(w.parent).flatten();
        if w.is_tiled() {
            let out = w.output;
            let app = w.app_id.clone();
            if let (Some(app), Some(o)) = (app, self.output_mut(out))
                && let Some(r) = o.canvas.rect(id)
            {
                open::remember_span(o, app, (r.w, r.h));
            }
            self.logged("close", out, Some(id), |c| c.remove(id));
        }
        self.forget_glides(id);
        self.moves.forget(id);
        let Some(_) = self.windows.remove(&id) else {
            return;
        };
        self.mru.retain(|&m| m != id);
        self.shrinking.retain(|&s| s != id);
        self.held.remove(&id);
        if self.shrinking.is_empty() {
            self.release_held();
        }
        if self.focus == Some(id) {
            self.focus = None;
            self.focus_dirty = true;
            let placed = |m: &Id| self.windows.get(m).is_some_and(|w| w.place.is_some());
            let next = back_to
                .filter(placed)
                .or_else(|| self.mru.iter().copied().find(placed));
            if let Some(next) = next {
                self.focus_window(next, now);
            }
        }
    }

    /// Focuses a window and makes it the most recently used one; a tiled window's canvas shows
    /// it whole.
    pub fn focus_window(&mut self, id: Id, now: Instant) -> bool {
        let id = self.modal_target(id);
        let Some(w) = self.windows.get(&id) else {
            return false;
        };
        if w.place.is_none() {
            return false;
        }
        let (tiled, output) = (w.is_tiled(), w.output);
        if tiled && let Some(o) = self.output_mut(output) {
            o.canvas.focus_id(id);
        }
        if self.focus != Some(id) {
            self.focus_dirty = true;
        }
        self.focus = Some(id);
        self.mru.retain(|&m| m != id);
        self.mru.insert(0, id);
        self.active_output = output;
        let t = now.duration_since(self.started);
        if let Some(w) = self.windows.get_mut(&id) {
            w.focus_time = Some(t);
            w.urgent = false;
        }
        true
    }

    /// The window focusing `id` focuses: its newest placed modal dialog (xdg-dialog-v1), and that
    /// dialog's, down the chain; `id` itself without one.
    fn modal_target(&self, mut id: Id) -> Id {
        for _ in 0..self.windows.len() {
            let child = self
                .windows
                .values()
                .filter(|c| c.modal && c.parent == Some(id) && c.place.is_some())
                .max_by_key(|c| c.id);
            match child {
                Some(c) => id = c.id,
                None => break,
            }
        }
        id
    }

    /// A window rang the system bell: unfocused, it turns urgent (its border takes the urgent
    /// colour, IPC says `is_urgent`). Returns whether that changed anything.
    pub fn on_bell(&mut self, id: Id) -> bool {
        let focused = self.focus == Some(id);
        match self.windows.get_mut(&id) {
            Some(w) if !focused && !w.urgent => {
                w.urgent = true;
                true
            }
            _ => false,
        }
    }

    /// A client asked to activate a window (newm_river_hints_v1.activate; river checked the token).
    /// The policy, for xdg-activation and the foreign-toplevel `activate`: a taskbar's request
    /// focuses; an xdg-activation token focuses unless it is urgency-only, which marks the window
    /// urgent; the window rule `on-xdg-activate` overrides both for xdg-activation. Urgency is set
    /// here; returns whether the window should take the focus (the caller runs
    /// `Action::FocusWindow`, as IPC does).
    pub fn on_activate(&mut self, id: Id, source: ActivateSource) -> bool {
        let focused = self.focus == Some(id);
        let Some(w) = self.windows.get_mut(&id) else {
            return false;
        };
        let focus = match (source, w.rules.on_xdg_activate) {
            (ActivateSource::ForeignToplevel, _) => true,
            (_, Some(OnXdgActivate::Ignore)) => return false,
            (_, Some(OnXdgActivate::SetUrgent)) => false,
            (_, Some(OnXdgActivate::Focus)) => true,
            (source, None) => source == ActivateSource::Token,
        };
        if !focus && !focused {
            w.urgent = true;
        }
        focus && !focused
    }

    /// A window became or stopped being a modal dialog. A dialog that turns modal while its
    /// parent has the focus takes it. Returns whether the focus moved.
    pub fn set_modal(&mut self, id: Id, modal: bool, now: Instant) -> bool {
        let Some(w) = self.windows.get_mut(&id) else {
            return false;
        };
        w.modal = modal;
        let parent = w.parent;
        let placed = w.place.is_some();
        if modal && placed && parent.is_some() && self.focus == parent {
            return self.focus_window(id, now);
        }
        false
    }

    /// River reported a window's size. A new tiled window's first commit puts it on its canvas
    /// (`model/open.rs`). Returns whether the canvas needs a manage sequence (the window opened,
    /// or kept a size larger than asked).
    pub fn on_dimensions(&mut self, id: Id, width: i32, height: i32) -> bool {
        let Some(w) = self.windows.get_mut(&id) else {
            return false;
        };
        let first = w.size.is_none();
        let prev = w.size.replace((width, height));
        self.grip_commit(id, prev, Instant::now());
        let Some(w) = self.windows.get_mut(&id) else {
            return false;
        };
        let opening = w.opening.is_some();
        if first {
            self.start_open(id);
            self.watch(id, Instant::now());
        }
        if opening {
            self.open_one(id);
        }
        self.check_settled(id, Instant::now());
        let Some(w) = self.windows.get_mut(&id) else {
            return false;
        };
        let mut relayout = opening;
        if w.is_tiled()
            && !w.fullscreen
            && let Some(asked) = w.asked
        {
            let kept = (
                w.kept.0.after(asked.0, width),
                w.kept.1.after(asked.1, height),
            );
            relayout |= kept.0.size > w.kept.0.size || kept.1.size > w.kept.1.size;
            w.kept = kept;
        }
        if self.shrinking.contains(&id) && self.fits_cell(id) {
            self.shrinking.retain(|&s| s != id);
            if self.shrinking.is_empty() {
                self.release_held();
                self.shrink_deadline = None;
            }
        }
        relayout
    }

    /// The window's minimum size from its app.
    pub fn set_hints(&mut self, id: Id, min: (i32, i32), max: (i32, i32)) {
        if let Some(w) = self.windows.get_mut(&id) {
            w.min = (min.0.max(0), min.1.max(0));
            w.max = (max.0.max(0), max.1.max(0));
        }
    }

    /// Updates every tiled window's minimum on its canvas from its app's and its kept size. A
    /// window that grows takes its tiles through the placement rules (newm-canvas `set_min`),
    /// logged as `app-grow` with its cause (its app's minimum or a size it kept) and whether the
    /// rules fell back to the push.
    fn sync_mins(&mut self) {
        let mins: Vec<MinUpdate> = self
            .windows
            .values()
            .filter(|w| w.is_tiled())
            .map(|w| {
                let kept = w.kept.0.size > w.min.0 || w.kept.1.size > w.min.1;
                let cause = if kept { "kept-commit" } else { "min-size" };
                (w.id, w.output, self.tile_min(w), cause)
            })
            .collect();
        for (id, out, (min_w, min_h), cause) in mins {
            let Some(o) = out.and_then(|o| self.output(o)) else {
                continue;
            };
            let Some(cw) = o.canvas.window(id) else {
                continue;
            };
            if (cw.min_w, cw.min_h) != (min_w, min_h) {
                self.logged_with(
                    "app-grow",
                    out,
                    Some(id),
                    |c| c.set_min(id, min_w, min_h),
                    |_, &placed| json!({ "cause": cause, "pushed": !placed }),
                );
            }
        }
    }

    /// Runs a canvas operation on an output's canvas. A window it shrinks that is still drawn
    /// larger holds every window it moved in place until it commits the smaller size (or
    /// [`SHRINK_WAIT`] passes), so that no neighbour slides over it. A drag's preview goes; a
    /// resize drag's comes back worked out on the changed canvas.
    fn apply<R>(&mut self, out: Option<OutputId>, op: impl FnOnce(&mut Canvas) -> R) -> Option<R> {
        let snap = self.glide_snapshot(out);
        self.moves.clear_preview();
        let o = self.output(out?)?;
        let before: Vec<Before> = o
            .canvas
            .windows()
            .iter()
            .map(|cw| {
                let pos = self.window_rect(cw.id).map(|r| (r.x, r.y));
                (cw.id, cw.rect, pos)
            })
            .collect();
        let rv = op(&mut self.output_mut(out)?.canvas);
        let o = self.output(out?)?;
        let mut shrunk = Vec::new();
        for &(id, old, _) in &before {
            if let Some(r) = o.canvas.rect(id)
                && (r.w < old.w || r.h < old.h)
                && !self.shrinking.contains(&id)
                && !self.fits_cell(id)
            {
                shrunk.push(id);
            }
        }
        if !shrunk.is_empty() {
            self.shrinking.extend(shrunk);
            self.shrink_deadline = Some(Instant::now() + SHRINK_WAIT);
        }
        if !self.shrinking.is_empty() {
            for (id, _, pos) in before {
                if let Some(pos) = pos
                    && !self.held.contains_key(&id)
                    && self.window_rect(id).is_some_and(|r| (r.x, r.y) != pos)
                {
                    self.held.insert(id, pos);
                }
            }
        }
        self.glide_from(out, snap);
        self.repreview_resize();
        Some(rv)
    }

    /// The windows held for a shrinking one go to their tiles, gliding from where they waited.
    fn release_held(&mut self) {
        let held: Vec<(Id, (i32, i32))> = self.held.drain().collect();
        for (id, at) in held {
            let Some(w) = self.windows.get(&id) else {
                continue;
            };
            let Some(o) = w.output.and_then(|o| self.output(o)) else {
                continue;
            };
            let Some(r) = self.shown_tiles(o, id) else {
                continue;
            };
            let p = self.tiled_pos(o, w, r);
            let s = self.camera(o).scale;
            let from = (f64::from(at.0 - p.0) / s, f64::from(at.1 - p.1) / s);
            self.start_glide(id, from);
        }
    }

    /// When neighbours stop waiting for a shrinking window, if they wait.
    pub fn deadline(&self) -> Option<Instant> {
        [
            self.shrink_deadline,
            self.hold_new,
            self.placeholders.deadline(),
            self.watch_deadline(),
            self.minimap_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Leaves new windows unplaced until `until` or [`Self::release_new_windows`]. A window
    /// manager started after a crash (`supervise.rs`) is handed every window of the running
    /// session at once: they wait for the saved spots to be put back, then take them.
    pub fn hold_new_windows(&mut self, until: Instant) {
        self.hold_new = Some(until);
    }

    pub fn release_new_windows(&mut self) {
        self.hold_new = None;
        self.released = true;
    }

    pub fn holding_new_windows(&self, now: Instant) -> bool {
        self.hold_new.is_some_and(|t| now < t)
    }

    /// Releases the held windows once [`SHRINK_WAIT`] passed. Returns whether it did.
    pub fn tick(&mut self, now: Instant) -> bool {
        self.watch_unsettled(now);
        let mut reserved = self.tick_placeholders(now);
        reserved |= self.tick_minimap(now);
        if self.hold_new.is_some_and(|t| now >= t) {
            self.hold_new = None;
            reserved = true;
        }
        if self.shrink_deadline.is_some_and(|d| now >= d) {
            self.shrink_deadline = None;
            self.shrinking.clear();
            self.release_held();
            return true;
        }
        reserved
    }

    /// The start of a manage sequence: screens from the outputs, new windows placed, minimums
    /// updated.
    pub fn manage(&mut self, now: Instant) {
        self.sync_screens();
        let hold = self.holding_new_windows(now);
        let fresh: Vec<Id> = self
            .windows
            .values()
            .filter(|w| w.place.is_none() && !hold)
            .map(|w| w.id)
            .filter(|&id| !self.awaits_its_output(id, now))
            .collect();
        for id in fresh {
            self.place(id, now);
        }
        self.released = false;
        self.open_mapped();
        self.finish_restores(now);
        self.sync_mins();
    }

    /// The sizes to ask of windows whose size changed since they were last asked: a tiled
    /// window's cell, and for a new floating one 0x0 (it picks its own size).
    pub fn proposals(&self) -> Vec<(Id, (i32, i32))> {
        let mut out = Vec::new();
        for w in self.windows.values() {
            if w.fullscreen {
                continue;
            }
            let want = match w.place {
                Some(Place::Tiled) if w.opening.is_some() => {
                    let o = w.output.and_then(|o| self.output(o));
                    let (Some(op), Some(o), None) = (w.opening, o, w.asked) else {
                        continue;
                    };
                    self.first_configure(o, w, op)
                }
                Some(Place::Tiled) => {
                    let Some(o) = w.output.and_then(|o| self.output(o)) else {
                        continue;
                    };
                    let Some(r) = o.canvas.rect(w.id) else {
                        continue;
                    };
                    self.tiled_size(o, w, r)
                }
                Some(Place::Floating { .. }) => match (w.resized, w.asked) {
                    (Some(size), _) => size,
                    (None, None) => (0, 0),
                    (None, Some(_)) => continue,
                },
                None => continue,
            };
            if w.asked != Some(want) {
                out.push((w.id, want));
            }
        }
        out
    }

    pub fn set_asked(&mut self, id: Id, size: (i32, i32)) {
        if let Some(w) = self.windows.get_mut(&id) {
            w.asked = Some(size);
            if let Some(watch) = &mut w.watch
                && watch.asked != Some(size)
            {
                watch.configures += 1;
                watch.asked = Some(size);
            }
        }
    }

    /// The largest size a window should take: its output's working area less the gaps and
    /// borders.
    pub fn bounds(&self, id: Id) -> Option<(i32, i32)> {
        let w = self.windows.get(&id)?;
        let wa = self.output(w.output?)?.work_area();
        let pad = (self.settings.gaps + f64::from(w.rules.border.width)) as i32 * 2;
        Some(((wa.w - pad).max(1), (wa.h - pad).max(1)))
    }

    /// Floating windows whose size is now known take their place.
    fn settle_floating(&mut self) {
        let mut placed = Vec::new();
        for w in self.windows.values() {
            let Some(Place::Floating { anchor, pos: None }) = w.place else {
                continue;
            };
            let (Some(size), Some(o)) = (w.size, w.output.and_then(|o| self.output(o))) else {
                continue;
            };
            let wa = o.work_area();
            let centre = |r: Rect| (r.x + (r.w - size.0) / 2, r.y + (r.h - size.1) / 2);
            let pos = match anchor {
                Anchor::Parent(p) => self
                    .settled(|| self.window_rect(p))
                    .map_or_else(|| centre(wa), centre),
                Anchor::Corner(c) => (
                    Corner::place(c.h, c.x, wa.x, wa.w, size.0),
                    Corner::place(c.v, c.y, wa.y, wa.h, size.1),
                ),
                Anchor::Centre => centre(wa),
            };
            placed.push((w.id, anchor, pos));
        }
        for (id, anchor, pos) in placed {
            if let Some(w) = self.windows.get_mut(&id) {
                w.place = Some(Place::Floating {
                    anchor,
                    pos: Some(pos),
                });
            }
        }
    }

    /// Where every shown window goes, bottom to top: tiled windows, then floating ones from the
    /// least recently used, then fullscreen ones.
    pub fn render(&mut self, now: Instant) -> Vec<Frame> {
        self.step_cameras(now);
        self.step_glides(now);
        self.settle_floating();
        self.sync_follow();
        self.sample_motion(now);
        self.minimap_watch(now);
        let rank = |id: Id| self.mru.iter().position(|&m| m == id).unwrap_or(usize::MAX);
        let mut frames = Vec::new();
        for w in self.windows.values() {
            let Some(o) = w.output.and_then(|o| self.output(o)) else {
                continue;
            };
            let Some(rect) = self.window_rect(w.id) else {
                continue;
            };
            let focused = self.focus == Some(w.id);
            let style = w.rules.border;
            let color = if focused {
                style.active
            } else if w.urgent {
                style.urgent
            } else {
                style.inactive
            };
            let tiled = w.is_tiled();
            let (mut rect, mut scale) = self.drawn(w, o, rect);
            // niri's open animation: from half the size about the middle, fading in.
            let open = self.open_progress(w.id);
            if open < 1. && !w.fullscreen {
                let k = (open / 2. + 0.5).max(0.);
                let shift = |v: i32| (f64::from(v) * scale * (1. - k) / 2.).round() as i32;
                rect = Rect::new(
                    rect.x + shift(rect.w),
                    rect.y + shift(rect.h),
                    rect.w,
                    rect.h,
                );
                scale *= k;
            }
            // The camera draws the window, its border and glow scaled about its top-left; the
            // clip box is in the window's own (unscaled) units.
            let b = f64::from(style.width) * scale;
            let outer = Rect::new(
                (f64::from(rect.x) - b).floor() as i32,
                (f64::from(rect.y) - b).floor() as i32,
                (f64::from(rect.w) * scale + 2. * b).ceil() as i32,
                (f64::from(rect.h) * scale + 2. * b).ceil() as i32,
            );
            let local = |v: i32| f64::from(v) / scale;
            let clip = outer.intersect(o.area).map(|_| {
                Rect::new(
                    local(o.area.x - rect.x).floor() as i32,
                    local(o.area.y - rect.y).floor() as i32,
                    local(o.area.w).ceil() as i32,
                    local(o.area.h).ceil() as i32,
                )
            });
            frames.push(Frame {
                id: w.id,
                pos: (rect.x, rect.y),
                clip,
                floating: !tiled,
                fullscreen: w.fullscreen,
                focused,
                border: (style.width, color),
                scale,
                opacity: rules::opacity(&self.rules, &self.rule_info(w, now))
                    * open.clamp(0., 1.) as f32,
            });
        }
        frames.sort_by_key(|f| {
            let layer = u8::from(f.floating) + 2 * u8::from(f.fullscreen);
            (layer, std::cmp::Reverse(rank(f.id)))
        });
        frames
    }

    // ---- actions -------------------------------------------------------------------------

    fn focused_tiled(&self) -> Option<(Id, Option<OutputId>)> {
        let w = self.windows.get(&self.focus?)?;
        (w.is_tiled() && !w.fullscreen).then_some((w.id, w.output))
    }

    /// The canvas of the focused window's output, else the active output.
    fn active_canvas_output(&self) -> Option<OutputId> {
        self.focus
            .and_then(|f| self.windows.get(&f))
            .and_then(|w| w.output)
            .or_else(|| self.active_output())
    }

    /// After a canvas operation, the keyboard focus follows the canvas's.
    fn follow_canvas(&mut self, out: Option<OutputId>, now: Instant) {
        if let Some(f) = out
            .and_then(|o| self.output(o))
            .and_then(|o| o.canvas.focus())
            && self.focus != Some(f)
        {
            self.focus_window(f, now);
        }
    }

    fn focus_dir(&mut self, d: Dir, now: Instant) {
        let out = self.active_canvas_output();
        let on_canvas = self.focused_tiled().is_some();
        if let Some(o) = self.output_mut(out)
            && (o.canvas.focus_dir(d) || !on_canvas)
        {
            self.follow_canvas(out, now);
        }
    }

    fn canvas_op(&mut self, now: Instant, name: &str, op: impl FnOnce(&mut Canvas, Id) -> bool) {
        if let Some((id, out)) = self.focused_tiled() {
            self.logged(name, out, Some(id), |c| op(c, id));
            self.follow_canvas(out, now);
        }
    }

    /// A history operation on the active output's canvas, which needs no focused window.
    fn history(&mut self, now: Instant, name: &str, op: fn(&mut Canvas) -> bool) {
        let out = self.active_canvas_output();
        self.logged(name, out, None, op);
        self.follow_canvas(out, now);
    }

    /// Undo (or, with `redo`, redo) on the active output's canvas, which needs no focused
    /// window: the whole of the last park while its steps are the last ones.
    fn undo_redo(&mut self, now: Instant, name: &str, redo: bool) {
        let out = self.active_canvas_output();
        let park = self.park_steps(out, redo);
        self.logged(name, out, None, |c| {
            (0..park.unwrap_or(1)).all(|_| if redo { c.redo() } else { c.undo() })
        });
        if park.is_some()
            && let Some((_, p)) = &mut self.park
        {
            p.stepped(redo);
        }
        self.follow_canvas(out, now);
    }

    fn zoom(&mut self, delta: i32) {
        let out = self.active_canvas_output();
        self.apply(out, |c| c.zoom_by(delta));
    }

    fn toggle_floating(&mut self, id: Id, now: Instant) {
        let Some(w) = self.windows.get(&id) else {
            return;
        };
        if w.fullscreen {
            return;
        }
        let (out, place) = (w.output, w.place);
        match place {
            Some(Place::Tiled) => {
                let pos = self.settled(|| self.window_rect(id)).map(|r| (r.x, r.y));
                if let Some(o) = self.output_mut(out) {
                    o.canvas.remove(id);
                }
                let w = self.windows.get_mut(&id).unwrap();
                w.place = Some(Place::Floating {
                    anchor: Anchor::Centre,
                    pos,
                });
                w.opening = None;
                w.watch = None;
                w.asked = w.size;
                (w.resized, w.grip) = (None, None);
                self.focus_window(id, now);
            }
            Some(Place::Floating { .. }) => {
                let w = &self.windows[&id];
                let (min_w, min_h) = self.tile_min(w);
                let size = w.size.unwrap_or((0, 0));
                let Some(o) = self.output(out.unwrap_or_default()) else {
                    return;
                };
                let (tw, th) = o.canvas.base_tile();
                let want = (
                    ((f64::from(size.0) / tw).round() as i32).max(1),
                    ((f64::from(size.1) / th).round() as i32).max(1),
                );
                if let Some(o) = self.output_mut(out) {
                    o.canvas.add(id, min_w, min_h, Some(want));
                }
                let w = self.windows.get_mut(&id).unwrap();
                w.place = Some(Place::Tiled);
                w.asked = None;
                (w.resized, w.grip) = (None, None);
                self.focus_window(id, now);
            }
            None => {}
        }
    }

    /// Fullscreen on or off; the river layer makes the requests.
    pub fn set_fullscreen(&mut self, id: Id, on: bool) -> Option<Effect> {
        let w = self.windows.get_mut(&id)?;
        if w.fullscreen == on || w.place.is_none() {
            return None;
        }
        w.fullscreen = on;
        w.asked = None;
        Some(Effect::Fullscreen(id, on))
    }

    /// Does what a key binding or IPC action asks; what needs river comes back as effects.
    pub fn act(&mut self, action: Action, now: Instant) -> Vec<Effect> {
        let focus = self.focus;
        match action {
            Action::Spawn(cmd) => return vec![Effect::Spawn(cmd)],
            Action::SpawnSh(cmd) => return vec![Effect::SpawnSh(cmd)],
            Action::Quit(true) => return vec![Effect::Quit],
            Action::ToggleKeyboardShortcutsInhibit => return vec![Effect::ToggleShortcutsInhibit],
            Action::ToggleSynthBackground => return vec![Effect::ToggleSynthBackground],
            Action::SetSynthBackground(mode) => return vec![Effect::SetSynthBackground(mode)],
            Action::Quit(false) => return vec![Effect::ConfirmQuit],
            Action::CloseWindow => return focus.map(Effect::Close).into_iter().collect(),
            Action::CloseWindowById(id) if self.windows.contains_key(&id) => {
                return vec![Effect::Close(id)];
            }
            Action::QuitApp => {
                return self
                    .app_windows(focus)
                    .into_iter()
                    .map(Effect::Close)
                    .collect()
            }
            Action::FullscreenWindow => {
                let on = focus
                    .and_then(|f| self.windows.get(&f))
                    .is_some_and(|w| !w.fullscreen);
                return focus
                    .and_then(|f| self.set_fullscreen(f, on))
                    .into_iter()
                    .collect();
            }
            Action::FullscreenWindowById(id) => {
                let on = self.windows.get(&id).is_some_and(|w| !w.fullscreen);
                return self.set_fullscreen(id, on).into_iter().collect();
            }
            Action::FocusWindow(id) if self.overview_open() => self.pick(id, now),
            Action::FocusWindow(id) => {
                self.focus_window(id, now);
            }
            Action::FocusColumnLeft => self.focus_dir(Dir::Left, now),
            Action::FocusColumnRight => self.focus_dir(Dir::Right, now),
            Action::FocusWindowUp => self.focus_dir(Dir::Up, now),
            Action::FocusWindowDown => self.focus_dir(Dir::Down, now),
            Action::MoveColumnLeft => self.canvas_op(now, "move", |c, id| c.move_by(id, Dir::Left)),
            Action::MoveColumnRight => {
                self.canvas_op(now, "move", |c, id| c.move_by(id, Dir::Right));
            }
            Action::MoveWindowUp => self.canvas_op(now, "move", |c, id| c.move_by(id, Dir::Up)),
            Action::MoveWindowDown => self.canvas_op(now, "move", |c, id| c.move_by(id, Dir::Down)),
            Action::SwapNeighbour(d) => self.canvas_op(now, "swap", |c, _| c.swap(dir(d))),
            Action::ResizeEdge(e, delta) => {
                self.canvas_op(now, "resize", |c, _| c.resize_focused(edge(e), delta));
            }
            Action::MaximizeColumn => {
                if let Some((id, _)) = self.focused_tiled() {
                    self.toggle_full_width(id, now);
                }
            }
            Action::ResetWindowHeight => {
                if let Some((id, _)) = self.focused_tiled() {
                    self.reset_window_height(id, now);
                }
            }
            Action::SendToEdge(side) => {
                let d = match side {
                    CanvasSide::Left => Dir::Left,
                    CanvasSide::Right => Dir::Right,
                };
                self.canvas_op(now, "send-to-edge", |c, _| c.send_to_edge(d));
            }
            Action::ZoomIn => self.zoom(-1),
            Action::ZoomOut => self.zoom(1),
            Action::ZoomReset => self.zoom_reset(),
            Action::CycleFloatingFollow => {
                self.set_floating_follow(self.floating_follow().next());
            }
            Action::SetFloatingFollow(mode) => self.set_floating_follow(mode),
            Action::Undo => self.undo_redo(now, "undo", false),
            Action::Redo => self.undo_redo(now, "redo", true),
            Action::GatherIslands => self.history(now, "gather", Canvas::gather_islands),
            Action::ToggleWindowFloating => {
                if let Some(f) = focus {
                    self.toggle_floating(f, now);
                }
            }
            Action::ToggleWindowFloatingById(id) => self.toggle_floating(id, now),
            Action::LaunchPlaceholder { app_id, icon } => {
                self.launch_placeholder(app_id, icon, now);
            }
            ref a if self.monitor_action(a, now).is_some() => {}
            Action::ToggleOverview => self.set_overview(!self.overview_open()),
            Action::OpenOverview => self.set_overview(true),
            Action::CloseOverview => self.set_overview(false),
            other => return vec![Effect::Unsupported(format!("{other:?}"))],
        }
        Vec::new()
    }

    /// The windows of the focused window's app: those with its PID and app ID.
    fn app_windows(&self, focus: Option<Id>) -> Vec<Id> {
        let Some(w) = focus.and_then(|f| self.windows.get(&f)) else {
            return Vec::new();
        };
        if w.pid.is_none() {
            return vec![w.id];
        }
        self.windows
            .values()
            .filter(|o| o.pid == w.pid && o.app_id == w.app_id)
            .map(|o| o.id)
            .collect()
    }

    // ---- IPC -----------------------------------------------------------------------------

    pub fn ipc_windows(&self) -> Vec<niri_ipc::Window> {
        self.windows
            .values()
            .filter(|w| w.place.is_some() || self.parked_window(w.id))
            .map(|w| self.ipc_window(w))
            .collect()
    }

    /// Whether a window waits on the canvas of the last output to leave ([`Parked`]); such a
    /// window stays listed, as niri lists windows while it has no output.
    fn parked_window(&self, id: Id) -> bool {
        self.parked
            .iter()
            .any(|p| p.places.iter().any(|&(i, _)| i == id))
    }

    pub fn ipc_window(&self, w: &Window) -> niri_ipc::Window {
        let o = w.output.and_then(|o| self.output(o));
        let rect = self.settled(|| {
            let r = self.window_rect(w.id)?;
            Some(match self.follow_map(w.id) {
                Some(m) if !w.fullscreen => m.rect(r),
                _ => r,
            })
        });
        let b = w.rules.border.width;
        let mut layout = niri_ipc::WindowLayout {
            pos_in_scrolling_layout: None,
            tile_size: rect.map_or((0., 0.), |r| {
                (f64::from(r.w + 2 * b), f64::from(r.h + 2 * b))
            }),
            window_size: w.size.unwrap_or_default(),
            tile_pos_in_workspace_view: None,
            window_offset_in_tile: (f64::from(b), f64::from(b)),
        };
        if let (Some(o), Some(rect)) = (o, rect) {
            layout.tile_pos_in_workspace_view = Some((
                f64::from(rect.x - b - o.area.x),
                f64::from(rect.y - b - o.area.y),
            ));
        }
        if w.is_tiled()
            && let Some(o) = o
            && let Some(r) = o.canvas.rect(w.id)
        {
            let min_i = o
                .canvas
                .windows()
                .iter()
                .map(|c| c.rect.i)
                .min()
                .unwrap_or(0);
            let min_j = o
                .canvas
                .windows()
                .iter()
                .map(|c| c.rect.j)
                .min()
                .unwrap_or(0);
            layout.pos_in_scrolling_layout =
                Some(((r.i - min_i + 1) as usize, (r.j - min_j + 1) as usize));
            let (_, _, cw, ch) = self.cell(&o.canvas, r);
            layout.tile_size = (cw, ch);
        }
        niri_ipc::Window {
            id: w.id,
            title: w.title.clone(),
            app_id: w.app_id.clone(),
            pid: w.pid,
            workspace_id: w.output.map(u64::from),
            is_focused: self.focus == Some(w.id),
            is_floating: w.is_floating(),
            is_urgent: w.urgent,
            layout,
            focus_timestamp: w.focus_time.map(|t| niri_ipc::Timestamp {
                secs: t.as_secs(),
                nanos: t.subsec_nanos(),
            }),
        }
    }

    /// One workspace per output, its id the output's.
    pub fn ipc_workspaces(&self) -> Vec<niri_ipc::Workspace> {
        let active = self.active_output();
        self.outputs
            .iter()
            .map(|o| niri_ipc::Workspace {
                id: u64::from(o.id),
                idx: 1,
                name: None,
                output: Some(o.name.clone()),
                is_urgent: self
                    .windows
                    .values()
                    .any(|w| w.urgent && w.output == Some(o.id)),
                is_active: true,
                is_focused: Some(o.id) == active,
                active_window_id: o.canvas.focus().filter(|f| self.windows.contains_key(f)),
            })
            .collect()
    }

    pub fn ipc_output(o: &Output) -> niri_ipc::Output {
        niri_ipc::Output {
            name: o.name.clone(),
            make: String::new(),
            model: String::new(),
            serial: None,
            physical_size: None,
            modes: Vec::new(),
            current_mode: None,
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: Some(niri_ipc::LogicalOutput {
                x: o.area.x,
                y: o.area.y,
                width: o.area.w.max(0) as u32,
                height: o.area.h.max(0) as u32,
                scale: 1.,
                transform: niri_ipc::Transform::Normal,
            }),
            max_bpc: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 1920;
    const H: i32 = 1080;

    fn settings() -> Settings {
        Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            gaps: 10.,
            default_width: Some(PresetSize::Fixed(960)),
            default_height: Some(PresetSize::Proportion(0.6667)),
            ..Settings::default()
        }
    }

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let mut d = Desktop::new(settings(), Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, W, H));
        (d, now)
    }

    /// Opens a window as river would: the window, its hints, a manage sequence, the size it
    /// was asked for committed, a render sequence.
    fn open(d: &mut Desktop, id: Id, min: (i32, i32), now: Instant) -> Frame {
        d.add_window(id);
        d.set_hints(id, min, (0, 0));
        settle(d, now);
        frame(d, id, now)
    }

    /// Manage sequences and the apps' answers until no window is asked a new size: a floating
    /// window asked 0x0 picks 400x300; a tiled one picks nothing on an axis left to it (0), so
    /// it opens at the canvas's default size (`model/open.rs` tests the app's own size).
    fn settle(d: &mut Desktop, now: Instant) {
        for _ in 0..4 {
            d.manage(now);
            let asked = d.proposals();
            if asked.is_empty() {
                break;
            }
            for (id, size) in asked {
                d.set_asked(id, size);
                let floating = d.window(id).is_some_and(Window::is_floating);
                let size = if floating && size == (0, 0) {
                    (400, 300)
                } else {
                    size
                };
                d.on_dimensions(id, size.0, size.1);
            }
        }
    }

    fn frame(d: &mut Desktop, id: Id, now: Instant) -> Frame {
        d.render(now).into_iter().find(|f| f.id == id).unwrap()
    }

    /// Lays the windows out side by side on row 0, each keeping its span, the view following
    /// the focused one. Where a new window opens follows the canvas's placement rules (beside the
    /// focus where the view scrolls least); a test about drawing should not depend on them.
    fn in_a_row(d: &mut Desktop, ids: &[Id], now: Instant) {
        let mut col = 0;
        for &id in ids {
            let span = tiles(d, id).w;
            d.apply(Some(1), |c| c.move_to(id, col, 0));
            col += span;
        }
        d.follow_canvas(Some(1), now);
        settle(d, now);
    }

    fn tiles(d: &Desktop, id: Id) -> Tiles {
        d.output(1).unwrap().canvas.rect(id).unwrap()
    }

    #[test]
    fn new_windows_open_at_the_default_size_beside_the_focus() {
        let (mut d, now) = desktop();
        let a = open(&mut d, 1, (0, 0), now);
        // 960 / 119 rounds to 8 tiles; 0.6667 of the 1070 px canvas screen is 6 tiles of 113.
        assert_eq!((tiles(&d, 1).w, tiles(&d, 1).h), (8, 6));
        assert_eq!(a.pos, (11, 11));
        assert_eq!(d.window(1).unwrap().size, Some((940, 666)));
        let b = open(&mut d, 2, (0, 0), now);
        assert_eq!(tiles(&d, 2).i, tiles(&d, 1).right());
        assert_eq!(b.pos.0, 11 + 952);
        assert_eq!(d.focus(), Some(2));
        assert!(b.focused && !frame(&mut d, 1, now).focused);
    }

    #[test]
    fn an_app_minimum_takes_more_tiles() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (1200, 0), now);
        // 1200 + border 2 + gaps 10 + 1 = 1213 px: 11 tiles of 119.
        assert_eq!(tiles(&d, 1).w, 11);
    }

    /// A client may ask any minimum (i32): the canvas caps it, so its tile areas and edges stay
    /// in i32 (overflow-checked in release builds), through opens, moves and a close.
    #[test]
    fn a_huge_minimum_is_capped() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (i32::MAX, i32::MAX), now);
        open(&mut d, 3, (i32::MAX, 1), now);
        open(&mut d, 4, (0, 0), now);
        d.act(Action::FocusColumnLeft, now);
        d.act(Action::MoveColumnLeft, now);
        settle(&mut d, now);
        d.remove_window(1, now);
        settle(&mut d, now);
        open(&mut d, 5, (0, 0), now);
        for id in [2, 3, 4, 5] {
            let t = tiles(&d, id);
            assert!(t.w <= 1024 && t.h <= 1024, "{t:?} is not capped");
        }
    }

    #[test]
    fn an_oversize_commit_becomes_the_minimum() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        let asked = d.window(1).unwrap().asked.unwrap();
        assert!(d.on_dimensions(1, asked.0 + 200, asked.1));
        d.manage(now);
        // 1140 + 2 + 10 + 1 = 1153: 10 tiles.
        assert_eq!(tiles(&d, 1).w, 10);
        let p = d.proposals();
        assert_eq!(p, vec![(1, (1178, asked.1))]);
        d.set_asked(1, p[0].1);
        assert!(!d.on_dimensions(1, 1178, asked.1));
        d.manage(now);
        assert_eq!(tiles(&d, 1).w, 10);
    }

    #[test]
    fn a_shrink_holds_neighbours_until_the_commit() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (0, 0), now);
        d.act(Action::FocusColumnLeft, now);
        assert_eq!(d.focus(), Some(1));
        let before = frame(&mut d, 2, now).pos;
        d.act(Action::ResizeEdge(CanvasEdge::Right, -2), now);
        assert_eq!(tiles(&d, 1).w, 6);
        // Window 1 still shows its old size: window 2 waits where it was.
        assert_eq!(frame(&mut d, 2, now).pos, before);
        let p = d.proposals();
        d.set_asked(1, p[0].1);
        d.on_dimensions(1, p[0].1 .0, p[0].1 .1);
        assert_eq!(frame(&mut d, 2, now).pos.0, before.0 - 2 * 119);
    }

    #[test]
    fn a_shrink_releases_neighbours_after_the_wait() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (0, 0), now);
        d.act(Action::FocusColumnLeft, now);
        let before = frame(&mut d, 2, now).pos;
        d.act(Action::ResizeEdge(CanvasEdge::Right, -2), now);
        assert!(d.deadline().is_some());
        assert!(d.tick(Instant::now() + SHRINK_WAIT));
        assert_eq!(frame(&mut d, 2, now).pos.0, before.0 - 2 * 119);
    }

    #[test]
    fn partial_tiles_are_clipped_to_their_output() {
        let (mut d, now) = desktop();
        for id in 1..=3 {
            open(&mut d, id, (0, 0), now);
        }
        in_a_row(&mut d, &[1, 2, 3], now);
        // Three 8-tile windows do not fit 1920 px: the view scrolled to show the focused one
        // whole, the first sticks out on the left.
        let f3 = frame(&mut d, 3, now);
        assert!(f3.pos.0 + 940 <= W);
        let f1 = frame(&mut d, 1, now);
        assert!(f1.pos.0 < 0);
        assert_eq!(f1.clip, Some(Rect::new(-f1.pos.0, -f1.pos.1, W, H)));
    }

    #[test]
    fn a_window_off_its_output_is_hidden() {
        let (mut d, now) = desktop();
        for id in 1..=5 {
            open(&mut d, id, (0, 0), now);
        }
        in_a_row(&mut d, &[1, 2, 3, 4, 5], now);
        assert_eq!(frame(&mut d, 1, now).clip, None);
    }

    #[test]
    fn closing_the_focused_window_focuses_the_most_recent() {
        let (mut d, now) = desktop();
        for id in 1..=3 {
            open(&mut d, id, (0, 0), now);
        }
        d.focus_window(1, now);
        d.focus_window(3, now);
        d.remove_window(3, now);
        assert_eq!(d.focus(), Some(1));
    }

    #[test]
    fn activation_follows_the_policy() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (0, 0), now);
        d.focus_window(2, now);
        // A fresh token and a taskbar's request focus; the focused window needs nothing.
        assert!(d.on_activate(1, ActivateSource::Token));
        assert!(d.on_activate(1, ActivateSource::ForeignToplevel));
        assert!(!d.window(1).unwrap().urgent);
        assert!(!d.on_activate(2, ActivateSource::Token));
        // An urgency-only token marks the window urgent instead.
        assert!(!d.on_activate(1, ActivateSource::UrgentOnly));
        assert!(d.window(1).unwrap().urgent);
        d.act(Action::FocusWindow(1), now);
        assert_eq!(d.focus(), Some(1));
        assert!(!d.window(1).unwrap().urgent, "focusing clears the urgency");
        // The window rule on-xdg-activate overrides xdg-activation, not a taskbar.
        d.focus_window(2, now);
        d.windows.get_mut(&1).unwrap().rules.on_xdg_activate = Some(OnXdgActivate::Ignore);
        assert!(!d.on_activate(1, ActivateSource::Token));
        assert!(!d.window(1).unwrap().urgent);
        assert!(d.on_activate(1, ActivateSource::ForeignToplevel));
        d.windows.get_mut(&1).unwrap().rules.on_xdg_activate = Some(OnXdgActivate::SetUrgent);
        assert!(!d.on_activate(1, ActivateSource::Token));
        assert!(d.window(1).unwrap().urgent);
        d.windows.get_mut(&1).unwrap().rules.on_xdg_activate = Some(OnXdgActivate::Focus);
        assert!(d.on_activate(1, ActivateSource::UrgentOnly));
        assert!(
            !d.on_activate(9, ActivateSource::Token),
            "an unknown window"
        );
    }

    #[test]
    fn a_bell_marks_an_unfocused_window_urgent_until_it_is_focused() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (0, 0), now);
        d.focus_window(2, now);
        assert!(!d.on_bell(2), "the focused window's bell changes nothing");
        assert!(d.on_bell(1));
        assert!(!d.on_bell(1), "already urgent");
        assert!(d.ipc_window(d.window(1).unwrap()).is_urgent);
        assert!(d.ipc_workspaces()[0].is_urgent);
        let f = frame(&mut d, 1, now);
        assert_eq!(f.border.1, d.settings.border.urgent);
        d.focus_window(1, now);
        assert!(!d.window(1).unwrap().urgent);
        assert!(!d.ipc_window(d.window(1).unwrap()).is_urgent);
    }

    #[test]
    fn a_modal_dialog_keeps_the_focus_until_it_closes() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 3, (0, 0), now);
        d.add_window(2);
        d.window_mut(2).unwrap().parent = Some(1);
        d.set_modal(2, true, now);
        settle(&mut d, now);
        assert!(frame(&mut d, 2, now).floating);
        assert_eq!(d.focus(), Some(2));
        d.focus_window(3, now);
        assert_eq!(d.focus(), Some(3));
        // Focusing the parent (a click, Alt+Tab, IPC) lands on its dialog.
        d.focus_window(1, now);
        assert_eq!(d.focus(), Some(2));
        // Closing the dialog gives the focus back to the parent, not the last used window.
        d.focus_window(3, now);
        d.focus_window(1, now);
        d.remove_window(2, now);
        assert_eq!(d.focus(), Some(1));
        d.focus_window(1, now);
        assert_eq!(d.focus(), Some(1));
    }

    #[test]
    fn a_dialog_turning_modal_takes_the_focus_from_its_parent() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        d.add_window(2);
        d.window_mut(2).unwrap().parent = Some(1);
        settle(&mut d, now);
        d.focus_window(1, now);
        assert_eq!(d.focus(), Some(1));
        assert!(d.set_modal(2, true, now));
        assert_eq!(d.focus(), Some(2));
        d.set_modal(2, false, now);
        d.focus_window(1, now);
        assert_eq!(
            d.focus(),
            Some(1),
            "a dialog no longer modal lets the parent take the focus"
        );
    }

    #[test]
    fn dialogs_float_centred_on_their_parent() {
        let (mut d, now) = desktop();
        let parent = open(&mut d, 1, (0, 0), now);
        d.add_window(2);
        d.window_mut(2).unwrap().parent = Some(1);
        settle(&mut d, now);
        let f = frame(&mut d, 2, now);
        assert!(f.floating && f.focused);
        let size = d.window(2).unwrap().size.unwrap();
        assert_eq!(size, (400, 300));
        assert_eq!(f.pos.0 + 200, parent.pos.0 + 940 / 2);
        assert_eq!(f.pos.1 + 150, parent.pos.1 + 666 / 2);
        // Floating windows draw above tiled ones.
        let order: Vec<Id> = d.render(now).iter().map(|f| f.id).collect();
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn picture_in_picture_floats_bottom_right_unfocused() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        d.add_window(2);
        d.set_hints(2, (0, 0), (1536, 864));
        settle(&mut d, now);
        let f = frame(&mut d, 2, now);
        assert!(f.floating && !f.focused);
        assert_eq!(f.pos, (W - 400 - 10, H - 300 - 10));
        assert_eq!(d.focus(), Some(1));
    }

    #[test]
    fn exclusive_zones_shrink_the_canvas() {
        let (mut d, now) = desktop();
        d.set_output_usable(1, Rect::new(0, 40, W, H - 40));
        let f = open(&mut d, 1, (0, 0), now);
        assert_eq!(f.pos, (11, 51));
    }

    #[test]
    fn floating_toggles_both_ways() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        let tiled = frame(&mut d, 1, now);
        d.act(Action::ToggleWindowFloating, now);
        let f = frame(&mut d, 1, now);
        assert!(f.floating);
        assert_eq!(f.pos, tiled.pos);
        assert!(d.output(1).unwrap().canvas.is_empty());
        d.act(Action::ToggleWindowFloating, now);
        assert_eq!((tiles(&d, 1).w, tiles(&d, 1).h), (8, 6));
    }

    #[test]
    fn quit_app_closes_the_apps_windows() {
        let (mut d, now) = desktop();
        for id in 1..=3 {
            d.add_window(id);
            let w = d.window_mut(id).unwrap();
            w.pid = Some(if id == 2 { 7 } else { 5 });
            w.app_id = Some("foot".into());
        }
        settle(&mut d, now);
        let effects = d.act(Action::QuitApp, now);
        assert_eq!(effects, vec![Effect::Close(1), Effect::Close(3)]);
    }

    #[test]
    fn toggle_keyboard_shortcuts_inhibit_goes_to_river() {
        let (mut d, now) = desktop();
        assert_eq!(
            d.act(Action::ToggleKeyboardShortcutsInhibit, now),
            vec![Effect::ToggleShortcutsInhibit]
        );
    }

    #[test]
    fn zoom_keeps_sizes_and_scales_positions() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        let before = d.proposals();
        d.act(Action::ZoomOut, now);
        assert_eq!(d.output(1).unwrap().canvas.zoom(), 1);
        assert_eq!(d.proposals(), before);
        assert!((frame(&mut d, 1, now).scale - 0.8).abs() < 1e-9);
    }

    #[test]
    fn zoom_reset_returns_to_one_to_one() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (0, 0), now);
        let zoom = |d: &Desktop| d.output(1).unwrap().canvas.zoom();
        d.act(Action::ZoomOut, now);
        d.act(Action::ZoomOut, now);
        assert_eq!(zoom(&d), 2);
        d.act(Action::ZoomReset, now);
        assert_eq!(zoom(&d), 0);
        assert!((frame(&mut d, 1, now).scale - 1.0).abs() < 1e-9);
        d.act(Action::ZoomIn, now);
        assert_eq!(zoom(&d), -1);
        d.act(Action::ZoomReset, now);
        assert_eq!(zoom(&d), 0);
        // Reset during a held zoom-to-fit: the release does not go back to the remembered zoom.
        d.act(Action::ZoomOut, now);
        d.zoom_to_fit();
        d.act(Action::ZoomReset, now);
        assert!(!d.zoom_back());
        assert_eq!(zoom(&d), 0);
    }

    #[test]
    fn clip_boxes_are_in_the_scaled_windows_own_units() {
        let (mut d, now) = desktop();
        for id in 1..=4 {
            open(&mut d, id, (0, 0), now);
        }
        in_a_row(&mut d, &[1, 2, 3, 4], now);
        d.act(Action::ZoomOut, now);
        // Window 1 left the screen; window 2 sticks out on the left.
        assert_eq!(frame(&mut d, 1, now).clip, None);
        let f = frame(&mut d, 2, now);
        assert!(f.pos.0 < 0);
        let clip = f.clip.unwrap();
        assert_eq!((clip.w, clip.h), (2400, 1350));
        assert_eq!(clip.x, (f64::from(-f.pos.0) / 0.8).floor() as i32);
    }

    #[test]
    fn kept_sizes_hold_until_the_window_commits() {
        let k = Kept::default().after(100, 140);
        assert_eq!(
            k,
            Kept {
                size: 140,
                margin: 40
            }
        );
        assert_eq!(
            k.after(200, 240),
            Kept {
                size: 140,
                margin: 40
            }
        );
        assert_eq!(
            k.after(140, 140),
            Kept {
                size: 140,
                margin: 0
            }
        );
        assert_eq!(k.after(100, 100), Kept::default());
    }

    #[test]
    fn ipc_reports_tiles() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        let w = d.ipc_windows().remove(0);
        assert_eq!(w.layout.pos_in_scrolling_layout, Some((1, 1)));
        assert_eq!(w.layout.tile_size, (942., 668.));
        assert_eq!(w.layout.tile_pos_in_workspace_view, Some((10., 10.)));
        assert!(w.is_focused && !w.is_floating);
        assert_eq!(d.ipc_workspaces()[0].active_window_id, Some(1));
    }

    /// The only output leaving (a VT switch away: wlroots destroys every output and makes new
    /// ones on the way back) parks its canvas; the next output takes it back, every window on
    /// its tiles, the camera where it was.
    #[test]
    fn windows_wait_on_their_tiles_while_no_output_is_left() {
        let (mut d, now) = desktop();
        for id in 1..=3 {
            open(&mut d, id, (0, 0), now);
        }
        in_a_row(&mut d, &[1, 2, 3], now);
        let before: Vec<Tiles> = (1..=3).map(|id| tiles(&d, id)).collect();
        let camera = d.output(1).unwrap().canvas.view();
        d.remove_output(1);
        assert!(!d.has_outputs());
        settle(&mut d, now);
        let listed = d.ipc_windows();
        assert_eq!(listed.len(), 3, "parked windows stay listed");
        assert!(listed
            .iter()
            .all(|w| w.layout.pos_in_scrolling_layout.is_none()));
        d.add_output(2);
        d.set_output_area(2, Rect::new(0, 0, W, H));
        settle(&mut d, now);
        let after: Vec<Tiles> = (1..=3)
            .map(|id| d.output(2).unwrap().canvas.rect(id).unwrap())
            .collect();
        assert_eq!(after, before);
        assert_eq!(d.output(2).unwrap().canvas.view(), camera);
        assert!((1..=3).all(|id| d.window(id).unwrap().output == Some(2)));
    }

    #[test]
    fn a_window_closed_while_no_output_is_left_leaves_the_parked_canvas() {
        let (mut d, now) = desktop();
        open(&mut d, 1, (0, 0), now);
        open(&mut d, 2, (0, 0), now);
        d.remove_output(1);
        d.remove_window(2, now);
        d.add_window(3);
        d.set_hints(3, (0, 0), (0, 0));
        settle(&mut d, now);
        d.add_output(2);
        d.set_output_area(2, Rect::new(0, 0, W, H));
        settle(&mut d, now);
        let canvas = &d.output(2).unwrap().canvas;
        assert!(canvas.rect(1).is_some());
        assert!(canvas.rect(2).is_none());
        // A window opened while no output was left opens when one comes.
        assert!(canvas.rect(3).is_some());
        assert_eq!(d.window(3).unwrap().output, Some(2));
    }

    /// The output comes back somewhere else in the layout (wlr-randr --on puts it where river's
    /// auto layout does): the windows are drawn on it, not where the old one was.
    #[test]
    fn parked_windows_are_drawn_on_the_output_that_takes_them() {
        let now = Instant::now();
        let mut d = Desktop::new(settings(), Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(3968, 0, W, H));
        open(&mut d, 1, (0, 0), now);
        assert!(frame(&mut d, 1, now).clip.is_some());
        d.remove_output(1);
        settle(&mut d, now);
        d.add_output(2);
        d.set_output_area(2, Rect::new(0, 0, W, H));
        settle(&mut d, now);
        let f = frame(&mut d, 1, now);
        assert!(f.clip.is_some(), "hidden after the output came back: {f:?}");
        assert!(
            (0..W).contains(&f.pos.0),
            "drawn at {:?}, off the output",
            f.pos
        );
    }

    /// With another output left, a leaving output's windows move there as before: nothing parks.
    #[test]
    fn a_second_output_takes_the_windows_and_nothing_parks() {
        let (mut d, now) = desktop();
        d.add_output(2);
        d.set_output_area(2, Rect::new(W, 0, W, H));
        open(&mut d, 1, (0, 0), now);
        d.remove_output(1);
        settle(&mut d, now);
        assert_eq!(d.window(1).unwrap().output, Some(2));
        d.add_output(3);
        d.set_output_area(3, Rect::new(0, 0, W, H));
        settle(&mut d, now);
        assert!(d.output(3).unwrap().canvas.rect(1).is_none());
    }

    /// A VT switch away with two monitors: both leave, both come back as new outputs whose names
    /// come a round trip later, and each takes back its own windows on their tiles.
    #[test]
    fn each_output_takes_back_its_own_windows_after_both_left() {
        let (mut d, now) = desktop();
        d.set_output_name(1, "DP-1".into());
        d.add_output(2);
        d.set_output_area(2, Rect::new(W, 0, W, H));
        d.set_output_name(2, "DP-2".into());
        open(&mut d, 1, (0, 0), now);
        d.focus_output(2, now);
        open(&mut d, 2, (0, 0), now);
        assert_eq!(d.window(2).unwrap().output, Some(2));
        let t1 = tiles(&d, 1);
        let t2 = d.output(2).unwrap().canvas.rect(2).unwrap();
        d.remove_output(1);
        d.remove_output(2);
        settle(&mut d, now);
        assert!(!d.has_outputs());
        assert_eq!(d.ipc_windows().len(), 2, "both stay listed");
        d.add_output(3);
        d.set_output_area(3, Rect::new(W, 0, W, H));
        d.add_output(4);
        d.set_output_area(4, Rect::new(0, 0, W, H));
        settle(&mut d, now);
        assert!(
            d.window(1).unwrap().place.is_none() && d.window(2).unwrap().place.is_none(),
            "nothing is placed before the outputs have names"
        );
        // The names come in the other order.
        d.set_output_name(3, "DP-2".into());
        d.set_output_name(4, "DP-1".into());
        settle(&mut d, now);
        assert_eq!(d.window(1).unwrap().output, Some(4));
        assert_eq!(d.window(2).unwrap().output, Some(3));
        assert_eq!(d.output(4).unwrap().canvas.rect(1), Some(t1));
        assert_eq!(d.output(3).unwrap().canvas.rect(2), Some(t2));
    }

    /// A monitor that leaves while another stays: its windows go to the other meanwhile, and
    /// back to their tiles on it when it returns.
    #[test]
    fn a_monitor_that_comes_back_takes_its_windows_back() {
        let (mut d, now) = desktop();
        d.set_output_name(1, "DP-1".into());
        d.add_output(2);
        d.set_output_area(2, Rect::new(W, 0, W, H));
        d.set_output_name(2, "DP-2".into());
        open(&mut d, 1, (0, 0), now);
        let t1 = tiles(&d, 1);
        d.remove_output(1);
        settle(&mut d, now);
        assert_eq!(d.window(1).unwrap().output, Some(2), "reachable meanwhile");
        d.add_output(3);
        d.set_output_area(3, Rect::new(0, 0, W, H));
        d.set_output_name(3, "DP-1".into());
        settle(&mut d, now);
        assert_eq!(d.window(1).unwrap().output, Some(3));
        assert_eq!(d.output(3).unwrap().canvas.rect(1), Some(t1));
        assert!(d.output(2).unwrap().canvas.rect(1).is_none());
    }
}
