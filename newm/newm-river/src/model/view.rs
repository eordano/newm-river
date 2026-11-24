//! The camera each output is drawn with, the overview and the Alt+Tab switch: the
//! desktop's side; the geometry is in [`crate::overview`] and [`crate::anim`].

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use newm_canvas::Id;

use super::{Desktop, Output, OutputId, Rect};
use crate::anim::{Camera, Follow};
use crate::overview;

#[derive(Clone, Debug, PartialEq)]
pub struct ViewSettings {
    /// How dark the overview draws what lies outside the screen area (`overview-dim`).
    pub dim: f64,
    /// The Alt+Tab switcher centres the window it confirms (`center-on-switch`).
    pub center_on_switch: bool,
    /// How long a camera move takes; zero (the default) moves at once.
    pub duration: Duration,
    /// How long a window takes to glide to new tiles (`window-movement`); zero: at once.
    pub movement: Duration,
    /// How long a new window takes to open (`window-open`); zero: at once.
    pub open: Duration,
    /// How long a closed window takes to go (`window-close`); zero: at once.
    pub close: Duration,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            dim: 0.4,
            center_on_switch: false,
            duration: Duration::ZERO,
            movement: Duration::ZERO,
            open: Duration::ZERO,
            close: Duration::ZERO,
        }
    }
}

#[derive(Debug, Default)]
pub struct View {
    pub settings: ViewSettings,
    overview: bool,
    cams: HashMap<OutputId, Follow>,
    /// While set, cameras read as their targets (see [`Desktop::settled`]).
    settled: Cell<bool>,
    /// A four-finger swipe holds the overview this far open (0 closed, 1 open).
    gesture: Option<f64>,
    /// The overview closed and the cameras still move back from it.
    closing: bool,
}

impl Desktop {
    pub fn set_view_settings(&mut self, settings: ViewSettings) {
        self.view.settings = settings;
    }

    pub fn settings_view(&self) -> &ViewSettings {
        &self.view.settings
    }

    pub fn overview_open(&self) -> bool {
        self.view.overview
    }

    pub(super) fn set_overview(&mut self, on: bool) {
        if self.view.overview && !on {
            self.view.closing = true;
        }
        self.view.overview = on;
    }

    /// A four-finger swipe takes hold of the overview where it is.
    pub fn overview_gesture_begin(&mut self) {
        let p = if self.view.overview { 1. } else { 0. };
        self.view.gesture = Some(p);
    }

    /// The fingers moved the overview `delta` further open (negative: closed).
    pub fn overview_gesture_update(&mut self, delta: f64) {
        if let Some(p) = &mut self.view.gesture {
            *p = (*p + delta).clamp(0., 1.);
        }
    }

    /// The fingers left moving the overview at `velocity` (progress per second): it opens when
    /// where they would coast to (niri's deceleration projection) is past half way.
    pub fn overview_gesture_release(&mut self, velocity: f64) {
        if let Some(p) = self.view.gesture {
            self.overview_gesture_end(p + velocity * 0.997 / 0.003 / 1000. > 0.5);
        }
    }

    /// The fingers left: the overview animates open or closed from where they held it.
    pub fn overview_gesture_end(&mut self, open: bool) {
        if let Some(p) = self.view.gesture.take() {
            if !open && p > 0. {
                self.view.closing = true;
            }
            self.view.overview = open;
        }
    }

    pub(super) fn is_settled(&self) -> bool {
        self.view.settled.get()
    }

    /// How far into the overview an output's camera is: 0 at home, 1 at the overview's camera;
    /// the dim and the floating windows follow it.
    pub fn overview_progress(&self, o: &Output) -> f64 {
        if let Some(p) = self.view.gesture {
            return p;
        }
        if !self.view.overview && !self.view.closing {
            return 0.;
        }
        let open = if self.view.overview { 1. } else { 0. };
        if self.view.settled.get() {
            return open;
        }
        let (home, ov, cam) = (Self::home(o), Self::overview_camera(o), self.camera(o));
        let span = (ov.scale / home.scale).ln();
        if span.abs() < 1e-6 {
            return open;
        }
        ((cam.scale / home.scale).ln() / span).clamp(0., 1.)
    }

    /// Where a floating window drawn at `pos` at home is drawn through the camera, and at what
    /// scale: the overview zooms it with the canvas about the area the screen shows at home.
    pub(super) fn floating_through_camera(&self, o: &Output, pos: (i32, i32)) -> ((i32, i32), f64) {
        if self.overview_progress(o) <= 0. {
            return (pos, 1.);
        }
        let (home, cam) = (Self::home(o), self.camera(o));
        let wa = o.work_area();
        let inset = self.inset();
        let origin = (f64::from(wa.x) + inset, f64::from(wa.y) + inset);
        let canvas = |p: i32, origin: f64, view: f64| (f64::from(p) - origin + view) / home.scale;
        let cx = canvas(pos.0, origin.0, home.view.0);
        let cy = canvas(pos.1, origin.1, home.view.1);
        let at = (
            (origin.0 + cx * cam.scale - cam.view.0).round() as i32,
            (origin.1 + cy * cam.scale - cam.view.1).round() as i32,
        );
        (at, cam.scale / home.scale)
    }

    /// The canvas's own camera: where the desktop is when the overview is closed.
    fn home(o: &Output) -> Camera {
        Camera {
            scale: o.canvas.scale(),
            view: o.canvas.view(),
        }
    }

    /// The camera that fits every canvas window and the area the screen shows at home.
    fn overview_camera(o: &Output) -> Camera {
        let (tw, th) = o.canvas.base_tile();
        let screen = o.canvas.screen();
        let home = Self::home(o);
        let cells = o.canvas.windows().iter().map(|w| {
            let r = w.rect;
            (
                f64::from(r.i) * tw,
                f64::from(r.j) * th,
                f64::from(r.w) * tw,
                f64::from(r.h) * th,
            )
        });
        let shown = overview::shown_area(home, screen);
        let bounds = overview::union(cells.chain([shown])).unwrap_or(shown);
        let cfg = o.canvas.config();
        let furthest = cfg.zoom_step.powi(cfg.zoom_max.max(cfg.zoom_min));
        overview::fit(bounds, screen, furthest, home.scale)
    }

    /// Moves every output's camera towards its target; a render sequence starts with it.
    pub(super) fn step_cameras(&mut self, now: Instant) {
        let duration = if self.view.gesture.is_some() {
            Duration::ZERO
        } else {
            self.view.settings.duration
        };
        // A pan draws the canvas under the pointer 1:1, as niri's gesture does.
        let panned = self.panning();
        let targets: Vec<(OutputId, Camera, (f64, f64))> = self
            .outputs
            .iter()
            .map(|o| (o.id, self.target(o), o.canvas.screen()))
            .collect();
        self.view
            .cams
            .retain(|id, _| targets.iter().any(|t| t.0 == *id));
        for (id, target, screen) in targets {
            let duration = if panned == Some(id) {
                Duration::ZERO
            } else {
                duration
            };
            self.view
                .cams
                .entry(id)
                .or_default()
                .step(target, now, screen, duration);
        }
        if self.view.closing && !self.view.cams.values().any(Follow::moving) {
            self.view.closing = false;
        }
    }

    /// Whether `out`'s camera still moves toward its target.
    pub(super) fn camera_moving(&self, out: OutputId) -> bool {
        self.view.cams.get(&out).is_some_and(Follow::moving)
    }

    /// The next move of `out`'s camera is a fling's glide: it starts at the speed it is aimed
    /// with and slows at [`super::moves::FLING_RATE`].
    pub(super) fn fling_camera(&mut self, out: OutputId) {
        if let Some(f) = self.view.cams.get_mut(&out) {
            f.fling(super::moves::FLING_RATE);
        }
    }

    /// Where an output's camera is going.
    fn target(&self, o: &Output) -> Camera {
        match self.view.gesture {
            Some(p) => Camera::between(
                Self::home(o),
                Self::overview_camera(o),
                p,
                o.canvas.screen(),
            ),
            None if self.view.overview => Self::overview_camera(o),
            None => Self::home(o),
        }
    }

    /// Runs `f` with every camera at its target: IPC reports where windows go, not where an
    /// animation has them now, so scripts need not wait for it.
    pub(super) fn settled<R>(&self, f: impl FnOnce() -> R) -> R {
        self.view.settled.set(true);
        let r = f();
        self.view.settled.set(false);
        r
    }

    /// The camera an output is drawn with now.
    pub(super) fn camera(&self, o: &Output) -> Camera {
        if self.view.settled.get() {
            return self.target(o);
        }
        self.view
            .cams
            .get(&o.id)
            .and_then(Follow::shown)
            .unwrap_or_else(|| Self::home(o))
    }

    /// Whether a camera still moves: more render sequences are due.
    pub fn animating(&self) -> bool {
        self.view.cams.values().any(Follow::moving) || self.gliding()
    }

    /// The overview's dim: how dark, and the bands of every output outside the area the desktop
    /// shows at home, where the cameras are now. Nothing while the overview is closed.
    pub fn dim(&self) -> (f64, Vec<Rect>) {
        let dim = self.view.settings.dim;
        let shown = self
            .outputs
            .iter()
            .map(|o| self.overview_progress(o))
            .fold(0., f64::max);
        if shown <= 0. || dim <= 0. {
            return (0., Vec::new());
        }
        let inset = self.inset();
        let mut bands = Vec::new();
        for o in &self.outputs {
            let wa = o.work_area();
            let origin = (f64::from(wa.x) + inset, f64::from(wa.y) + inset);
            let home = Self::home(o);
            let (x, y, w, h) = overview::shown_area(home, o.canvas.screen());
            // The gaps at the edges of the working area belong to it.
            let pad = inset / home.scale;
            let shown = (x - pad, y - pad, w + 2. * pad, h + 2. * pad);
            let display = overview::on_screen(shown, self.camera(o), origin);
            bands.extend(overview::bands(o.area, display));
        }
        (dim * shown, bands)
    }

    /// Every window once: the most recently focused first, then those never focused in the order
    /// they opened.
    pub fn mru_windows(&self) -> Vec<Id> {
        let mut seen = HashSet::new();
        let placed = |id: &Id| self.windows.get(id).is_some_and(|w| w.place.is_some());
        self.mru
            .iter()
            .copied()
            .chain(self.windows.keys().copied())
            .filter(|id| placed(id) && seen.insert(*id))
            .collect()
    }

    /// Centres the view on a canvas window; one larger than the screen shows its top-left.
    fn centre_on(&mut self, id: Id) {
        let out = self.windows.get(&id).and_then(|w| w.output);
        let Some(o) = self.output_mut(out) else {
            return;
        };
        let Some(r) = o.canvas.rect(id) else {
            return;
        };
        let (x, y, w, h) = o.canvas.canvas_px(r);
        let (sw, sh) = o.canvas.screen();
        let c = |start: f64, len: f64, screen: f64| {
            if len > screen {
                start
            } else {
                start + (len - screen) / 2.
            }
        };
        o.canvas.set_view(c(x, w, sw), c(y, h, sh));
    }

    /// The Alt+Tab switcher confirmed a window: it takes the focus and, with
    /// `center-on-switch`, the middle of the screen.
    pub fn switch_to(&mut self, id: Id, now: Instant) {
        if self.focus_window(id, now) && self.view.settings.center_on_switch {
            self.centre_on(id);
        }
    }

    /// A click in the overview picks a window: the overview closes centred on it.
    pub(super) fn pick(&mut self, id: Id, now: Instant) {
        self.set_overview(false);
        if self.focus_window(id, now) {
            self.centre_on(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use niri_config::Action;
    use niri_config::PresetSize;

    use super::super::{CanvasConfig, Desktop, Frame, Settings};
    use super::*;

    const W: i32 = 1280;
    const H: i32 = 720;

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                zoom_step: 0.8,
                zoom_max: 7,
                ..CanvasConfig::default()
            },
            gaps: 10.,
            default_width: Some(PresetSize::Fixed(960)),
            default_height: Some(PresetSize::Proportion(0.6667)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, W, H));
        (d, now)
    }

    fn open(d: &mut Desktop, id: Id, now: Instant) {
        d.add_window(id);
        d.manage(now);
        for (id, size) in d.proposals() {
            d.set_asked(id, size);
            d.on_dimensions(id, size.0, size.1);
        }
        d.render(now);
    }

    fn frames(d: &mut Desktop, now: Instant) -> Vec<Frame> {
        d.render(now)
    }

    fn on_output(f: &Frame) -> bool {
        f.clip.is_some() && f.pos.0 >= 0 && f.pos.1 >= 0 && f.pos.0 < W && f.pos.1 < H
    }

    #[test]
    fn the_overview_draws_every_window_on_the_output() {
        let (mut d, now) = desktop();
        for id in 1..=4 {
            open(&mut d, id, now);
        }
        assert!(
            frames(&mut d, now).iter().any(|f| f.clip.is_none()),
            "a window should be off screen before the overview"
        );
        d.act(Action::ToggleOverview, now);
        assert!(d.overview_open());
        let fs = frames(&mut d, now);
        assert_eq!(fs.len(), 4);
        for f in &fs {
            assert!(on_output(f), "{f:?} is not on the output");
            assert!(f.scale < 1., "{f:?} is not zoomed out");
        }
        let furthest = 0.8f64.powi(7);
        assert!(fs.iter().all(|f| f.scale >= furthest - 1e-9));
    }

    #[test]
    fn the_overview_dims_outside_the_display_area_only_while_open() {
        let (mut d, now) = desktop();
        for id in 1..=3 {
            open(&mut d, id, now);
        }
        assert_eq!(d.dim(), (0., vec![]));
        d.act(Action::ToggleOverview, now);
        frames(&mut d, now);
        let (dim, bands) = d.dim();
        assert_eq!(dim, 0.4);
        assert!(!bands.is_empty());
        let covered: i32 = bands.iter().map(|b| b.w * b.h).sum();
        assert!(covered > 0 && covered < W * H);
        // The focused window is shown at home, so it is not dimmed.
        let f = frames(&mut d, now).into_iter().find(|f| f.focused).unwrap();
        let focused = Rect::new(f.pos.0, f.pos.1, 10, 10);
        assert!(bands.iter().all(|b| b.intersect(focused).is_none()));
        d.act(Action::ToggleOverview, now);
        frames(&mut d, now);
        assert_eq!(d.dim().1, vec![]);
    }

    #[test]
    fn a_click_in_the_overview_closes_it_centred_on_the_window() {
        let (mut d, now) = desktop();
        for id in 1..=4 {
            open(&mut d, id, now);
        }
        d.act(Action::ToggleOverview, now);
        frames(&mut d, now);
        d.act(Action::FocusWindow(1), now);
        assert!(!d.overview_open());
        assert_eq!(d.focus(), Some(1));
        let f = frames(&mut d, now).into_iter().find(|f| f.id == 1).unwrap();
        assert_eq!(f.scale, 1.);
        let (w, _) = d.window(1).unwrap().size.unwrap();
        let mid = f.pos.0 + w / 2;
        assert!(
            (mid - W / 2).abs() <= 6,
            "window 1 spans {}..{}",
            f.pos.0,
            f.pos.0 + w
        );
    }

    #[test]
    fn camera_moves_animate_about_the_middle_of_the_screen() {
        let (mut d, now) = desktop();
        d.set_view_settings(ViewSettings {
            duration: Duration::from_millis(200),
            ..ViewSettings::default()
        });
        open(&mut d, 1, now);
        let before = frames(&mut d, now)[0];
        d.act(Action::ZoomOut, now);
        assert_eq!(frames(&mut d, now)[0].pos, before.pos);
        assert!(d.animating());
        let mid = frames(&mut d, now + Duration::from_millis(100))[0];
        assert!(mid.scale < 1. && mid.scale > 0.8, "{mid:?}");
        let ipc = d.ipc_window(d.window(1).unwrap());
        let end_pos = d.settled(|| d.window_rect(1)).unwrap();
        assert_eq!(
            ipc.layout
                .tile_pos_in_workspace_view
                .map(|p| p.0 as i32 + 1),
            Some(end_pos.x)
        );
        let end = frames(&mut d, now + Duration::from_millis(200))[0];
        assert!((end.scale - 0.8).abs() < 1e-9);
        assert!(!d.animating());
    }

    #[test]
    fn the_switcher_lists_every_window_once_most_recent_first() {
        let (mut d, now) = desktop();
        for id in 1..=4 {
            open(&mut d, id, now);
        }
        d.focus_window(2, now);
        d.focus_window(4, now);
        d.focus_window(2, now);
        assert_eq!(d.mru_windows(), vec![2, 4, 3, 1]);
    }

    #[test]
    fn center_on_switch_centres_the_confirmed_window() {
        let (mut d, now) = desktop();
        d.set_view_settings(ViewSettings {
            center_on_switch: true,
            ..ViewSettings::default()
        });
        for id in 1..=3 {
            open(&mut d, id, now);
        }
        d.switch_to(1, now);
        assert_eq!(d.focus(), Some(1));
        let f = frames(&mut d, now).into_iter().find(|f| f.id == 1).unwrap();
        let (w, _) = d.window(1).unwrap().size.unwrap();
        assert!((f.pos.0 + w / 2 - W / 2).abs() <= 6, "{f:?}");
    }
}
