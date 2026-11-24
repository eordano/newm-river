//! niri's monitor actions on the canvases. Focusing an output makes it the active one (new
//! windows open there) and focuses its canvas's focus, or nothing when it is empty; moving a
//! window (or a "column": the canvas has none, so the window) puts it on the target output's
//! canvas at its span, beside that canvas's focus, and the focus follows it.

use std::time::Instant;

use niri_config::Action;

use super::{Anchor, Desktop, OutputId, Place, Rect};
use newm_canvas::Id;

/// Where the target output lies from the active one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toward {
    Left,
    Right,
    Up,
    Down,
    Previous,
    Next,
}

fn centre(r: Rect) -> (i64, i64) {
    (
        i64::from(r.x) + i64::from(r.w) / 2,
        i64::from(r.y) + i64::from(r.h) / 2,
    )
}

/// Whether two spans overlap by more than a point.
fn overlap(a: i32, al: i32, b: i32, bl: i32) -> bool {
    i64::from(a) < i64::from(b) + i64::from(bl) && i64::from(b) < i64::from(a) + i64::from(al)
}

impl Desktop {
    /// The output in a direction from `from`, as niri finds it: among those overlapping its
    /// row (or column) whose centre lies that way, the nearest; previous/next cycle through the
    /// outputs sorted by name.
    pub fn output_toward(&self, from: OutputId, t: Toward) -> Option<OutputId> {
        let cur = self.output(from)?.area;
        let (cx, cy) = centre(cur);
        let pick = |test: &dyn Fn(Rect) -> Option<i64>| {
            self.outputs
                .iter()
                .filter(|o| o.id != from)
                .filter_map(|o| test(o.area).map(|d| (d, o.id)))
                .min()
                .map(|(_, id)| id)
        };
        let row = |r: Rect| overlap(r.y, r.h, cur.y, cur.h);
        let col = |r: Rect| overlap(r.x, r.w, cur.x, cur.w);
        match t {
            Toward::Left => pick(&|r| (row(r) && centre(r).0 < cx).then(|| cx - centre(r).0)),
            Toward::Right => pick(&|r| (row(r) && centre(r).0 > cx).then(|| centre(r).0 - cx)),
            Toward::Up => pick(&|r| (col(r) && centre(r).1 < cy).then(|| cy - centre(r).1)),
            Toward::Down => pick(&|r| (col(r) && centre(r).1 > cy).then(|| centre(r).1 - cy)),
            Toward::Previous | Toward::Next => {
                let mut sorted: Vec<_> = self.outputs.iter().map(|o| (&o.name, o.id)).collect();
                sorted.sort();
                let k = sorted.iter().position(|&(_, id)| id == from)?;
                let n = sorted.len();
                let k = if t == Toward::Next {
                    (k + 1) % n
                } else {
                    (k + n - 1) % n
                };
                Some(sorted[k].1).filter(|&id| id != from)
            }
        }
    }

    /// The output a config or IPC names: its connector, ignoring case.
    pub fn output_named(&self, name: &str) -> Option<OutputId> {
        self.outputs
            .iter()
            .find(|o| o.name.eq_ignore_ascii_case(name))
            .map(|o| o.id)
    }

    /// Makes an output the active one: its canvas's focus takes the keyboard, or nothing when it
    /// has no window, and new windows open there. Returns whether anything changed.
    pub fn focus_output(&mut self, id: OutputId, now: Instant) -> bool {
        if self.output(id).is_none() || self.active_output() == Some(id) {
            return false;
        }
        let target = self.output(id).and_then(|o| o.canvas.focus()).or_else(|| {
            self.mru
                .iter()
                .copied()
                .find(|m| self.windows.get(m).is_some_and(|w| w.output == Some(id)))
        });
        match target {
            Some(w) if self.focus_window(w, now) => {}
            _ => {
                if self.focus.is_some() {
                    self.focus_dirty = true;
                }
                self.focus = None;
            }
        }
        self.active_output = Some(id);
        true
    }

    /// A press on the desktop (no window under it) activates the output it lands on.
    pub fn press_on_desktop(&mut self, x: i32, y: i32, now: Instant) -> bool {
        self.output_at(x, y)
            .is_some_and(|id| self.focus_output(id, now))
    }

    /// Moves a window to another output: a tiled one onto that canvas at its span beside the
    /// canvas's focus, a floating one by the offset between the outputs. Focuses it there. A
    /// restored window no longer holds its saved spot.
    pub fn move_window_to_output(&mut self, id: Id, to: OutputId, now: Instant) -> bool {
        let Some(w) = self.windows.get(&id) else {
            return false;
        };
        let (from, place, mins) = (w.output, w.place, self.tile_min(w));
        if from == Some(to) || w.fullscreen || self.output(to).is_none() {
            return false;
        }
        match place {
            Some(Place::Tiled) => {
                let span = from
                    .and_then(|o| self.output(o))
                    .and_then(|o| o.canvas.rect(id))
                    .map(|r| (r.w, r.h));
                let (min_w, min_h) = mins;
                let app = w.app_id.clone();
                let opening = w.opening.is_some();
                if let (Some(app), Some(span), Some(o)) = (app, span, self.output_mut(from)) {
                    super::open::remember_span(o, app, span);
                }
                self.apply(from, |c| c.remove(id));
                self.held.remove(&id);
                self.shrinking.retain(|&s| s != id);
                if !opening && let Some(o) = self.output_mut(Some(to)) {
                    o.canvas.add(id, min_w, min_h, span);
                }
            }
            Some(Place::Floating { anchor, pos }) => {
                let origin = |o: Option<OutputId>| {
                    o.and_then(|o| self.output(o))
                        .map_or((0, 0), |o| (o.work_area().x, o.work_area().y))
                };
                let (fx, fy) = origin(from);
                let (tx, ty) = origin(Some(to));
                let anchor = match anchor {
                    Anchor::Parent(_) => Anchor::Centre,
                    a => a,
                };
                let pos = match anchor {
                    Anchor::Centre if pos.is_none() => None,
                    _ => pos.map(|(x, y)| (x - fx + tx, y - fy + ty)),
                };
                self.windows.get_mut(&id).unwrap().place = Some(Place::Floating { anchor, pos });
            }
            None => return false,
        }
        let w = self.windows.get_mut(&id).unwrap();
        w.output = Some(to);
        w.asked = None;
        // Moved off its saved spot, a restored window trades it no more (`rematch.rs`).
        w.slot = None;
        self.focus_window(id, now);
        true
    }

    fn monitor_toward(&self, t: Toward) -> Option<OutputId> {
        self.output_toward(self.active_canvas_output()?, t)
    }

    fn focus_toward(&mut self, t: Toward, now: Instant) -> bool {
        self.monitor_toward(t)
            .is_some_and(|o| self.focus_output(o, now))
    }

    fn move_focus_toward(&mut self, t: Toward, now: Instant) -> bool {
        match (self.focus, self.monitor_toward(t)) {
            (Some(f), Some(o)) => self.move_window_to_output(f, o, now),
            _ => false,
        }
    }

    /// The monitor actions; None when `action` is not one.
    pub(super) fn monitor_action(&mut self, action: &Action, now: Instant) -> Option<bool> {
        use Toward::*;
        let toward = |a: &Action| {
            Some(match a {
                Action::FocusMonitorLeft
                | Action::MoveWindowToMonitorLeft
                | Action::MoveColumnToMonitorLeft => Left,
                Action::FocusMonitorRight
                | Action::MoveWindowToMonitorRight
                | Action::MoveColumnToMonitorRight => Right,
                Action::FocusMonitorUp
                | Action::MoveWindowToMonitorUp
                | Action::MoveColumnToMonitorUp => Up,
                Action::FocusMonitorDown
                | Action::MoveWindowToMonitorDown
                | Action::MoveColumnToMonitorDown => Down,
                Action::FocusMonitorPrevious
                | Action::MoveWindowToMonitorPrevious
                | Action::MoveColumnToMonitorPrevious => Previous,
                Action::FocusMonitorNext
                | Action::MoveWindowToMonitorNext
                | Action::MoveColumnToMonitorNext => Next,
                _ => return None,
            })
        };
        Some(match action {
            Action::FocusMonitorLeft
            | Action::FocusMonitorRight
            | Action::FocusMonitorUp
            | Action::FocusMonitorDown
            | Action::FocusMonitorPrevious
            | Action::FocusMonitorNext => self.focus_toward(toward(action)?, now),
            Action::MoveWindowToMonitorLeft
            | Action::MoveWindowToMonitorRight
            | Action::MoveWindowToMonitorUp
            | Action::MoveWindowToMonitorDown
            | Action::MoveWindowToMonitorPrevious
            | Action::MoveWindowToMonitorNext
            | Action::MoveColumnToMonitorLeft
            | Action::MoveColumnToMonitorRight
            | Action::MoveColumnToMonitorUp
            | Action::MoveColumnToMonitorDown
            | Action::MoveColumnToMonitorPrevious
            | Action::MoveColumnToMonitorNext => self.move_focus_toward(toward(action)?, now),
            Action::FocusMonitor(name) => self
                .output_named(name)
                .is_some_and(|o| self.focus_output(o, now)),
            Action::MoveWindowToMonitor(name) | Action::MoveColumnToMonitor(name) => {
                match (self.focus, self.output_named(name)) {
                    (Some(f), Some(o)) => self.move_window_to_output(f, o, now),
                    _ => false,
                }
            }
            Action::MoveWindowToMonitorById { id, output } => self
                .output_named(output)
                .is_some_and(|o| self.move_window_to_output(*id, o, now)),
            Action::FocusColumnOrMonitorLeft | Action::FocusColumnOrMonitorRight => {
                let (d, t) = if *action == Action::FocusColumnOrMonitorLeft {
                    (newm_canvas::Dir::Left, Left)
                } else {
                    (newm_canvas::Dir::Right, Right)
                };
                self.focus_dir_or_monitor(d, t, now)
            }
            Action::FocusWindowOrMonitorUp | Action::FocusWindowOrMonitorDown => {
                let (d, t) = if *action == Action::FocusWindowOrMonitorUp {
                    (newm_canvas::Dir::Up, Up)
                } else {
                    (newm_canvas::Dir::Down, Down)
                };
                self.focus_dir_or_monitor(d, t, now)
            }
            Action::MoveColumnLeftOrToMonitorLeft | Action::MoveColumnRightOrToMonitorRight => {
                let (d, t) = if *action == Action::MoveColumnLeftOrToMonitorLeft {
                    (newm_canvas::Dir::Left, Left)
                } else {
                    (newm_canvas::Dir::Right, Right)
                };
                let moved = self.focused_tiled().is_some_and(|(id, out)| {
                    self.apply(out, |c| c.move_by(id, d)).unwrap_or(false)
                });
                if moved {
                    let out = self.active_canvas_output();
                    self.follow_canvas(out, now);
                    true
                } else {
                    self.move_focus_toward(t, now)
                }
            }
            _ => return None,
        })
    }

    /// Focus moves on the canvas, or to the output that way when the canvas has nothing there.
    fn focus_dir_or_monitor(&mut self, d: newm_canvas::Dir, t: Toward, now: Instant) -> bool {
        let out = self.active_canvas_output();
        let moved = self.focused_tiled().is_some()
            && self.output_mut(out).is_some_and(|o| o.canvas.focus_dir(d));
        if moved {
            self.follow_canvas(out, now);
            true
        } else {
            self.focus_toward(t, now)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Settings;
    use newm_canvas::Config as CanvasConfig;
    use niri_config::PresetSize;

    fn desktop(areas: &[(&str, Rect)]) -> (Desktop, Instant) {
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
        for (k, (name, area)) in areas.iter().enumerate() {
            let id = k as OutputId + 1;
            d.add_output(id);
            d.set_output_name(id, (*name).into());
            d.set_output_area(id, *area);
        }
        (d, now)
    }

    fn two() -> (Desktop, Instant) {
        desktop(&[
            ("DP-1", Rect::new(0, 0, 1920, 1080)),
            ("HDMI-A-1", Rect::new(1920, 0, 2560, 1080)),
        ])
    }

    fn settle(d: &mut Desktop, now: Instant) {
        d.manage(now);
        for (id, size) in d.proposals() {
            d.set_asked(id, size);
            let size = if size == (0, 0) { (400, 300) } else { size };
            d.on_dimensions(id, size.0, size.1);
        }
    }

    fn open(d: &mut Desktop, id: Id, now: Instant) {
        d.add_window(id);
        settle(d, now);
    }

    fn span(d: &Desktop, out: OutputId, id: Id) -> Option<(i32, i32)> {
        d.output(out)?.canvas.rect(id).map(|r| (r.w, r.h))
    }

    #[test]
    fn outputs_toward_follow_niri() {
        let (d, _) = desktop(&[
            ("DP-1", Rect::new(0, 0, 1920, 1080)),
            ("DP-2", Rect::new(1920, 0, 2560, 1080)),
            ("HDMI-A-1", Rect::new(0, 1080, 1920, 1080)),
        ]);
        assert_eq!(d.output_toward(1, Toward::Right), Some(2));
        assert_eq!(d.output_toward(1, Toward::Down), Some(3));
        assert_eq!(d.output_toward(1, Toward::Left), None);
        assert_eq!(d.output_toward(1, Toward::Up), None);
        assert_eq!(d.output_toward(3, Toward::Up), Some(1));
        assert_eq!(d.output_toward(3, Toward::Right), None);
        assert_eq!(d.output_toward(2, Toward::Left), Some(1));
        assert_eq!(d.output_toward(3, Toward::Next), Some(1));
        assert_eq!(d.output_toward(1, Toward::Previous), Some(3));
        assert_eq!(d.output_at(1920, 5), Some(2));
        assert_eq!(d.output_named("hdmi-a-1"), Some(3));
    }

    #[test]
    fn a_press_on_an_empty_output_opens_new_windows_there() {
        let (mut d, now) = two();
        open(&mut d, 1, now);
        assert_eq!(d.window(1).unwrap().output, Some(1));
        d.focus_dirty = false;
        assert!(d.press_on_desktop(2500, 500, now));
        assert_eq!((d.active_output(), d.focus()), (Some(2), None));
        assert!(d.focus_dirty, "river is told to clear the keyboard focus");
        assert!(!d.press_on_desktop(2600, 500, now), "already active");
        open(&mut d, 2, now);
        assert_eq!(d.window(2).unwrap().output, Some(2));
        assert_eq!(d.focus(), Some(2));
    }

    #[test]
    fn focus_monitor_focuses_that_canvas_focus() {
        let (mut d, now) = two();
        open(&mut d, 1, now);
        d.act(Action::FocusMonitorRight, now);
        open(&mut d, 2, now);
        assert_eq!(d.window(2).unwrap().output, Some(2));
        d.act(Action::FocusMonitorLeft, now);
        assert_eq!((d.focus(), d.active_output()), (Some(1), Some(1)));
        d.act(Action::FocusMonitor("HDMI-A-1".into()), now);
        assert_eq!(d.focus(), Some(2));
        d.act(Action::FocusMonitorNext, now);
        assert_eq!(d.focus(), Some(1));
    }

    #[test]
    fn a_window_moves_to_a_monitor_at_its_span_and_the_focus_follows() {
        let (mut d, now) = two();
        open(&mut d, 1, now);
        open(&mut d, 2, now);
        let s = span(&d, 1, 2).unwrap();
        d.act(Action::FocusMonitorRight, now);
        open(&mut d, 3, now);
        d.act(Action::FocusMonitorLeft, now);
        assert_eq!(d.focus(), Some(2));
        d.act(Action::MoveWindowToMonitorRight, now);
        settle(&mut d, now);
        assert_eq!(span(&d, 1, 2), None);
        assert_eq!(span(&d, 2, 2), Some(s));
        assert_eq!(d.window(2).unwrap().output, Some(2));
        assert_eq!((d.focus(), d.active_output()), (Some(2), Some(2)));
        let (a, b) = (
            d.output(2).unwrap().canvas.rect(2).unwrap(),
            d.output(2).unwrap().canvas.rect(3).unwrap(),
        );
        let apart = a.i + a.w <= b.i || b.i + b.w <= a.i || a.j + a.h <= b.j || b.j + b.h <= a.j;
        assert!(apart, "{a:?} overlaps {b:?}");
        d.act(Action::MoveColumnToMonitorLeft, now);
        assert_eq!(span(&d, 1, 2), Some(s));
        d.act(
            Action::MoveWindowToMonitorById {
                id: 3,
                output: "DP-1".into(),
            },
            now,
        );
        assert_eq!(d.window(3).unwrap().output, Some(1));
        assert_eq!(d.output(2).unwrap().canvas.windows().len(), 0);
    }

    #[test]
    fn a_window_moved_in_keeps_its_span_and_its_apps_placeholder_waits() {
        // A carried span beats a placeholder. Here a moved window
        // never claims one: only a window placed for the first time does (`claim_placeholder`).
        let (mut d, now) = two();
        d.add_window(1);
        d.window_mut(1).unwrap().app_id = Some("app".into());
        settle(&mut d, now);
        d.act(Action::ResizeEdge(niri_ipc::CanvasEdge::Right, 1), now);
        d.act(Action::ResizeEdge(niri_ipc::CanvasEdge::Bottom, 1), now);
        settle(&mut d, now);
        let s = span(&d, 1, 1).unwrap();
        d.act(Action::FocusMonitorRight, now);
        let launch = Action::LaunchPlaceholder {
            app_id: "app".into(),
            icon: None,
        };
        assert!(d.act(launch, now).is_empty());
        assert_eq!(d.output(2).unwrap().canvas.windows().len(), 1);
        let held = d.output(2).unwrap().canvas.windows()[0].rect;
        assert_ne!((held.w, held.h), s, "the placeholder holds another span");
        d.act(Action::FocusMonitorLeft, now);
        d.act(Action::MoveWindowToMonitorRight, now);
        settle(&mut d, now);
        assert_eq!(span(&d, 2, 1), Some(s), "1 keeps the span it carried");
        assert_eq!(d.placeholder_frames().len(), 1, "the placeholder waits");
    }

    #[test]
    fn focus_or_monitor_crosses_to_the_next_output_at_the_canvas_edge() {
        let (mut d, now) = two();
        open(&mut d, 1, now);
        d.act(Action::FocusMonitorRight, now);
        open(&mut d, 2, now);
        d.act(Action::FocusMonitorLeft, now);
        d.act(Action::FocusColumnOrMonitorRight, now);
        assert_eq!(d.focus(), Some(2));
        d.act(Action::MoveColumnLeftOrToMonitorLeft, now);
        let w = d.window(2).unwrap().output;
        assert!(
            w == Some(1) || span(&d, 2, 2).is_some(),
            "moved on the canvas or across"
        );
    }

    #[test]
    fn a_scale_change_refits_the_canvas_and_keeps_the_tiles() {
        let (mut d, now) = desktop(&[("DP-1", Rect::new(0, 0, 2560, 1080))]);
        open(&mut d, 1, now);
        open(&mut d, 2, now);
        let before: Vec<_> = [1, 2].map(|id| d.output(1).unwrap().canvas.rect(id)).into();
        d.set_output_area(1, Rect::new(0, 0, 2048, 864));
        settle(&mut d, now);
        let o = d.output(1).unwrap();
        assert_eq!(o.canvas.screen(), (2048. - 10., 864. - 10.));
        let after: Vec<_> = [1, 2].map(|id| o.canvas.rect(id)).into();
        assert_eq!(before, after);
        for f in d.render(now) {
            let r = d.window_rect(f.id).unwrap();
            if let Some(c) = f.clip {
                assert!(c.w <= 2048 && c.h <= 864, "{c:?} for {r:?}");
            }
        }
        // The overview fits the canvas into the smaller screen: every window is drawn.
        d.act(Action::OpenOverview, now);
        let later = now + std::time::Duration::from_secs(5);
        d.tick(later);
        let frames = d.render(later);
        assert_eq!(frames.len(), 2);
        assert!(
            frames.iter().all(|f| f.clip.is_some() && f.scale <= 1.),
            "{frames:?}"
        );
    }
}
