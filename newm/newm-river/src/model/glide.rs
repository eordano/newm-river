//! Windows gliding to new tiles and opening, as niri draws them: render-only offsets on top of
//! where the canvas puts each window, never a change to the canvas. A window moved by a push, a
//! swap, a neighbour's resize, an undo or redo, a drop or a drag's preview glides from where it
//! was drawn to its new tiles; a new window opens scaled from half its size about its middle and
//! fading in (niri's `window-open`). Offsets are in canvas pixels at 1:1, so a camera moving at
//! the same time draws them at its own scale.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use newm_canvas::{Id, Rect as Tiles};

use super::{Desktop, Output, OutputId};
use crate::anim::ease_out_cubic;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Glide {
    /// Where the window starts, from where the canvas puts it.
    from: (f64, f64),
    /// Where it is drawn at the last render, from where the canvas puts it.
    shown: (f64, f64),
    start: Instant,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Glides {
    moving: HashMap<Id, Glide>,
    /// Opening windows: when they first showed, and the eased progress of the last render.
    opening: HashMap<Id, (Instant, f64)>,
}

/// The windows of one canvas before a change: the tiles each is shown on and where it is drawn,
/// in canvas pixels.
pub(super) type Snapshot = Vec<(Id, Option<Tiles>, (f64, f64))>;

/// niri's `ease-out-expo`, the curve of its `window-open` animation.
pub fn ease_out_expo(t: f64) -> f64 {
    if t >= 1. {
        1.
    } else {
        1. - 2f64.powf(-10. * t.max(0.))
    }
}

fn progress(start: Instant, now: Instant, duration: Duration) -> f64 {
    if duration.is_zero() {
        return 1.;
    }
    now.saturating_duration_since(start).as_secs_f64() / duration.as_secs_f64()
}

impl Desktop {
    /// The tiles a tiled window is drawn on: its drag preview's, else its own.
    pub(super) fn shown_tiles(&self, o: &Output, id: Id) -> Option<Tiles> {
        self.moves.preview_rect(id).or_else(|| o.canvas.rect(id))
    }

    fn anchor(&self, o: &Output, r: Tiles) -> (f64, f64) {
        let (x, y, _, _) = self.cell(&o.canvas, r);
        (x, y)
    }

    /// How far from where the canvas puts it window `id` is drawn now, in canvas pixels; none
    /// while the cameras read settled (IPC reports where windows go).
    pub(super) fn glide_offset(&self, id: Id) -> (f64, f64) {
        if self.is_settled() {
            return (0., 0.);
        }
        self.glides.moving.get(&id).map_or((0., 0.), |g| g.shown)
    }

    /// Every window of `out`'s canvas: its shown tiles and where it is drawn.
    pub(super) fn glide_snapshot(&self, out: Option<OutputId>) -> Snapshot {
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return Vec::new();
        };
        o.canvas
            .windows()
            .iter()
            .map(|cw| {
                let shown = self.shown_tiles(o, cw.id);
                let at = shown.map_or((0., 0.), |r| self.anchor(o, r));
                let g = self.glides.moving.get(&cw.id).map_or((0., 0.), |g| g.shown);
                (cw.id, shown, (at.0 + g.0, at.1 + g.1))
            })
            .collect()
    }

    /// Every window of `snap` shown on other tiles now glides there from where it was drawn.
    /// One whose tiles did not change keeps its glide, so a drop onto the tiles its preview
    /// showed starts no second one.
    pub(super) fn glide_from(&mut self, out: Option<OutputId>, snap: Snapshot) {
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return;
        };
        let starts: Vec<(Id, (f64, f64))> = snap
            .into_iter()
            .filter_map(|(id, before, drawn)| {
                let after = self.shown_tiles(o, id)?;
                (Some(after) != before).then(|| {
                    let at = self.anchor(o, after);
                    (id, (drawn.0 - at.0, drawn.1 - at.1))
                })
            })
            .collect();
        for (id, from) in starts {
            self.start_glide(id, from);
        }
    }

    /// Window `id` glides from `from` (canvas pixels from where the canvas puts it) to its place.
    pub(super) fn start_glide(&mut self, id: Id, from: (f64, f64)) {
        if self.view.settings.movement.is_zero() || from.0.hypot(from.1) < 0.5 {
            self.glides.moving.remove(&id);
            return;
        }
        let start = Instant::now();
        self.glides.moving.insert(
            id,
            Glide {
                from,
                shown: from,
                start,
            },
        );
    }

    /// Window `id` glides home from `by` further than it is drawn now: a drag let go of.
    pub(super) fn glide_by(&mut self, id: Id, by: (f64, f64)) {
        let g = self.glides.moving.get(&id).map_or((0., 0.), |g| g.shown);
        self.start_glide(id, (g.0 + by.0, g.1 + by.1));
    }

    /// Window `id` showed for the first time: it opens.
    pub(super) fn start_open(&mut self, id: Id) {
        if !self.view.settings.open.is_zero() {
            self.glides.opening.insert(id, (Instant::now(), 0.));
        }
    }

    /// Moves every glide and opening on to `now`, ending those whose time is up.
    pub(super) fn step_glides(&mut self, now: Instant) {
        let d = self.view.settings.movement;
        self.glides.moving.retain(|_, g| {
            let t = progress(g.start, now, d);
            let k = 1. - ease_out_cubic(t);
            g.shown = (g.from.0 * k, g.from.1 * k);
            t < 1.
        });
        let d = self.view.settings.open;
        self.glides.opening.retain(|_, (start, p)| {
            let t = progress(*start, now, d);
            *p = ease_out_expo(t);
            t < 1.
        });
    }

    /// How far window `id`'s open animation is: 1 once it is open.
    pub(super) fn open_progress(&self, id: Id) -> f64 {
        if self.is_settled() {
            return 1.;
        }
        self.glides.opening.get(&id).map_or(1., |o| o.1)
    }

    /// Whether window `id` glides to new tiles now.
    pub(super) fn glides_on(&self, id: Id) -> bool {
        self.glides.moving.contains_key(&id)
    }

    /// Whether a window glides or opens: more render sequences are due.
    pub(super) fn gliding(&self) -> bool {
        !self.glides.moving.is_empty() || !self.glides.opening.is_empty()
    }

    pub(super) fn forget_glides(&mut self, id: Id) {
        self.glides.moving.remove(&id);
        self.glides.opening.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_expo_starts_fast_and_ends_on_one() {
        assert_eq!(ease_out_expo(0.), 0.);
        assert!(ease_out_expo(0.1) > 0.4);
        assert_eq!(ease_out_expo(1.), 1.);
    }
}
