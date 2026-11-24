//! A resize drag and its preview. Super and the
//! secondary button on a window drag the edges nearest the pointer: in its outer thirds along
//! each axis (niri's `resize_edges_under`), none from its middle.
//!
//! On a tiled window, while the drag runs, a box in the border's active colour shows where the
//! window will be when the button is let go, and the windows the resize pushes are drawn where it
//! pushes them. Each dragged edge moves by the whole tiles the pointer travelled, so the preview
//! changes once per tile, worked out on a copy of the canvas the drag started on: dragging back
//! puts the others back. The window keeps its size until the release, which applies the previewed
//! resize without a second glide. Esc cancels the drag (logged as `cancel`) and the window keeps
//! its size. A second press within [`DOUBLE_CLICK_TIME`] is a double click instead (niri's
//! double resize click): it drags nothing, and along an axis where both presses took the same
//! edge it toggles full width or resets the height.
//!
//! A floating window resizes freely, as niri's floating `interactive_resize_*`: by the pointer's
//! motion in its own logical pixels, within its app's minimum and maximum size, asked of the app
//! as the drag moves. Dragging its left or top edge moves it by the change each commit makes, so
//! the opposite edge stays. The release keeps the size; Esc puts its size and place back.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use newm_canvas::{Canvas, Edge, Id, Rect as Tiles};

use super::{Desktop, Output, OutputId, Place, Rect};

/// Most tiles one drag moves an edge by, so spans stay far from integer overflow.
const MAX_SPAN: i32 = 1024;

/// Two Super+right presses on a window this close are a double click (niri's
/// `DOUBLE_CLICK_TIME`).
pub const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);

/// The largest size a floating resize asks for (niri's `MAX_PX`).
const MAX_PX: i32 = 100_000;

/// How long after a floating resize drag ends the app's commits still move the window by a left
/// or top edge's change, when none of them is the size last asked.
const GRIP_WAIT: Duration = Duration::from_secs(1);

/// The edge a drag moves along each axis: left or right, top or bottom.
pub type Edges = (Option<Edge>, Option<Edge>);

/// A floating window when its resize drag began.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Free {
    size: (i32, i32),
    pos: (i32, i32),
    /// The scale it is drawn at: the drag's on-screen motion over it is its logical size.
    scale: f64,
}

/// A floating window's left or top edge under a resize drag (niri's `interactive_resize_data`):
/// each commit moves it by the change in its width or height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grip {
    left: bool,
    top: bool,
    /// When the drag ended: the commit of the size last asked, or the first after
    /// [`GRIP_WAIT`], lets go.
    released: Option<Instant>,
}

/// A resize drag of a tiled window.
#[derive(Clone, Debug, PartialEq)]
pub struct Resize {
    id: Id,
    /// The edge it drags along each axis: left or right, top or bottom.
    edges: Edges,
    /// How far the pointer moved since the drag began, in on-screen pixels.
    by: (f64, f64),
    /// The edge moves the preview is for.
    at: Option<Vec<(Edge, i32)>>,
    /// The window's tiles after the resize, while they differ from its own: the box.
    target: Option<Tiles>,
    /// A floating window's free resize.
    free: Option<Free>,
}

/// The resize box in a render sequence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeBoxFrame {
    /// The box's cell on screen, through the camera.
    pub rect: Rect,
    /// Its output's area: the box is drawn clipped to it.
    pub area: Rect,
    /// The camera scale: the outline and the corners scale with it.
    pub scale: f64,
    /// The border's active colour, premultiplied.
    pub color: [f32; 4],
}

/// A floating window's length along an axis, within its app's minimum and maximum (0: none),
/// as niri's `ensure_min_max_size`.
fn clamp_len(len: i32, min: i32, max: i32) -> i32 {
    let mut len = len.clamp(1, MAX_PX);
    if max > 0 {
        len = len.min(max);
    }
    if min > 0 {
        len = len.max(min);
    }
    len
}

/// The tiles the screen holds along an axis at 1:1.
fn full_span(c: &Canvas, horizontal: bool) -> i32 {
    let (tw, th) = c.base_tile();
    let (sw, sh) = c.screen();
    let n = if horizontal { sw / tw } else { sh / th };
    (n + 1e-6).floor().max(1.) as i32
}

/// The edge a drag from `at` moves along an axis of a window drawn from `start` over `len`: the
/// near one in the outer thirds.
fn edge_at(at: f64, start: f64, len: f64, near: Edge, far: Edge) -> Option<Edge> {
    let f = (at - start) / len;
    if f < 1. / 3. {
        Some(near)
    } else if f > 2. / 3. {
        Some(far)
    } else {
        None
    }
}

impl Desktop {
    /// Super and the secondary button pick the window under the pointer, the edges nearest the
    /// pointer and focus the window. False with no such window, a fullscreen one, one under the
    /// bar, the pointer in its middle third both ways, or a double click on a tiled window
    /// (see [`Self::double_resize_click`]).
    pub fn resize_begin(&mut self, x: i32, y: i32, now: Instant) -> bool {
        let in_work_area = self.outputs.iter().any(|o| {
            let a = o.work_area();
            x >= a.x && y >= a.y && x < a.x + a.w && y < a.y + a.h
        });
        if self.moves.resize.is_some() || !in_work_area {
            return false;
        }
        let Some(id) = self.window_at(x, y) else {
            return false;
        };
        let w = &self.windows[&id];
        if w.fullscreen || w.place.is_none() {
            return false;
        }
        let (Some(rect), Some(o)) = (self.window_rect(id), w.output.and_then(|o| self.output(o)))
        else {
            return false;
        };
        let (r, s) = self.drawn(w, o, rect);
        let edges = (
            edge_at(
                f64::from(x),
                f64::from(r.x),
                f64::from(r.w) * s,
                Edge::Left,
                Edge::Right,
            ),
            edge_at(
                f64::from(y),
                f64::from(r.y),
                f64::from(r.h) * s,
                Edge::Top,
                Edge::Bottom,
            ),
        );
        if edges == (None, None) {
            return false;
        }
        let free = match (w.place, w.size) {
            (Some(Place::Floating { pos: Some(pos), .. }), Some(size)) => Some(Free {
                size,
                pos,
                scale: s,
            }),
            (Some(Place::Floating { .. }), _) => return false,
            _ => None,
        };
        // Floating windows have no double click: every press resizes.
        let last = self.windows.get_mut(&id).and_then(|w| {
            let last = w.resize_press.take();
            if free.is_none() {
                w.resize_press = Some((now, edges));
            }
            last
        });
        if let Some((at, before)) = last
            && free.is_none()
            && now.saturating_duration_since(at) <= DOUBLE_CLICK_TIME
        {
            // A third press resizes again.
            if let Some(w) = self.windows.get_mut(&id) {
                w.resize_press = None;
            }
            self.double_resize_click(id, edges, before, now);
            return false;
        }
        if !self.focus_window(id, now) {
            return false;
        }
        if free.is_some()
            && let Some(w) = self.windows.get_mut(&id)
        {
            w.grip = Some(Grip {
                left: edges.0 == Some(Edge::Left),
                top: edges.1 == Some(Edge::Top),
                released: None,
            });
        }
        self.moves.resize = Some(Resize {
            id,
            edges,
            by: (0., 0.),
            at: None,
            target: None,
            free,
        });
        true
    }

    /// A second Super+right press on a tiled window within [`DOUBLE_CLICK_TIME`] of the first
    /// (niri's double resize click): where both presses took the same left or right edge it
    /// toggles full width, the same top or bottom edge resets the height. It focuses the window
    /// when it does either.
    fn double_resize_click(&mut self, id: Id, edges: Edges, before: Edges, now: Instant) {
        let across = edges.0.is_some() && edges.0 == before.0;
        let down = edges.1.is_some() && edges.1 == before.1;
        if !(across || down) || !self.focus_window(id, now) {
            return;
        }
        if across {
            self.toggle_full_width(id, now);
        }
        if down {
            self.reset_window_height(id, now);
        }
    }

    /// niri's `maximize-column` on the canvas: a tiled window
    /// narrower than the tiles the screen holds across at 1:1 spans them all, one that spans them
    /// already goes back to the fewest it needs. The right edge moves.
    pub(super) fn toggle_full_width(&mut self, id: Id, now: Instant) {
        let Some((c, r)) = self.tiled_on(id) else {
            return;
        };
        let full = full_span(c, true);
        let target = if r.w < full {
            full
        } else {
            self.min_span(id).0
        };
        self.set_span(id, Edge::Right, target, now);
    }

    /// niri's `reset-window-height` on the canvas: a tiled window goes back to the fewest rows it
    /// needs, moving its bottom edge.
    pub(super) fn reset_window_height(&mut self, id: Id, now: Instant) {
        let target = self.min_span(id).1;
        self.set_span(id, Edge::Bottom, target, now);
    }

    /// A tiled window's canvas and tiles.
    fn tiled_on(&self, id: Id) -> Option<(&Canvas, Tiles)> {
        let w = self.windows.get(&id)?;
        if !w.is_tiled() || w.fullscreen {
            return None;
        }
        let c = &self.output(w.output?)?.canvas;
        Some((c, c.rect(id)?))
    }

    /// The fewest tiles a tiled window needs.
    fn min_span(&self, id: Id) -> (i32, i32) {
        self.tiled_on(id)
            .and_then(|(c, _)| c.window(id).map(|cw| c.need(cw.min_w, cw.min_h)))
            .unwrap_or((1, 1))
    }

    /// Resizes a tiled window to `target` tiles along `edge`'s axis by moving `edge`, logged as
    /// `resize`; the camera follows.
    fn set_span(&mut self, id: Id, edge: Edge, target: i32, now: Instant) {
        let Some((_, r)) = self.tiled_on(id) else {
            return;
        };
        let current = match edge {
            Edge::Left | Edge::Right => r.w,
            Edge::Top | Edge::Bottom => r.h,
        };
        let target = target.clamp(1, MAX_SPAN);
        if target == current {
            return;
        }
        let out = self.windows.get(&id).and_then(|w| w.output);
        self.logged("resize", out, Some(id), |c| {
            c.resize(id, edge, target - current)
        });
        self.follow_canvas(out, now);
    }

    /// The pointer moved `dx`, `dy` on-screen pixels since the resize drag began: the preview
    /// follows, once per whole tile.
    pub fn resize_to(&mut self, dx: f64, dy: f64) {
        let Some(rs) = &mut self.moves.resize else {
            return;
        };
        rs.by = (dx, dy);
        if rs.free.is_some() {
            self.resize_free();
        } else {
            self.preview_resize();
        }
    }

    /// A floating window's size for the drag so far, asked of its app at once: each dragged edge
    /// moves by the pointer's motion over the scale the window is drawn at, within the app's
    /// minimum and maximum (niri's floating `interactive_resize_update`).
    fn resize_free(&mut self) {
        let Some(rs) = &self.moves.resize else {
            return;
        };
        let (Some(f), Some(w)) = (rs.free, self.windows.get_mut(&rs.id)) else {
            return;
        };
        let len = |edge: Option<Edge>, by: f64, len: i32, min: i32, max: i32| match edge {
            None => len,
            Some(e) => {
                let by = if matches!(e, Edge::Left | Edge::Top) {
                    -by
                } else {
                    by
                };
                clamp_len((f64::from(len) + by / f.scale).round() as i32, min, max)
            }
        };
        w.resized = Some((
            len(rs.edges.0, rs.by.0, f.size.0, w.min.0, w.max.0),
            len(rs.edges.1, rs.by.1, f.size.1, w.min.1, w.max.1),
        ));
    }

    /// Window `id` committed a size after `prev`: under a resize drag of its left or top edge a
    /// floating window moves by the change, so the opposite edge stays (niri's floating
    /// `update_window`). The grip lets go once the drag is over and the app committed the size
    /// last asked, or [`GRIP_WAIT`] after the release.
    pub(super) fn grip_commit(&mut self, id: Id, prev: Option<(i32, i32)>, now: Instant) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let (Some(g), Some(size)) = (w.grip, w.size) else {
            return;
        };
        if g.released
            .is_some_and(|t| now.saturating_duration_since(t) >= GRIP_WAIT)
        {
            w.grip = None;
            return;
        }
        if let (
            Some(Place::Floating {
                anchor,
                pos: Some(p),
            }),
            Some(prev),
        ) = (w.place, prev)
        {
            let dx = if g.left { prev.0 - size.0 } else { 0 };
            let dy = if g.top { prev.1 - size.1 } else { 0 };
            w.place = Some(Place::Floating {
                anchor,
                pos: Some((p.0 + dx, p.1 + dy)),
            });
        }
        if g.released.is_some() && w.asked == Some(size) && w.asked == w.resized {
            w.grip = None;
        }
    }

    fn resize_output(&self) -> Option<OutputId> {
        let rs = self.moves.resize.as_ref()?;
        self.windows.get(&rs.id)?.output
    }

    /// The whole tiles each dragged edge has moved, outward positive: what the release applies.
    fn resize_moves(&self) -> Vec<(Edge, i32)> {
        let (Some(rs), Some(o)) = (
            self.moves.resize.as_ref(),
            self.resize_output().and_then(|o| self.output(o)),
        ) else {
            return Vec::new();
        };
        let (tw, th) = o.canvas.tile_size();
        let tiles = |px: f64, tile: f64| ((px / tile).round() as i32).clamp(-MAX_SPAN, MAX_SPAN);
        let (dx, dy) = (tiles(rs.by.0, tw), tiles(rs.by.1, th));
        let mut moves = Vec::new();
        match rs.edges.0 {
            Some(Edge::Left) => moves.push((Edge::Left, -dx)),
            Some(e) => moves.push((e, dx)),
            None => {}
        }
        match rs.edges.1 {
            Some(Edge::Top) => moves.push((Edge::Top, -dy)),
            Some(e) => moves.push((e, dy)),
            None => {}
        }
        moves.retain(|&(_, delta)| delta != 0);
        moves
    }

    /// Shows what letting go now would do: the box where the window goes, the others where the
    /// resize pushes them. Worked out again only when the drag crosses into another tile.
    fn preview_resize(&mut self) {
        if let Some((out, shown)) = self.resize_preview_state() {
            self.show_preview(out, shown);
        }
    }

    /// The canvas changed under a tiled window's resize drag (`apply`: an open, a zoom, an app
    /// growing, an op): what letting go does now is previewed again, so the box and the pushed
    /// windows show what the release does on the changed canvas. The op's
    /// motion episode counts the glides.
    pub(super) fn repreview_resize(&mut self) {
        let Some(rs) = self.moves.resize.as_mut().filter(|rs| rs.free.is_none()) else {
            return;
        };
        rs.at = None;
        rs.target = None;
        if let Some((out, shown)) = self.resize_preview_state() {
            self.set_preview(out, shown);
        }
    }

    /// For [`Self::preview_resize`]: the box's tiles (kept in the drag) and the others' tiles
    /// after letting go now, on the resized window's output; none when the tiles the drag
    /// moves its edges by are the ones previewed last.
    fn resize_preview_state(&mut self) -> Option<(Option<OutputId>, HashMap<Id, Tiles>)> {
        let moves = self.resize_moves();
        let out = self.resize_output();
        let (rs, o) = (self.moves.resize.as_ref()?, self.output(out?)?);
        let id = rs.id;
        if rs.at.as_ref() == Some(&moves) {
            return None;
        }
        let own = o.canvas.rect(id)?;
        let mut c = o.canvas.clone();
        for &(edge, delta) in &moves {
            c.resize(id, edge, delta);
        }
        let target = c.rect(id).unwrap_or(own);
        let shown: HashMap<Id, Tiles> = if target == own {
            HashMap::new()
        } else {
            c.windows()
                .iter()
                .filter(|w| w.id != id && Some(w.rect) != o.canvas.rect(w.id))
                .map(|w| (w.id, w.rect))
                .collect()
        };
        if let Some(rs) = &mut self.moves.resize {
            rs.target = (target != own).then_some(target);
            rs.at = Some(moves);
        }
        Some((out, shown))
    }

    /// Ends a resize drag: each dragged edge moves by the whole tiles the pointer travelled, as
    /// its preview showed. The windows the preview drew on their new tiles stay drawn there:
    /// no second glide, and no hold back on their old tiles while the window's shrink waits.
    pub fn resize_end(&mut self, now: Instant) {
        if let Some(rs) = self.moves.resize.take_if(|rs| rs.free.is_some()) {
            if let Some(w) = self.windows.get_mut(&rs.id)
                && let Some(g) = &mut w.grip
            {
                g.released = Some(now);
                if w.size.is_some() && w.size == w.asked && w.asked == w.resized {
                    w.grip = None;
                }
            }
            return;
        }
        let moves = self.resize_moves();
        let out = self.resize_output();
        let Some(rs) = self.moves.resize.take() else {
            return;
        };
        let before = self.before_release();
        for (edge, delta) in moves {
            self.logged("resize", out, Some(rs.id), |c| c.resize(rs.id, edge, delta));
        }
        self.keep_previewed(out, before);
        self.end_preview(out);
        self.follow_canvas(out, now);
    }

    /// Esc during a resize drag cancels it: the box goes, the others glide back and the window
    /// keeps its size; a floating window goes back to its size and place. Returns whether a
    /// resize drag ran.
    pub fn cancel_resize(&mut self) -> bool {
        let out = self.resize_output();
        let Some(rs) = self.moves.resize.take() else {
            return false;
        };
        if let Some(f) = rs.free {
            if let Some(w) = self.windows.get_mut(&rs.id) {
                w.grip = None;
                w.resized = Some(f.size);
                if let Some(Place::Floating { anchor, .. }) = w.place {
                    w.place = Some(Place::Floating {
                        anchor,
                        pos: Some(f.pos),
                    });
                }
            }
            return true;
        }
        self.end_preview(out);
        self.logged("cancel", out, Some(rs.id), |_| ());
        true
    }

    /// Whether a resize drag's preview is up.
    pub(super) fn previewing_resize(&self) -> bool {
        self.moves.resize.as_ref().is_some_and(|rs| rs.at.is_some())
    }

    /// The resize box in a render sequence: where the window goes, through the camera, while
    /// the drag would change its tiles.
    pub fn resize_box_frame(&self) -> Option<ResizeBoxFrame> {
        let target = self.moves.resize.as_ref()?.target?;
        let o = self.output(self.resize_output()?)?;
        let (rect, scale) = self.cell_on_screen(o, target);
        Some(ResizeBoxFrame {
            rect,
            area: o.area,
            scale,
            color: self.settings.border.active,
        })
    }

    /// Tiles `r` of `o`'s canvas on screen, through its camera, and the camera's scale.
    pub(super) fn cell_on_screen(&self, o: &Output, r: Tiles) -> (Rect, f64) {
        let inset = self.inset();
        let (cx, cy, cw, ch) = self.cell(&o.canvas, r);
        let wa = o.work_area();
        let cam = self.camera(o);
        let (s, (vx, vy)) = (cam.scale, cam.view);
        let x = f64::from(wa.x) + inset + cx * s - vx;
        let y = f64::from(wa.y) + inset + cy * s - vy;
        let rect = Rect::new(
            x.round() as i32,
            y.round() as i32,
            (cw * s).round().max(1.) as i32,
            (ch * s).round().max(1.) as i32,
        );
        (rect, s)
    }
}

impl super::moves::Moves {
    /// Window `id` closed: a resize drag of it ends.
    pub(super) fn forget_resize(&mut self, id: Id) {
        if self.resize.as_ref().is_some_and(|rs| rs.id == id) {
            self.resize = None;
            self.clear_preview();
        }
    }
}

#[cfg(test)]
impl Desktop {
    /// The box's tiles and the tiles the others are previewed on, sorted.
    pub fn resize_preview(&self) -> (Option<Tiles>, Vec<Tiles>) {
        let mut others: Vec<Tiles> = self.moves.preview.values().copied().collect();
        others.sort_by_key(|r| (r.j, r.i));
        (self.moves.resize.as_ref().and_then(|rs| rs.target), others)
    }
}

#[cfg(test)]
mod tests {
    use newm_canvas::Config as CanvasConfig;
    use niri_config::{Action, PresetSize};
    use niri_ipc::FloatingFollow;

    use super::*;
    use crate::model::{Rect, Settings};

    /// Two windows in a row, each 8x6 tiles of 119x113 on a 1920x1080 output.
    fn desktop() -> (Desktop, Instant) {
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
        for id in 1..=2 {
            d.add_window(id);
            settle(&mut d, now);
        }
        (d, now)
    }

    fn settle(d: &mut Desktop, now: Instant) {
        for _ in 0..4 {
            d.manage(now);
            let asked = d.proposals();
            if asked.is_empty() {
                break;
            }
            for (id, size) in asked {
                d.set_asked(id, size);
                d.on_dimensions(id, size.0, size.1);
            }
        }
    }

    fn canvas(d: &Desktop) -> &Canvas {
        &d.output(1).unwrap().canvas
    }

    fn rects(d: &Desktop) -> Vec<(Id, Tiles)> {
        let mut v: Vec<(Id, Tiles)> = canvas(d).windows().iter().map(|w| (w.id, w.rect)).collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    fn tiles(d: &Desktop, id: Id) -> Tiles {
        canvas(d).rect(id).unwrap()
    }

    /// A point of window `id` as drawn, at fractions `fx`, `fy` of its size.
    fn point(d: &mut Desktop, id: Id, (fx, fy): (f64, f64), now: Instant) -> (i32, i32) {
        let f = d.render(now).into_iter().find(|f| f.id == id).unwrap();
        let r = d.window_rect(id).unwrap();
        (
            f.pos.0 + (f64::from(r.w) * fx) as i32,
            f.pos.1 + (f64::from(r.h) * fy) as i32,
        )
    }

    #[test]
    fn the_edges_follow_the_thirds_of_the_window_and_none_from_its_middle() {
        let (mut d, now) = desktop();
        let (x, y) = point(&mut d, 1, (0.5, 0.5), now);
        assert!(
            !d.resize_begin(x, y, now),
            "the middle third both ways resizes nothing"
        );
        assert!(d.moves.resize.is_none());
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        assert_eq!(
            d.moves.resize.as_ref().unwrap().edges,
            (Some(Edge::Right), None)
        );
        assert_eq!(d.focus(), Some(1), "the resized window takes the focus");
        d.cancel_resize();
        let (x, y) = point(&mut d, 2, (0.1, 0.9), now);
        assert!(d.resize_begin(x, y, now));
        assert_eq!(
            d.moves.resize.as_ref().unwrap().edges,
            (Some(Edge::Left), Some(Edge::Bottom))
        );
        assert!(!d.resize_begin(x, y, now), "one resize drag at a time");
    }

    #[test]
    fn a_resize_drag_previews_what_the_release_does() {
        let (mut d, now) = desktop();
        let before = rects(&d);
        assert_eq!(tiles(&d, 1), Tiles::new(0, 0, 8, 6));
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(40., 0.);
        assert_eq!(
            d.resize_preview(),
            (None, vec![]),
            "under half a tile, nothing to preview"
        );
        assert_eq!(d.resize_box_frame(), None);
        d.resize_to(130., 0.);
        let (shown, others) = d.resize_preview();
        assert_eq!(
            rects(&d),
            before,
            "the drag changes nothing until the release"
        );
        assert_eq!(shown, Some(Tiles::new(0, 0, 9, 6)), "one tile wider");
        assert!(
            !others.is_empty(),
            "the neighbour is drawn where the resize pushes it"
        );
        let frame = d.resize_box_frame().expect("the box is drawn");
        assert_eq!(
            frame.color, d.settings.border.active,
            "in the border's active colour"
        );
        assert_eq!(
            frame.rect.w,
            9 * 119 - 10,
            "over the window's tiles after the resize"
        );
        d.resize_end(now);
        let after = rects(&d);
        assert_eq!(
            tiles(&d, 1),
            Tiles::new(0, 0, 9, 6),
            "the release applies the previewed size"
        );
        let mut moved: Vec<Tiles> = after
            .iter()
            .zip(&before)
            .filter(|((id, a), (_, b))| *id != 1 && a != b)
            .map(|((_, a), _)| *a)
            .collect();
        moved.sort_by_key(|r| (r.j, r.i));
        assert_eq!(moved, others, "the others land where the preview drew them");
        assert_eq!(
            d.resize_preview(),
            (None, vec![]),
            "the release takes the preview down"
        );
        assert_eq!(d.resize_box_frame(), None);
        #[cfg(feature = "telemetry")]
        {
            let resizes: Vec<_> = d
                .motion
                .lines
                .iter()
                .filter(|l| l["op"] == "resize")
                .collect();
            assert_eq!(resizes.len(), 1, "one line per edge moved");
        }
        canvas(&d).check().unwrap();
    }

    #[test]
    fn a_resize_drag_back_and_cancel_keep_the_layout() {
        let (mut d, now) = desktop();
        let before = rects(&d);
        let (x, y) = point(&mut d, 2, (0.1, 0.9), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(-130., 120.);
        let (shown, _) = d.resize_preview();
        assert!(
            shown.is_some(),
            "a tile left and a tile down previews a bigger window"
        );
        d.resize_to(-20., 10.);
        assert_eq!(
            d.resize_preview(),
            (None, vec![]),
            "dragging back takes the preview down"
        );
        d.resize_to(0., 130.);
        assert!(d.resize_preview().0.is_some());
        assert!(d.cancel_resize(), "Esc cancels the resize drag");
        assert!(!d.cancel_resize(), "and only once");
        d.resize_end(now);
        assert_eq!(rects(&d), before, "a cancelled drag resizes nothing");
        assert_eq!(d.resize_preview(), (None, vec![]));
        #[cfg(feature = "telemetry")]
        {
            assert!(
                d.motion.lines.iter().any(|l| l["op"] == "cancel"),
                "the cancel is logged"
            );
            assert!(!d.motion.lines.iter().any(|l| l["op"] == "resize"));
        }
    }

    #[test]
    fn a_release_onto_the_preview_starts_no_second_glide() {
        let (mut d, now) = desktop();
        d.set_view_settings(crate::model::ViewSettings {
            movement: std::time::Duration::from_millis(250),
            ..Default::default()
        });
        let home = d.render(now);
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(130., 0.);
        let later = Instant::now() + std::time::Duration::from_secs(1);
        let previewed = d.render(later);
        d.resize_end(later);
        let released = d.render(Instant::now());
        let at = |frames: &[crate::model::Frame]| frames.iter().find(|f| f.id == 2).unwrap().pos;
        assert_eq!(
            at(&released),
            at(&previewed),
            "the pushed window is on its new tiles already"
        );
        assert_ne!(at(&previewed), at(&home), "the preview pushed it");
        assert_eq!(d.resize_preview(), (None, vec![]));
    }

    /// Window 1 floating where it was tiled, its app taking each size asked: its rectangle.
    fn floating(d: &mut Desktop, now: Instant) -> Rect {
        d.act(Action::FocusWindow(1), now);
        d.act(Action::ToggleWindowFloating, now);
        settle(d, now);
        d.render(now);
        assert!(d.window(1).unwrap().is_floating());
        d.window_rect(1).unwrap()
    }

    fn rect(d: &Desktop) -> Rect {
        d.window_rect(1).unwrap()
    }

    fn edges(d: &Desktop) -> Edges {
        d.moves.resize.as_ref().unwrap().edges
    }

    fn min_span(d: &Desktop, id: Id) -> (i32, i32) {
        let cw = canvas(d).window(id).unwrap();
        canvas(d).need(cw.min_w, cw.min_h)
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_floating_window_resizes_by_the_pixel_from_a_corner_as_the_drag_moves() {
        let (mut d, now) = desktop();
        let r = floating(&mut d, now);
        let tiled = rects(&d);
        let (x, y) = point(&mut d, 1, (0.9, 0.9), now);
        assert!(d.resize_begin(x, y, now));
        assert_eq!(edges(&d), (Some(Edge::Right), Some(Edge::Bottom)));
        d.resize_to(61., 39.);
        assert_eq!(
            d.proposals(),
            vec![(1, (r.w + 61, r.h + 39))],
            "asked of the app as the drag moves, by the pixel"
        );
        settle(&mut d, now);
        assert_eq!(
            rect(&d),
            Rect::new(r.x, r.y, r.w + 61, r.h + 39),
            "top-left stays"
        );
        assert_eq!(
            d.resize_box_frame(),
            None,
            "no box: the window itself resizes"
        );
        d.resize_to(100., 50.);
        settle(&mut d, now);
        d.resize_end(now);
        settle(&mut d, now);
        assert_eq!(
            rect(&d),
            Rect::new(r.x, r.y, r.w + 100, r.h + 50),
            "the release keeps the size"
        );
        assert_eq!(d.window(1).unwrap().grip, None);
        assert_eq!(rects(&d), tiled, "no tiles move");
        #[cfg(feature = "telemetry")]
        assert!(!d.motion.lines.iter().any(|l| l["op"] == "resize"));
    }

    #[test]
    fn dragging_a_floating_windows_left_edge_moves_it_and_the_right_edge_stays() {
        let (mut d, now) = desktop();
        let r = floating(&mut d, now);
        let (x, y) = point(&mut d, 1, (0.1, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        assert_eq!(edges(&d), (Some(Edge::Left), None));
        d.resize_to(-50., 30.);
        assert_eq!(
            d.proposals(),
            vec![(1, (r.w + 50, r.h))],
            "only the dragged axis"
        );
        assert_eq!(rect(&d), r, "nothing moves before the app commits");
        settle(&mut d, now);
        assert_eq!(rect(&d), Rect::new(r.x - 50, r.y, r.w + 50, r.h));
        d.resize_to(-20., 0.);
        settle(&mut d, now);
        assert_eq!(rect(&d), Rect::new(r.x - 20, r.y, r.w + 20, r.h));
        d.resize_end(now);
        assert_eq!(d.window(1).unwrap().grip, None, "the app has the last size");
        d.on_dimensions(1, r.w, r.h);
        assert_eq!(
            rect(&d),
            Rect::new(r.x - 20, r.y, r.w, r.h),
            "a later size of the app's own keeps the left edge"
        );
    }

    #[test]
    fn a_left_edge_commit_after_the_release_still_moves_the_window() {
        let (mut d, now) = desktop();
        let r = floating(&mut d, now);
        let (x, y) = point(&mut d, 1, (0.1, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(-40., 0.);
        d.resize_end(now);
        assert!(
            d.window(1).unwrap().grip.is_some(),
            "the app has not committed yet"
        );
        settle(&mut d, now);
        assert_eq!(rect(&d), Rect::new(r.x - 40, r.y, r.w + 40, r.h));
        assert_eq!(d.window(1).unwrap().grip, None);
    }

    #[test]
    fn a_floating_resize_keeps_to_the_apps_minimum_and_maximum() {
        let (mut d, now) = desktop();
        let r = floating(&mut d, now);
        let w = d.window_mut(1).unwrap();
        w.min = (400, 300);
        w.max = (r.w + 30, 0);
        let (x, y) = point(&mut d, 1, (0.1, 0.1), now);
        assert!(d.resize_begin(x, y, now));
        assert_eq!(edges(&d), (Some(Edge::Left), Some(Edge::Top)));
        d.resize_to(5000., 5000.);
        settle(&mut d, now);
        assert_eq!(
            rect(&d),
            Rect::new(r.x + r.w - 400, r.y + r.h - 300, 400, 300),
            "down to its minimum, the bottom-right corner in place"
        );
        d.resize_to(-200., -200.);
        settle(&mut d, now);
        assert_eq!(
            rect(&d),
            Rect::new(r.x - 30, r.y - 200, r.w + 30, r.h + 200),
            "no wider than its maximum, no limit down"
        );
        d.resize_end(now);
    }

    #[test]
    fn esc_puts_a_floating_windows_size_and_place_back() {
        let (mut d, now) = desktop();
        let r = floating(&mut d, now);
        let (x, y) = point(&mut d, 1, (0.1, 0.9), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(-80., 45.);
        settle(&mut d, now);
        assert_eq!(rect(&d), Rect::new(r.x - 80, r.y, r.w + 80, r.h + 45));
        assert!(d.cancel_resize());
        assert_eq!(d.proposals(), vec![(1, (r.w, r.h))], "its size asked back");
        assert_eq!(
            (rect(&d).x, rect(&d).y),
            (r.x, r.y),
            "back in place at once"
        );
        settle(&mut d, now);
        assert_eq!(rect(&d), r);
        assert!(!d.cancel_resize());
        #[cfg(feature = "telemetry")]
        assert!(
            !d.motion.lines.iter().any(|l| l["op"] == "cancel"),
            "no tiles: nothing logged"
        );
    }

    #[test]
    fn a_floating_window_has_no_double_click_every_press_resizes() {
        let (mut d, now) = desktop();
        floating(&mut d, now);
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        for n in 0..3_u32 {
            assert!(d.resize_begin(x, y, now + n * 50 * MS), "press {n}");
            d.resize_end(now);
        }
        assert_eq!(d.window(1).unwrap().resize_press, None);
    }

    #[test]
    fn a_floating_window_drawn_smaller_resizes_by_the_pointer_over_its_scale() {
        let (mut d, now) = desktop();
        let r = floating(&mut d, now);
        d.act(Action::SetFloatingFollow(FloatingFollow::Canvas), now);
        d.render(now);
        d.act(Action::ZoomOut, now);
        let f = d.render(now).into_iter().find(|f| f.id == 1).unwrap();
        let s = f.scale;
        assert!(s < 1., "drawn at the camera's scale: {s}");
        let x = f.pos.0 + (f64::from(r.w) * s * 0.9) as i32;
        let y = f.pos.1 + (f64::from(r.h) * s * 0.5) as i32;
        assert!(d.resize_begin(x, y, now), "resizes while zoomed");
        assert_eq!(edges(&d), (Some(Edge::Right), None));
        d.resize_to(80., 0.);
        assert_eq!(
            d.proposals(),
            vec![(1, (r.w + (80. / s).round() as i32, r.h))],
            "the edge keeps up with the pointer as drawn"
        );
    }

    #[test]
    fn a_double_super_right_click_toggles_full_width_and_back() {
        let (mut d, now) = desktop();
        let full = (canvas(&d).screen().0 / 119.).floor() as i32;
        assert!(full > 8, "{full}");
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now), "the first press resizes");
        d.resize_end(now);
        d.act(Action::FocusWindow(2), now);
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        let t = now + 300 * MS;
        assert!(
            !d.resize_begin(x, y, t),
            "the second within 400 ms drags nothing"
        );
        assert!(d.moves.resize.is_none());
        assert_eq!(
            tiles(&d, 1),
            Tiles::new(0, 0, full, 6),
            "full width, same height"
        );
        assert_eq!(d.focus(), Some(1), "and focuses the window");
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(
            d.resize_begin(x, y, t + 100 * MS),
            "a third press resizes again"
        );
        d.resize_end(t);
        assert!(!d.resize_begin(x, y, t + 200 * MS));
        let min = min_span(&d, 1);
        assert!(min.0 < full);
        assert_eq!(
            tiles(&d, 1),
            Tiles::new(0, 0, min.0, 6),
            "full width again goes back to the fewest tiles"
        );
        #[cfg(feature = "telemetry")]
        {
            let resizes = d
                .motion
                .lines
                .iter()
                .filter(|l| l["op"] == "resize")
                .count();
            assert_eq!(resizes, 2, "each toggle is a logged resize");
        }
        canvas(&d).check().unwrap();
        d.act(Action::Undo, now);
        assert_eq!(tiles(&d, 1).w, full, "undone like any resize");
    }

    #[test]
    fn a_double_click_takes_the_edges_both_presses_share() {
        let (mut d, now) = desktop();
        let min = min_span(&d, 1);
        assert!(min.1 < 6);
        let (x, y) = point(&mut d, 1, (0.9, 0.9), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_end(now);
        let (x, y) = point(&mut d, 1, (0.5, 0.9), now);
        assert!(!d.resize_begin(x, y, now + 100 * MS));
        assert_eq!(
            tiles(&d, 1),
            Tiles::new(0, 0, 8, min.1),
            "the bottom edge both took resets the height; the width stays"
        );
        let before = rects(&d);
        let t = now + 2000 * MS;
        let (xl, yl) = point(&mut d, 1, (0.1, 0.5), now);
        let (xr, yr) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(xl, yl, t));
        d.resize_end(t);
        assert!(
            !d.resize_begin(xr, yr, t + 100 * MS),
            "left then right: a double click all the same"
        );
        assert_eq!(
            rects(&d),
            before,
            "with no edge in common it changes nothing"
        );
        assert!(d.resize_begin(xr, yr, t + 200 * MS));
        d.resize_end(t);
        assert!(
            d.resize_begin(xr, yr, t + 700 * MS),
            "presses 500 ms apart are two resizes"
        );
        d.cancel_resize();
        assert_eq!(rects(&d), before);
    }

    #[test]
    fn maximize_column_and_reset_window_height_take_the_same_paths() {
        let (mut d, now) = desktop();
        let full = (canvas(&d).screen().0 / 119.).floor() as i32;
        d.act(Action::FocusWindow(2), now);
        d.act(Action::MaximizeColumn, now);
        assert_eq!((tiles(&d, 2).w, tiles(&d, 2).h), (full, 6));
        d.act(Action::ResetWindowHeight, now);
        assert_eq!(tiles(&d, 2).h, min_span(&d, 2).1);
        d.act(Action::MaximizeColumn, now);
        assert_eq!(tiles(&d, 2).w, min_span(&d, 2).0);
        canvas(&d).check().unwrap();
    }

    #[test]
    fn a_closed_window_ends_its_resize_drag() {
        let (mut d, now) = desktop();
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(130., 0.);
        d.remove_window(1, now);
        assert!(d.moves.resize.is_none());
        assert_eq!(d.resize_preview(), (None, vec![]));
        assert!(!d.cancel_resize());
    }

    /// Where window `id` is drawn.
    fn drawn(d: &mut Desktop, id: Id, now: Instant) -> (i32, i32) {
        d.render(now).into_iter().find(|f| f.id == id).unwrap().pos
    }

    /// Lets go of window `id`'s resize drag and checks it does what the preview showed just
    /// before.
    #[track_caller]
    fn assert_release_matches_preview(d: &mut Desktop, id: Id, when: &str, now: Instant) {
        let before = rects(d);
        let (shown, others) = d.resize_preview();
        d.resize_end(now);
        let after = rects(d);
        let own = |v: &[(Id, Tiles)]| v.iter().find(|(w, _)| *w == id).map(|(_, r)| *r);
        let applied = own(&after).filter(|r| Some(*r) != own(&before));
        assert_eq!(applied, shown, "{when}: the release applies the box");
        let mut moved: Vec<Tiles> = after
            .iter()
            .filter(|(w, r)| *w != id && before.iter().any(|(b, br)| b == w && br != r))
            .map(|(_, r)| *r)
            .collect();
        moved.sort_by_key(|r| (r.j, r.i));
        assert_eq!(
            moved, others,
            "{when}: the others land where the preview drew them"
        );
    }

    #[test]
    fn a_shrinking_release_keeps_the_previewed_neighbour_where_the_preview_drew_it() {
        let (mut d, now) = desktop();
        let home = drawn(&mut d, 2, now);
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(-240., 0.);
        let (shown, others) = d.resize_preview();
        assert_eq!(shown, Some(Tiles::new(0, 0, 6, 6)), "two tiles narrower");
        assert!(!others.is_empty(), "2 is drawn in the freed tiles");
        let previewed = drawn(&mut d, 2, now);
        assert_ne!(previewed, home);
        d.resize_end(now);
        assert!(
            d.shrinking.contains(&1),
            "1 is still drawn at its old size: its shrink waits"
        );
        assert_eq!(
            drawn(&mut d, 2, now),
            previewed,
            "2 stays where the preview drew it while the shrink waits"
        );
        settle(&mut d, now);
        assert!(d.shrinking.is_empty(), "1 committed its smaller size");
        assert_eq!(drawn(&mut d, 2, now), previewed, "and after it shrank");
    }

    #[test]
    fn the_resize_preview_follows_a_window_opening_during_the_drag() {
        let (mut d, now) = desktop();
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(130., 0.);
        assert!(!d.resize_preview().1.is_empty(), "2 is previewed pushed");
        d.add_window(3);
        settle(&mut d, now);
        assert!(canvas(&d).rect(3).is_some(), "3 opened during the drag");
        assert!(
            !d.resize_preview().1.is_empty(),
            "the preview is drawn again on the changed canvas"
        );
        assert_release_matches_preview(&mut d, 1, "after a window opened", now);
    }

    #[test]
    fn the_resize_preview_follows_a_zoom_during_the_drag() {
        let (mut d, now) = desktop();
        let (x, y) = point(&mut d, 1, (0.9, 0.5), now);
        assert!(d.resize_begin(x, y, now));
        d.resize_to(170., 0.);
        assert_eq!(
            d.resize_preview().0,
            Some(Tiles::new(0, 0, 9, 6)),
            "170 px is one tile of 119"
        );
        d.act(Action::ZoomOut, now);
        assert_eq!(
            d.resize_preview().0,
            Some(Tiles::new(0, 0, 10, 6)),
            "and two of 95 at 0.8: the box follows the zoom"
        );
        assert_release_matches_preview(&mut d, 1, "after a zoom", now);
    }
}
