//! What the pointer and touchpad do to the model: the window under a point, a Super
//! drag that keeps the window's span, the Super pan, the
//! zoom to fit a four-finger move pulls the camera back to, and the swipe steps. The decisions of
//! when to do them are `crate::pointer`'s.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use newm_canvas::{Canvas, Dir, Id, Rect as Tiles};
use niri_config::Action;
use serde_json::json;

use super::motion::{self, Cause};
use super::{Desktop, OutputId, Place};
use crate::pointer::SwipeAction;

/// The touchpad motion a pan takes to scroll across the working area (niri's
/// `VIEW_GESTURE_WORKING_AREA_MOVEMENT`).
const WORKING_AREA_MOVEMENT: f64 = 1200.;

/// A window held by a Super drag and how far from its place it is drawn, in on-screen pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    id: Id,
    by: (f64, f64),
    /// A tiled window's tiles when the drag began, for the drop's `home_moved`.
    home: Option<Tiles>,
}

/// A dragged tiled window, its output, its tiles and the tile it is drawn nearest.
type DragTarget = (Id, Option<OutputId>, Tiles, (i32, i32));

/// Each window a preview draws elsewhere: the tiles it is drawn on and its hold, if any.
pub(super) type Previewed = HashMap<Id, (Tiles, Option<(i32, i32)>)>;

/// The pointer's and the touchpad's state in the model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Moves {
    drag: Option<Drag>,
    /// Where the windows other than a dragged or resized one are drawn while the drag previews
    /// its drop or resize: that state's tiles, uncommitted.
    pub(super) preview: HashMap<Id, Tiles>,
    /// The tile the preview is for.
    preview_at: Option<(i32, i32)>,
    /// The output a Super pan moves the camera of, while it pans.
    pan: Option<OutputId>,
    /// The pan's motion over its last [`PAN_HISTORY`], for the speed the fling starts at.
    pan_motion: VecDeque<(Instant, (f64, f64))>,
    /// The output and zoom a zoom to fit pulled back from.
    zoom_back: Option<(OutputId, i32)>,
    /// The output a touchpad pinch zooms the camera of, and the zoom it began at.
    pinch: Option<(OutputId, i32)>,
    /// A Super+secondary drag resizing a tiled window (`model/resize.rs`).
    pub(super) resize: Option<super::resize::Resize>,
}

/// How much of a pan's motion its speed is measured over (niri's swipe tracker's history).
const PAN_HISTORY: Duration = Duration::from_millis(150);

/// The rate a fling slows at per millisecond (niri's touchpad deceleration): it starts at the
/// speed the pan ended with and coasts `speed / -(1000 ln rate)` further.
pub const FLING_RATE: f64 = 0.997;

/// The speed of a pan's motion that ended at `now`, on-screen px/s: its sum over the time of its
/// last [`PAN_HISTORY`], niri's swipe tracker's velocity. Motion that stopped longer ago than that
/// before the end has no speed left.
fn pan_velocity(motion: &VecDeque<(Instant, (f64, f64))>, now: Instant) -> (f64, f64) {
    let recent: Vec<&(Instant, (f64, f64))> = motion
        .iter()
        .filter(|(t, _)| now.saturating_duration_since(*t) <= PAN_HISTORY)
        .collect();
    let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
        return (0., 0.);
    };
    let dt = last.0.saturating_duration_since(first.0).as_secs_f64();
    if dt <= 0. {
        return (0., 0.);
    }
    let sum = recent
        .iter()
        .fold((0., 0.), |s, (_, by)| (s.0 + by.0, s.1 + by.1));
    (sum.0 / dt, sum.1 / dt)
}

impl Moves {
    /// Where a window is drawn: its place, or for the dragged window its place plus the drag.
    pub fn offset(&self, id: Id, pos: (i32, i32)) -> (i32, i32) {
        match self.drag {
            Some(d) if d.id == id => (pos.0 + d.by.0.round() as i32, pos.1 + d.by.1.round() as i32),
            _ => pos,
        }
    }

    /// The tiles window `id` is drawn on while a drag previews its drop.
    pub fn preview_rect(&self, id: Id) -> Option<Tiles> {
        self.preview.get(&id).copied()
    }

    pub(super) fn clear_preview(&mut self) {
        self.preview.clear();
        self.preview_at = None;
    }

    /// Window `id` closed.
    pub(super) fn forget(&mut self, id: Id) {
        if self.drag.is_some_and(|d| d.id == id) {
            self.drag = None;
            self.clear_preview();
        }
        self.forget_resize(id);
        self.preview.remove(&id);
    }
}

/// The span of tiles every window lies in.
fn bounds(c: &Canvas) -> Option<Tiles> {
    let mut rects = c.windows().iter().map(|w| w.rect);
    let r = rects.next()?;
    let (i, j, right, bottom) = rects.fold((r.i, r.j, r.right(), r.bottom()), |b, r| {
        (
            b.0.min(r.i),
            b.1.min(r.j),
            b.2.max(r.right()),
            b.3.max(r.bottom()),
        )
    });
    Some(Tiles::new(i, j, right - i, bottom - j))
}

fn step_action(action: SwipeAction, d: Dir) -> Action {
    match (action, d) {
        (SwipeAction::Move, Dir::Left) => Action::MoveColumnLeft,
        (SwipeAction::Move, Dir::Right) => Action::MoveColumnRight,
        (SwipeAction::Move, Dir::Up) => Action::MoveWindowUp,
        (SwipeAction::Move, Dir::Down) => Action::MoveWindowDown,
        (_, Dir::Left) => Action::FocusColumnLeft,
        (_, Dir::Right) => Action::FocusColumnRight,
        (_, Dir::Up) => Action::FocusWindowUp,
        (_, Dir::Down) => Action::FocusWindowDown,
    }
}

impl Desktop {
    /// The output under a point of the global space.
    pub(crate) fn output_at(&self, x: i32, y: i32) -> Option<OutputId> {
        self.outputs
            .iter()
            .find(|o| {
                let a = o.area;
                x >= a.x && y >= a.y && x < a.x + a.w && y < a.y + a.h
            })
            .map(|o| o.id)
    }

    /// Whether a point of the global space is within `d` of an edge of the output layout, where
    /// the cursor stops: a step of `d` from it in some direction leaves every output.
    pub fn near_layout_edge(&self, x: i32, y: i32, d: f64) -> bool {
        let d = d.ceil() as i32;
        [(d, 0), (-d, 0), (0, d), (0, -d)]
            .into_iter()
            .any(|(dx, dy)| self.output_at(x + dx, y + dy).is_none())
    }

    /// The window drawn under a point in the global space, topmost first: fullscreen, then
    /// floating, then tiled windows as the camera draws them, the most recently focused on top.
    pub fn window_at(&self, x: i32, y: i32) -> Option<Id> {
        let rank = |id: Id| self.mru.iter().position(|&m| m == id).unwrap_or(usize::MAX);
        let mut order: Vec<&super::Window> = self.windows.values().collect();
        order.sort_by_key(|w| {
            let layer = u8::from(w.is_floating()) + 2 * u8::from(w.fullscreen);
            (std::cmp::Reverse(layer), rank(w.id))
        });
        let (px, py) = (f64::from(x), f64::from(y));
        order.into_iter().map(|w| w.id).find(|&id| {
            let w = &self.windows[&id];
            let (Some(r), Some(o)) = (self.window_rect(id), w.output.and_then(|o| self.output(o)))
            else {
                return false;
            };
            let (r, s) = self.drawn(w, o, r);
            let b = f64::from(w.rules.border.width) * s;
            let (x0, y0) = (f64::from(r.x) - b, f64::from(r.y) - b);
            let (x1, y1) = (
                f64::from(r.x) + f64::from(r.w) * s + b,
                f64::from(r.y) + f64::from(r.h) * s + b,
            );
            let a = o.area;
            let on_output = x >= a.x && y >= a.y && x < a.x + a.w && y < a.y + a.h;
            on_output && px >= x0 && py >= y0 && px < x1 && py < y1
        })
    }

    /// A Super drag picks the window under the pointer and focuses it. Fullscreen windows stay,
    /// and so does a window under a layer-shell surface's exclusive zone (the bar).
    pub fn drag_begin(&mut self, x: i32, y: i32, now: Instant) -> bool {
        let in_work_area = self.outputs.iter().any(|o| {
            let a = o.work_area();
            x >= a.x && y >= a.y && x < a.x + a.w && y < a.y + a.h
        });
        if !in_work_area {
            return false;
        }
        let Some(id) = self.window_at(x, y) else {
            return false;
        };
        if self.windows[&id].fullscreen || !self.focus_window(id, now) {
            return false;
        }
        let home = self.windows[&id]
            .output
            .and_then(|o| self.output(o))
            .and_then(|o| o.canvas.rect(id));
        self.moves.drag = Some(Drag {
            id,
            by: (0., 0.),
            home,
        });
        true
    }

    /// The dragged window follows the pointer 1:1: drawn `by` from its place, on-screen pixels.
    /// A tiled one previews its drop: the others are drawn where it would put them.
    pub fn drag_to(&mut self, dx: f64, dy: f64) {
        if let Some(d) = &mut self.moves.drag {
            d.by = (dx, dy);
        }
        self.preview_drag();
    }

    /// The tile a dragged tiled window is drawn nearest (its own plus the drag in tiles), its
    /// output, and its own tiles.
    fn drag_target(&self) -> Option<DragTarget> {
        let Drag { id, by, .. } = self.moves.drag?;
        let w = self.windows.get(&id)?;
        if !w.is_tiled() || w.fullscreen {
            return None;
        }
        let o = self.output(w.output?)?;
        let r = o.canvas.rect(id)?;
        let (tw, th) = o.canvas.tile_size();
        let at = (
            r.i + (by.0 / tw).round() as i32,
            r.j + (by.1 / th).round() as i32,
        );
        Some((id, w.output, r, at))
    }

    /// Draws the others where dropping the dragged window on the tile it is drawn nearest
    /// would put them: each tile names one drop state, worked out on a copy of the canvas the
    /// drag started on, so dragging back puts them back.
    fn preview_drag(&mut self) {
        let Some((id, out, r, at)) = self.drag_target() else {
            return;
        };
        if self.moves.preview_at == Some(at) {
            return;
        }
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return;
        };
        let shown: HashMap<Id, Tiles> = if at == (r.i, r.j) {
            HashMap::new()
        } else {
            let mut c = o.canvas.clone();
            c.move_to(id, at.0, at.1);
            c.windows()
                .iter()
                .filter(|w| w.id != id && Some(w.rect) != o.canvas.rect(w.id))
                .map(|w| (w.id, w.rect))
                .collect()
        };
        self.moves.preview_at = Some(at);
        self.show_preview(out, shown);
    }

    /// Draws the windows of `out` at `shown` (their own tiles when absent), each gliding from
    /// where it is drawn now: a drag's or a resize drag's preview.
    pub(super) fn show_preview(&mut self, out: Option<OutputId>, shown: HashMap<Id, Tiles>) {
        let before = out.and_then(|o| Some((o, self.drawn_now(o)?)));
        self.set_preview(out, shown);
        if let Some((o, before)) = before {
            self.begin_motion(o, Cause::carry(motion::DRAG), before);
        }
    }

    /// [`Self::show_preview`] within an op, whose motion episode counts the glides.
    pub(super) fn set_preview(&mut self, out: Option<OutputId>, shown: HashMap<Id, Tiles>) {
        let snap = self.glide_snapshot(out);
        self.moves.preview = shown;
        self.glide_from(out, snap);
    }

    /// The windows a drag's preview draws elsewhere and the hold (`Desktop::held`) each had,
    /// taken before the op its release applies, for [`Self::keep_previewed`].
    pub(super) fn before_release(&self) -> Previewed {
        self.moves
            .preview
            .iter()
            .map(|(&id, &r)| (id, (r, self.held.get(&id).copied())))
            .collect()
    }

    /// After the op a drag's release applied: a window the preview drew on the tiles the op
    /// gave it stays drawn there. Its glide never started (`glide_from` saw
    /// the same tiles), but while a shrink waits `apply` held it on its old tiles (see
    /// [`Desktop::apply`]): that hold is taken back. One the op put elsewhere moves on as
    /// `apply` started it.
    pub(super) fn keep_previewed(&mut self, out: Option<OutputId>, before: Previewed) {
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return;
        };
        let kept: Vec<(Id, Option<(i32, i32)>)> = before
            .into_iter()
            .filter(|(id, (r, _))| o.canvas.rect(*id) == Some(*r))
            .map(|(id, (_, held))| (id, held))
            .collect();
        for (id, held) in kept {
            match held {
                Some(at) => self.held.insert(id, at),
                None => self.held.remove(&id),
            };
        }
    }

    /// Takes a drag's preview down, the windows gliding back to their own tiles.
    pub(super) fn end_preview(&mut self, out: Option<OutputId>) {
        let snap = self.glide_snapshot(out);
        self.moves.clear_preview();
        self.glide_from(out, snap);
    }

    /// Esc during a drag cancels it: the preview goes, the window glides home and nothing
    /// drops. Returns whether a drag ran.
    pub fn cancel_drag(&mut self) -> bool {
        let Some(Drag { id, by, .. }) = self.moves.drag else {
            return false;
        };
        let Some(w) = self.windows.get(&id) else {
            self.moves.drag = None;
            return false;
        };
        let (out, tiled) = (w.output, w.is_tiled());
        self.end_preview(out);
        self.moves.drag = None;
        let s = match out.and_then(|o| self.output(o)) {
            Some(o) if tiled => self.camera(o).scale,
            _ => 1.,
        };
        self.glide_by(id, (by.0 / s, by.1 / s));
        if tiled {
            self.logged("cancel", out, Some(id), |_| ());
        }
        true
    }

    /// Ends a drag. A tiled window goes to the tiles nearest where it is drawn through one
    /// `Canvas::move_to`, keeping its span and pushing what is in the way, takes the focus and
    /// glides there from where the pointer left it; the others its preview showed on their new
    /// tiles already start no second glide. A drop on the tiles it holds changes nothing. A
    /// floating window stays where it was dropped.
    pub fn drop_drag(&mut self, now: Instant) {
        let Some(Drag { id, by, home }) = self.moves.drag else {
            return;
        };
        let Some((out, place)) = self.windows.get(&id).map(|w| (w.output, w.place)) else {
            self.moves.drag = None;
            return;
        };
        match place {
            Some(Place::Tiled) => {
                let Some((_, _, r, (i, j))) = self.drag_target() else {
                    // It left the grid mid-drag (fullscreen): nothing drops, the preview goes.
                    self.moves.drag = None;
                    self.end_preview(out);
                    return;
                };
                if (i, j) != (r.i, r.j) {
                    let home_moved = home.is_some_and(|h| h != r);
                    let before = self.before_release();
                    self.logged_with(
                        "drop",
                        out,
                        Some(id),
                        |c| {
                            c.move_to(id, i, j);
                            c.focus_id(id)
                        },
                        |_, _| json!({ "home_moved": home_moved }),
                    );
                    self.keep_previewed(out, before);
                    self.follow_canvas(out, now);
                } else {
                    self.end_preview(out);
                }
                self.moves.drag = None;
                let s = out
                    .and_then(|o| self.output(o))
                    .map_or(1., |o| self.camera(o).scale);
                self.glide_by(id, (by.0 / s, by.1 / s));
            }
            Some(Place::Floating {
                anchor,
                pos: Some(p),
            }) => {
                self.moves.drag = None;
                let pos = (p.0 + by.0.round() as i32, p.1 + by.1.round() as i32);
                if let Some(w) = self.windows.get_mut(&id) {
                    w.place = Some(Place::Floating {
                        anchor,
                        pos: Some(pos),
                    });
                }
            }
            _ => self.moves.drag = None,
        }
    }

    /// Pans the camera of the output under `at` (the first time; the same one until the pan
    /// ends) so the canvas follows the pointer's motion `by` 1:1.
    pub fn pan(&mut self, at: (i32, i32), by: (f64, f64), now: Instant) {
        if self.moves.pan.is_none() {
            self.moves.pan = self
                .output_at(at.0, at.1)
                .or_else(|| self.active_canvas_output());
            self.moves.pan_motion.clear();
            if let Some(o) = self.moves.pan
                && let Some(before) = self.drawn_now(o)
            {
                self.begin_motion(o, Cause::new(motion::PAN), before);
            }
        }
        let motion = &mut self.moves.pan_motion;
        motion.push_back((now, by));
        while motion
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > PAN_HISTORY)
        {
            motion.pop_front();
        }
        let out = self.moves.pan;
        if let Some(o) = self.output_mut(out) {
            o.canvas.scroll_by(-by.0, -by.1);
        }
    }

    /// The output a pan moves the camera of, while it pans.
    pub(super) fn panning(&self) -> Option<OutputId> {
        self.moves.pan
    }

    /// The window a drag moves, while it does.
    pub(super) fn dragged(&self) -> Option<Id> {
        self.moves.drag.map(|d| d.id)
    }

    /// Whether a drag's or a resize drag's preview draws the other windows where the drop or
    /// the release would put them.
    pub(super) fn previewing(&self) -> bool {
        self.moves.preview_at.is_some() || self.previewing_resize()
    }

    /// Ends a pan. A fling coasts on from the pan's speed to where niri's swipe tracker projects
    /// it, and the camera glides there from where the pan left it, slowing at [`FLING_RATE`].
    /// `pan-momentum` is the share of that coast kept: 0 stops where the hand stopped. The window
    /// nearest the middle of the screen then takes the focus and is shown.
    pub fn pan_end(&mut self, now: Instant) {
        self.pan_end_keeping(self.settings.pan_momentum, now);
    }

    /// Super and three fingers pan the camera after the fingers' motion `by`, in touchpad
    /// units: [`WORKING_AREA_MOVEMENT`] of it crosses the working area, as niri's touchpad view
    /// gesture.
    pub fn swipe_pan(&mut self, at: (i32, i32), by: (f64, f64), now: Instant) {
        let out = self
            .moves
            .pan
            .or_else(|| self.output_at(at.0, at.1))
            .or_else(|| self.active_canvas_output());
        let Some(norm) = out
            .and_then(|o| self.output(o))
            .map(|o| f64::from(o.work_area().w) / WORKING_AREA_MOVEMENT)
        else {
            return;
        };
        self.pan(at, (by.0 * norm, by.1 * norm), now);
    }

    /// The fingers of a swipe pan lifted: the camera coasts on with all of the touchpad's
    /// momentum, whatever `pan-momentum` leaves a mouse pan.
    pub fn swipe_pan_end(&mut self, now: Instant) {
        self.pan_end_keeping(1., now);
    }

    /// Zoom steps of a four-finger swipe, each the `zoom-out` action (positive) or `zoom-in`.
    pub fn swipe_zoom(&mut self, steps: i32, now: Instant) {
        let action = if steps > 0 {
            Action::ZoomOut
        } else {
            Action::ZoomIn
        };
        for _ in 0..steps.unsigned_abs() {
            self.act(action.clone(), now);
        }
    }

    /// Ends a pan keeping `momentum` of its fling.
    fn pan_end_keeping(&mut self, momentum: f64, now: Instant) {
        let out = self.moves.pan.take();
        let motion = std::mem::take(&mut self.moves.pan_motion);
        if out.is_none() {
            return;
        }
        let v = pan_velocity(&motion, now);
        let coast = -momentum / (1000. * FLING_RATE.ln());
        let extra = (-v.0 * coast, -v.1 * coast);
        self.apply(out, |c| {
            c.scroll_by(extra.0, extra.1);
            c.focus_view_center()
        });
        if extra != (0., 0.)
            && let Some(o) = out
        {
            self.fling_camera(o);
        }
        self.follow_canvas(out, now);
    }

    /// Pulls the camera back the fewest zoom steps, up to `zoom-max`, that fit every window on
    /// screen, then shows them all, or the focus when they do not fit. The first call remembers
    /// the zoom for [`Self::zoom_back`]; later ones keep the canvas fitting as it grows.
    pub fn zoom_to_fit(&mut self) {
        let out = self.active_canvas_output();
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return;
        };
        let (here, b) = ((o.id, o.canvas.zoom()), bounds(&o.canvas));
        self.moves.zoom_back.get_or_insert(here);
        let Some(b) = b else {
            return;
        };
        self.apply(out, |c| {
            while !c.fits_on_screen(b.w, b.h) && c.zoom_by(1) {}
            c.ensure_visible(b);
            c.ensure_focus_visible();
        });
    }

    /// Returns the camera to the zoom [`Self::zoom_to_fit`] remembered, showing the focused
    /// window. Returns whether there was one.
    pub fn zoom_back(&mut self) -> bool {
        let Some((out, zoom)) = self.moves.zoom_back.take() else {
            return false;
        };
        self.apply(Some(out), |c| {
            c.zoom_by(zoom - c.zoom());
            c.ensure_focus_visible();
        });
        true
    }

    /// Returns the active output's camera to 1:1, about the middle of the screen as a zoom step
    /// does; a zoom [`Self::zoom_to_fit`] remembered is dropped, so a held Super's release stays
    /// at 1:1.
    pub fn zoom_reset(&mut self) {
        self.moves.zoom_back = None;
        let out = self.active_canvas_output();
        self.apply(out, |c| c.zoom_by(-c.zoom()));
    }

    /// A Super tap with `super-tap-zoom`: from 1:1 the active output's camera zooms out to the
    /// whole step nearest `scale`, about the middle of the screen as a zoom step does; from any
    /// other zoom it returns to 1:1 as [`Self::zoom_reset`] does.
    pub fn tap_zoom(&mut self, scale: f64) {
        let out = self.active_canvas_output();
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return;
        };
        if o.canvas.zoom() != 0 {
            self.zoom_reset();
            return;
        }
        self.apply(out, |c| {
            let target = pinch_zoom(0, scale, c.config());
            c.zoom_by(target - c.zoom())
        });
    }

    /// A touchpad pinch that zooms began (`crate::pointer::pinch_zooms`): it zooms the active
    /// output's camera, from that camera's zoom at the begin.
    pub fn pinch_begin(&mut self) {
        let out = self.active_canvas_output();
        self.moves.pinch = out
            .and_then(|o| self.output(o))
            .map(|o| (o.id, o.canvas.zoom()));
    }

    /// The fingers of the pinch are `scale` times as far apart as when it began: the camera moves
    /// to the zoom [`pinch_zoom`] gives, by the steps `zoom-in` and `zoom-out` take, about the
    /// middle of the screen and eased like them.
    pub fn pinch_scale(&mut self, scale: f64) {
        let Some((out, start)) = self.moves.pinch else {
            return;
        };
        self.apply(Some(out), |c| {
            let target = pinch_zoom(start, scale, c.config());
            c.zoom_by(target - c.zoom())
        });
    }

    /// The pinch ended: the camera keeps the zoom it reached.
    pub fn pinch_end(&mut self) {
        if let Some((out, start)) = self.moves.pinch.take()
            && let Some(o) = self.output(out)
        {
            tracing::debug!("pinch zoomed {start} to {}", o.canvas.zoom());
        }
    }

    /// The touchpad motion a pan takes to scroll the camera one tile along each axis, at its zoom
    /// now: a step of a swipe.
    pub fn touchpad_tile_motion(&self) -> (f64, f64) {
        let Some(o) = self.active_canvas_output().and_then(|o| self.output(o)) else {
            return (0., 0.);
        };
        let norm = f64::from(o.work_area().w) / WORKING_AREA_MOVEMENT;
        if norm <= 0. {
            return (0., 0.);
        }
        let (tw, th) = o.canvas.tile_size();
        (tw / norm, th / norm)
    }

    /// One step of a swipe, down the path its key binding takes; a move keeps the whole canvas
    /// in view.
    pub fn swipe_step(&mut self, action: SwipeAction, d: Dir, now: Instant) {
        self.act(step_action(action, d), now);
        if action == SwipeAction::Move {
            self.zoom_to_fit();
        }
    }
}

/// The zoom a pinch that began at zoom `start` reaches when the fingers are `scale` times as far
/// apart as when it began: `start` plus the whole zoom steps nearest to `scale`, within `zoom-min`
/// and `zoom-max`. The camera draws at `zoom-step` to the power of the zoom, so pinching in
/// (`scale` below 1) pulls the camera back and spreading the fingers brings it closer. A
/// `zoom-step` of 1 draws every zoom at 1:1, and the pinch keeps `start`.
pub fn pinch_zoom(start: i32, scale: f64, config: &newm_canvas::Config) -> i32 {
    let steps = (scale.ln() / config.zoom_step.ln()).round();
    if !steps.is_finite() {
        return start;
    }
    (f64::from(start) + steps).clamp(f64::from(config.zoom_min), f64::from(config.zoom_max)) as i32
}

#[cfg(test)]
mod tests {
    use newm_canvas::Config as CanvasConfig;
    use niri_config::PresetSize;

    use super::*;
    use crate::model::{Rect, Settings};

    fn desktop(n: Id) -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            gaps: 10.,
            default_width: Some(PresetSize::Fixed(960)),
            default_height: Some(PresetSize::Proportion(0.6667)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        for id in 1..=n {
            d.add_window(id);
            settle(&mut d, now);
        }
        (d, now)
    }

    /// Manage sequences and the apps' answers until no size is asked: a floating window asked
    /// 0x0 picks 400x300, a tiled one picks nothing (it opens at the canvas's default size).
    fn settle(d: &mut Desktop, now: Instant) {
        for _ in 0..4 {
            d.manage(now);
            let asked = d.proposals();
            if asked.is_empty() {
                break;
            }
            for (id, size) in asked {
                d.set_asked(id, size);
                let floating = d.window(id).is_some_and(|w| w.is_floating());
                let size = if floating && size == (0, 0) {
                    (400, 300)
                } else {
                    size
                };
                d.on_dimensions(id, size.0, size.1);
            }
        }
    }

    fn tiles(d: &Desktop, id: Id) -> Tiles {
        d.output(1).unwrap().canvas.rect(id).unwrap()
    }

    fn pos(d: &mut Desktop, id: Id, now: Instant) -> (i32, i32) {
        d.render(now).into_iter().find(|f| f.id == id).unwrap().pos
    }

    fn canvas(d: &Desktop) -> &Canvas {
        &d.output(1).unwrap().canvas
    }

    #[test]
    fn a_drag_follows_the_pointer_and_drops_on_the_nearest_tiles_keeping_its_span() {
        let (mut d, now) = desktop(2);
        let (a, b) = (tiles(&d, 1), tiles(&d, 2));
        assert_eq!((a.i, b.i, a.w, a.h), (0, 8, 8, 6));
        let start = pos(&mut d, 1, now);
        assert!(d.drag_begin(100, 100, now));
        assert_eq!(d.focus(), Some(1));
        d.drag_to(250., 120.);
        assert_eq!(pos(&mut d, 1, now), (start.0 + 250, start.1 + 120));
        assert_eq!(tiles(&d, 1), a, "the tiles wait for the drop");
        d.drop_drag(now);
        let moved = tiles(&d, 1);
        assert_eq!((moved.i, moved.j, moved.w, moved.h), (2, 1, 8, 6));
        assert!(
            !tiles(&d, 2).overlaps(&moved),
            "what was in the way was pushed"
        );
        assert_eq!(tiles(&d, 2).w, b.w);
        canvas(&d).check().unwrap();
        assert_eq!(d.focus(), Some(1));
    }

    #[test]
    fn a_drop_on_its_own_tiles_changes_nothing() {
        let (mut d, now) = desktop(2);
        let before: Vec<Tiles> = vec![tiles(&d, 1), tiles(&d, 2)];
        assert!(d.drag_begin(100, 100, now));
        d.drag_to(50., -40.);
        d.drop_drag(now);
        assert_eq!(vec![tiles(&d, 1), tiles(&d, 2)], before);
    }

    #[test]
    fn a_zoomed_drag_counts_tiles_as_the_camera_draws_them() {
        let (mut d, now) = desktop(1);
        d.act(Action::ZoomOut, now);
        let (tw, _) = canvas(&d).tile_size();
        assert!((tw - 119. * 0.8).abs() < 1e-9);
        let p = pos(&mut d, 1, now);
        assert!(d.drag_begin(p.0 + 10, p.1 + 10, now));
        d.drag_to(2. * tw, 0.);
        d.drop_drag(now);
        assert_eq!(tiles(&d, 1).i, 2);
    }

    #[test]
    fn no_window_no_drag() {
        let (mut d, now) = desktop(1);
        assert!(!d.drag_begin(1900, 1070, now));
        d.drag_to(500., 0.);
        d.drop_drag(now);
        assert_eq!(tiles(&d, 1).i, 0);
    }

    #[test]
    fn a_floating_window_is_dropped_where_it_is_drawn() {
        let (mut d, now) = desktop(1);
        d.act(Action::ToggleWindowFloating, now);
        settle(&mut d, now);
        let p = pos(&mut d, 1, now);
        assert_eq!(d.window_at(p.0 + 5, p.1 + 5), Some(1));
        assert!(d.drag_begin(p.0 + 5, p.1 + 5, now));
        d.drag_to(-30., 40.);
        d.drop_drag(now);
        assert_eq!(pos(&mut d, 1, now), (p.0 - 30, p.1 + 40));
    }

    #[test]
    fn the_pick_follows_the_camera() {
        let (mut d, now) = desktop(2);
        let (x, y) = pos(&mut d, 2, now);
        assert_eq!(d.window_at(x + 5, y + 5), Some(2));
        d.act(Action::ZoomOut, now);
        let (x1, y1) = pos(&mut d, 1, now);
        let (x2, y2) = pos(&mut d, 2, now);
        assert_eq!(d.window_at(x2 + 5, y2 + 5), Some(2));
        let w = d.window(1).unwrap().size.unwrap().0;
        assert_eq!(d.window_at(x1 + w * 8 / 10 - 5, y1 + 5), Some(1));
        assert_ne!(
            d.window_at(x1 + w - 5, y1 + 5),
            Some(1),
            "drawn at 0.8, not 1:1"
        );
    }

    #[test]
    fn a_pan_moves_the_view_one_to_one_and_settles_the_focus_in_the_middle() {
        let (mut d, now) = desktop(2);
        assert_eq!(d.focus(), Some(2));
        let view = canvas(&d).view();
        d.pan((500, 500), (600., -20.), now);
        d.pan((1100, 480), (300., 0.), now);
        assert_eq!(canvas(&d).view(), (view.0 - 900., view.1 + 20.));
        assert_eq!(d.focus(), Some(2), "the focus waits for the end");
        d.focus_dirty = false;
        d.pan_end(now);
        assert_eq!(d.focus(), Some(1));
        assert!(d.focus_dirty);
    }

    #[test]
    fn a_fling_coasts_from_the_pan_speed_and_the_camera_glides_there() {
        let (mut d, now) = desktop(2);
        d.set_view_settings(super::super::view::ViewSettings {
            duration: Duration::from_millis(200),
            ..Default::default()
        });
        let ms = Duration::from_millis;
        let x = |d: &mut Desktop, t: Duration| {
            d.render(now + t);
            d.camera(&d.outputs[0]).view.0
        };
        let start = x(&mut d, ms(0));
        for k in 1..=10 {
            d.pan((500, 500), (-10., 0.), now + ms(10 * k));
            let shown = x(&mut d, ms(10 * k));
            assert!(
                (shown - start - 10. * k as f64).abs() < 1e-6,
                "a pan is drawn 1:1"
            );
        }
        let moved = start + 100.;
        d.pan_end(now + ms(110));
        let target = canvas(&d).view().0;
        assert!(
            target > moved + 100.,
            "the fling coasts on: {moved} -> {target}"
        );
        let first = x(&mut d, ms(110));
        assert!(
            (first - moved).abs() < 1e-6,
            "no jump at the release: {first}"
        );
        let next = x(&mut d, ms(126));
        let speed = (next - first) / 0.016;
        let expected = (target - moved) * -1000. * FLING_RATE.ln();
        assert!(
            (speed - expected).abs() < expected * 0.05,
            "the glide starts at the pan's speed: {speed} vs {expected}"
        );
        assert!(x(&mut d, ms(500)) < target, "still gliding at 400 ms");
        assert!(
            (x(&mut d, ms(4000)) - target).abs() < 1e-6,
            "it ends on the target"
        );
        let (mut d, now) = desktop(2);
        for k in 1..=10 {
            d.pan((500, 500), (-10., 0.), now + ms(10 * k));
        }
        let v = canvas(&d).view();
        d.pan_end(now + ms(400));
        assert_eq!(
            v,
            canvas(&d).view(),
            "a pan that stopped before the end has no fling"
        );
    }

    #[test]
    fn pan_momentum_is_the_share_of_the_fling_kept_and_reloads() {
        let ms = Duration::from_millis;
        // How far the camera goes on after a pan of 100 px in 100 ms that ends at `t`.
        let coast = |d: &mut Desktop, t: Instant| {
            for k in 1..=10 {
                d.pan((500, 500), (-10., 0.), t + ms(10 * k));
            }
            let left = canvas(d).view().0;
            d.pan_end(t + ms(110));
            canvas(d).view().0 - left
        };
        let (mut d, now) = desktop(2);
        let full = coast(&mut d, now);
        assert!(
            full > 100.,
            "pan-momentum 1 (the default) glides on: {full}"
        );
        let mut s = d.settings.clone();
        s.pan_momentum = 0.5;
        d.reconfigure(s.clone(), Vec::new(), now);
        let half = coast(&mut d, now + ms(1000));
        assert!(
            (half - full / 2.).abs() < 1e-6,
            "0.5 keeps half of it: {half} of {full}"
        );
        s.pan_momentum = 0.;
        d.reconfigure(s, Vec::new(), now);
        assert_eq!(
            coast(&mut d, now + ms(2000)),
            0.,
            "0 stops where the hand stopped"
        );
    }

    #[test]
    fn a_swipe_pan_goes_after_the_fingers_and_keeps_all_its_momentum() {
        let ms = Duration::from_millis;
        let (mut d, now) = desktop(2);
        let mut s = d.settings.clone();
        s.pan_momentum = 0.;
        d.reconfigure(s, Vec::new(), now);
        let start = canvas(&d).view();
        for k in 1..=10 {
            d.swipe_pan((500, 500), (-10., -5.), now + ms(10 * k));
        }
        let left = canvas(&d).view();
        // 1200 of touchpad motion crosses the 1920 px working area.
        assert!(
            (left.0 - start.0 - 160.).abs() < 1e-6 && (left.1 - start.1 - 80.).abs() < 1e-6,
            "the camera goes after the fingers on both axes: {start:?} to {left:?}"
        );
        d.swipe_pan_end(now + ms(110));
        let after = canvas(&d).view().0;
        assert!(
            after - left.0 > 100.,
            "the touchpad's momentum, whatever pan-momentum leaves the mouse: {after}"
        );
    }

    #[test]
    fn the_minimap_shows_for_a_moment_after_the_focus_or_the_camera_moves() {
        let ms = Duration::from_millis;
        let (mut d, now) = desktop(2);
        d.render(now);
        assert_eq!(d.minimap_frame(), None, "the first look shows nothing");
        d.render(now + ms(10));
        assert_eq!(d.minimap_frame(), None, "nor does a still canvas");
        d.act(Action::FocusColumnLeft, now + ms(20));
        d.render(now + ms(20));
        let f = d.minimap_frame().expect("the focus moved");
        let o = d.output(1).unwrap();
        let wa = o.work_area();
        assert_eq!(
            f.rect,
            crate::model::Rect::new(wa.x + 12, wa.y + wa.h - 12 - 160, 240, 160),
            "12 px from the left and bottom edges"
        );
        assert_eq!(f.output, o.name);
        let [other, focused] = &f.plan.windows[..] else {
            panic!("both windows on the plan, the focused one last");
        };
        assert!(focused.focused && !other.focused);
        assert!(
            focused.rect.0 < other.rect.0,
            "the focus went left: {focused:?} {other:?}"
        );
        let view = f.plan.view;
        assert!(
            (view.0 + view.2 / 2. - 120.).abs() < 1e-9,
            "the screen in the middle"
        );
        assert!(f.plan.dots.is_some(), "tile corners at 1:1");
        let a = d.settings.border.active;
        assert!((f.accent[0] * f64::from(a[3]) - f64::from(a[0])).abs() < 1e-6);
        d.render(now + ms(750));
        assert!(d.minimap_frame().is_some());
        assert!(d.deadline().is_some_and(|t| t <= now + ms(770)));
        assert!(d.tick(now + ms(770)), "its time is over: a sequence is due");
        assert_eq!(d.minimap_frame(), None, "gone 750 ms after the last move");
        d.set_remote_hosts(|id| (id == 1).then(|| "Lab.Example.org".to_owned()));
        d.act(Action::FocusColumnRight, now + ms(1000));
        d.render(now + ms(1000));
        let f = d.minimap_frame().expect("the focus moved back");
        let labels: Vec<_> = f.plan.windows.iter().map(|w| w.label.as_deref()).collect();
        assert_eq!(
            labels[0],
            Some("lab"),
            "the window connected to another machine is named by it: {labels:?}"
        );
        assert!(d.tick(now + ms(1750)));
        d.pan((500, 500), (-40., 0.), now + ms(2000));
        d.render(now + ms(2000));
        assert!(d.minimap_frame().is_some(), "the camera moved");
        d.pan_end(now + ms(2000));
    }

    #[test]
    fn swipe_zoom_steps_are_the_zoom_actions() {
        let (mut d, now) = desktop(1);
        d.swipe_zoom(2, now);
        assert_eq!(canvas(&d).zoom(), 2, "up: a zoom-out a step");
        d.swipe_zoom(-3, now);
        assert_eq!(canvas(&d).zoom(), -1, "down: a zoom-in a step");
    }

    /// newm-canvas's defaults: `zoom-step 0.8`, `zoom-min -2`, `zoom-max 4`.
    fn zooms() -> CanvasConfig {
        CanvasConfig::default()
    }

    #[test]
    fn pinching_in_pulls_the_camera_back_and_spreading_brings_it_closer() {
        let c = zooms();
        assert_eq!((c.zoom_step, c.zoom_min, c.zoom_max), (0.8, -2, 4));
        assert_eq!(pinch_zoom(0, 1.0, &c), 0);
        assert_eq!(pinch_zoom(0, 0.8, &c), 1);
        assert_eq!(pinch_zoom(0, 0.6, &c), 2, "0.6 is 2.3 steps of 0.8");
        assert_eq!(pinch_zoom(0, 1.25, &c), -1);
        assert_eq!(pinch_zoom(1, 1.6, &c), -1, "1.6 is 2.1 steps closer");
    }

    #[test]
    fn a_pinch_follows_the_fingers_in_whole_steps() {
        let c = zooms();
        assert_eq!(pinch_zoom(0, 0.8f64.powf(0.49), &c), 0);
        assert_eq!(pinch_zoom(0, 0.8f64.powf(0.51), &c), 1);
        assert_eq!(pinch_zoom(0, 0.8f64.powf(-0.51), &c), -1);
    }

    #[test]
    fn a_pinch_stays_within_zoom_min_and_zoom_max() {
        let c = zooms();
        assert_eq!(pinch_zoom(0, 0.01, &c), 4);
        assert_eq!(pinch_zoom(3, 0.5, &c), 4);
        assert_eq!(pinch_zoom(0, 100.0, &c), -2);
        assert_eq!(pinch_zoom(-1, 2.0, &c), -2);
    }

    #[test]
    fn a_pinch_back_to_where_the_fingers_began_returns_to_its_zoom() {
        let c = zooms();
        for start in -2..=4 {
            for steps in -6..=6 {
                let scale = c.zoom_step.powi(steps);
                assert_eq!(pinch_zoom(start, scale, &c), (start + steps).clamp(-2, 4));
                assert_eq!(pinch_zoom(start, 1.0, &c), start);
            }
        }
    }

    #[test]
    fn a_pinch_with_zoom_step_one_keeps_the_zoom() {
        let c = CanvasConfig {
            zoom_step: 1.0,
            ..zooms()
        };
        for scale in [0.5, 1.0, 2.0] {
            assert_eq!(pinch_zoom(1, scale, &c), 1);
        }
    }

    #[test]
    fn a_pinch_zooms_the_active_camera_and_its_release_keeps_the_zoom() {
        let (mut d, now) = desktop(2);
        let sizes = d.proposals();
        d.pinch_begin();
        for scale in [0.9, 0.75, 0.6] {
            d.pinch_scale(scale);
        }
        assert_eq!(canvas(&d).zoom(), 2);
        d.pinch_end();
        settle(&mut d, now);
        assert_eq!(canvas(&d).zoom(), 2, "the release keeps the zoom");
        assert_eq!(d.proposals(), sizes, "a pinch resizes no window");
        // The next pinch starts from there; back where the fingers began is back at its start.
        d.pinch_begin();
        d.pinch_scale(1.6);
        assert_eq!(canvas(&d).zoom(), 0);
        d.pinch_scale(1.0);
        assert_eq!(canvas(&d).zoom(), 2);
        d.pinch_end();
        d.pinch_scale(0.5);
        assert_eq!(canvas(&d).zoom(), 2, "no pinch, no zoom");
    }

    #[test]
    fn zoom_to_fit_pulls_back_until_everything_shows_and_zoom_back_returns() {
        let (mut d, _) = desktop(3);
        assert_eq!(canvas(&d).zoom(), 0);
        d.zoom_to_fit();
        let c = canvas(&d);
        let b = bounds(c).unwrap();
        assert!(c.fits_on_screen(b.w, b.h) && c.is_visible(b));
        let (sw, sh) = c.screen();
        let s = c.config().zoom_step.powi(c.zoom() - 1);
        let fits = |tiles: i32, base: f64, screen: f64| f64::from(tiles) * base * s <= screen;
        assert!(c.zoom() >= 1);
        assert!(
            !(fits(b.w, 119., sw) && fits(b.h, 113., sh)),
            "the fewest steps"
        );
        d.zoom_to_fit();
        assert!(d.zoom_back());
        assert_eq!(
            canvas(&d).zoom(),
            0,
            "back to the zoom before the first fit"
        );
        assert!(!d.zoom_back());
    }

    #[test]
    fn swipe_steps_take_their_bindings_paths() {
        let (mut d, now) = desktop(2);
        d.swipe_step(SwipeAction::Focus, Dir::Left, now);
        assert_eq!(d.focus(), Some(1));
        d.act(Action::SwapNeighbour(niri_ipc::CanvasDirection::Right), now);
        assert!(tiles(&d, 1).i > tiles(&d, 2).i);
        let i = tiles(&d, 1).i;
        d.swipe_step(SwipeAction::Move, Dir::Right, now);
        assert_eq!(tiles(&d, 1).i, i + 1);
        let c = canvas(&d);
        let b = bounds(c).unwrap();
        assert!(c.fits_on_screen(b.w, b.h));
        assert!(d.zoom_back());
    }

    #[test]
    fn a_swipe_step_is_a_tile_of_pan_motion() {
        let (mut d, now) = desktop(1);
        let (sx, sy) = d.touchpad_tile_motion();
        assert!((sx - 119. * 1200. / 1920.).abs() < 1e-9);
        assert!((sy - 113. * 1200. / 1920.).abs() < 1e-9);
        d.act(Action::ZoomOut, now);
        assert!((d.touchpad_tile_motion().0 - sx * 0.8).abs() < 1e-9);
    }

    fn gliding(d: &mut Desktop) {
        d.set_view_settings(crate::model::ViewSettings {
            movement: std::time::Duration::from_millis(250),
            ..Default::default()
        });
    }

    #[test]
    fn a_drag_previews_its_drop_and_dragging_back_puts_them_back() {
        let (mut d, now) = desktop(2);
        let (a, b) = (tiles(&d, 1), tiles(&d, 2));
        let home2 = pos(&mut d, 2, now);
        assert!(d.drag_begin(100, 100, now));
        d.drag_to(250., 120.);
        assert_eq!(tiles(&d, 2), b, "the canvas waits for the drop");
        let shown = d
            .moves
            .preview_rect(2)
            .expect("e2e-2 is in the way: previewed");
        assert!(!shown.overlaps(&Tiles::new(a.i + 2, a.j + 1, a.w, a.h)));
        assert_ne!(
            pos(&mut d, 2, now),
            home2,
            "drawn where the drop would put it"
        );
        d.drag_to(10., 5.);
        assert_eq!(d.moves.preview_rect(2), None);
        assert_eq!(pos(&mut d, 2, now), home2);
        d.drag_to(250., 120.);
        d.drop_drag(now);
        assert_eq!(tiles(&d, 2), shown, "the drop commits the previewed state");
        assert_eq!(d.moves.preview_rect(2), None);
    }

    #[test]
    fn a_drop_onto_the_preview_starts_no_second_glide() {
        let (mut d, now) = desktop(2);
        gliding(&mut d);
        assert!(d.drag_begin(100, 100, now));
        d.drag_to(250., 120.);
        let later = Instant::now() + std::time::Duration::from_secs(1);
        let previewed = pos(&mut d, 2, later);
        d.drop_drag(later);
        let dropped = pos(&mut d, 1, Instant::now());
        assert_eq!(
            pos(&mut d, 2, Instant::now()),
            previewed,
            "already on its new tiles"
        );
        let target = pos(&mut d, 1, later + std::time::Duration::from_secs(1));
        assert_ne!(
            dropped, target,
            "the dropped window glides from where the pointer left it"
        );
    }

    #[test]
    fn a_drop_that_left_the_grid_takes_the_preview_down() {
        let (mut d, now) = desktop(2);
        let home2 = pos(&mut d, 2, now);
        assert!(d.drag_begin(100, 100, now));
        d.drag_to(250., 120.);
        assert!(
            d.moves.preview_rect(2).is_some(),
            "dropping 1 here would move 2"
        );
        assert!(d.set_fullscreen(1, true).is_some());
        d.drop_drag(now);
        assert!(d.moves.drag.is_none());
        assert_eq!(
            d.moves.preview_rect(2),
            None,
            "a drop that leaves the grid takes the preview down"
        );
        assert_eq!(pos(&mut d, 2, now), home2, "2 is drawn on its own tiles");
    }

    #[test]
    fn esc_cancels_a_drag_and_the_window_glides_home() {
        let (mut d, now) = desktop(2);
        gliding(&mut d);
        let (a, b) = (tiles(&d, 1), tiles(&d, 2));
        let later = Instant::now() + std::time::Duration::from_secs(1);
        let home = pos(&mut d, 1, later);
        assert!(!d.cancel_drag(), "no drag, nothing to cancel");
        assert!(d.drag_begin(100, 100, now));
        d.drag_to(250., 120.);
        assert!(d.moves.preview_rect(2).is_some());
        assert!(d.cancel_drag());
        assert!(d.moves.drag.is_none());
        assert_eq!((tiles(&d, 1), tiles(&d, 2)), (a, b), "nothing dropped");
        assert_eq!(d.moves.preview_rect(2), None);
        let now = Instant::now();
        let drawn = pos(&mut d, 1, now);
        assert!(
            drawn.0 > home.0 + 200,
            "still drawn near the pointer: {drawn:?}"
        );
        assert_eq!(
            pos(&mut d, 1, later + std::time::Duration::from_secs(1)),
            home
        );
        d.drop_drag(now);
        assert_eq!((tiles(&d, 1), tiles(&d, 2)), (a, b));
    }

    #[test]
    fn a_push_or_undo_glides_the_windows_it_moves() {
        let (mut d, now) = desktop(2);
        gliding(&mut d);
        let end = Instant::now() + std::time::Duration::from_secs(2);
        let before = pos(&mut d, 2, end);
        d.focus_window(1, now);
        d.act(Action::MoveColumnRight, now);
        let start = pos(&mut d, 2, Instant::now());
        assert!(d.animating());
        let after = pos(&mut d, 2, end);
        assert_ne!(before, after, "the move pushed e2e-2");
        assert!(
            (start.0 - before.0).abs() <= 2,
            "it starts where it was: {start:?} {before:?}"
        );
        d.act(Action::Undo, now);
        assert_eq!(
            pos(&mut d, 2, end + std::time::Duration::from_secs(1)),
            before
        );
    }

    /// The gather-islands action (Super+I) brings two islands together, no window changing its
    /// size, and undo puts them back apart.
    #[test]
    fn gather_islands_makes_one_island_and_undo_takes_it_back() {
        let (mut d, now) = desktop(2);
        let islands = |d: &Desktop| {
            newm_canvas::place::islands(canvas(d).windows(), &[])
                .values()
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        let a = tiles(&d, 1);
        let out = d.output_mut(Some(1)).unwrap();
        assert!(out.canvas.move_to(2, a.i + a.w + 3, a.j));
        let apart = (tiles(&d, 1), tiles(&d, 2));
        assert_eq!(islands(&d), 2, "{apart:?}");
        d.act(Action::GatherIslands, now);
        let gathered = (tiles(&d, 1), tiles(&d, 2));
        assert_eq!(islands(&d), 1, "{gathered:?}");
        let spans = |(a, b): (Tiles, Tiles)| (a.w, a.h, b.w, b.h);
        assert_eq!(spans(gathered), spans(apart), "no window changes its size");
        d.act(Action::Undo, now);
        assert_eq!((tiles(&d, 1), tiles(&d, 2)), apart);
    }

    #[test]
    fn super_left_over_the_bar_picks_nothing() {
        let (mut d, now) = desktop(1);
        d.set_output_usable(1, Rect::new(0, 40, 1920, 1040));
        settle(&mut d, now);
        assert!(!d.drag_begin(100, 20, now), "the bar's exclusive zone");
        assert!(d.drag_begin(100, 200, now));
    }
}
