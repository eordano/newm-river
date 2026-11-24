//! Floating windows that follow the canvas camera (`floating-follow`).
//!
//! A floating window keeps its place on screen. One that follows something has an anchor: where
//! the camera drew that reference when the window started following it. The window is then drawn
//! through the map that takes the reference from there to where it is drawn now, so it keeps its
//! offset to it, scaled as it is: `"canvas"` follows the canvas's grid origin through the camera
//! (pans, zooms and the overview), `"parent"` the middle of the tiled window a dialog belongs to,
//! as drawn. A window that stops following is put where it was drawn, at 1:1 about its middle.

use newm_canvas::Id;
use niri_ipc::FloatingFollow;

use super::{Desktop, OutputId, Place, Rect};

/// What a floating window follows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Reference {
    /// The canvas of an output, through its camera.
    Canvas(OutputId),
    /// The tiled window a dialog belongs to, as drawn.
    Parent(Id),
}

/// A point of a reference as it is drawn, and the scale it is drawn at.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Drawn {
    pos: (f64, f64),
    scale: f64,
}

/// The map from where a reference was drawn to where it is drawn now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Map {
    from: (f64, f64),
    to: (f64, f64),
    pub scale: f64,
}

impl Map {
    fn between(a: Drawn, b: Drawn) -> Self {
        Self {
            from: a.pos,
            to: b.pos,
            scale: b.scale / a.scale,
        }
    }

    fn point(&self, p: (f64, f64)) -> (f64, f64) {
        (
            self.to.0 + (p.0 - self.from.0) * self.scale,
            self.to.1 + (p.1 - self.from.1) * self.scale,
        )
    }

    /// Where a window at `r` (its own, unscaled size) is drawn through the map: its middle
    /// mapped, scaled about it. The scale is the map's.
    pub fn rect(&self, r: Rect) -> Rect {
        let (w, h) = (f64::from(r.w), f64::from(r.h));
        let (mx, my) = self.point((f64::from(r.x) + w / 2., f64::from(r.y) + h / 2.));
        Rect::new(
            (mx - w * self.scale / 2.).round() as i32,
            (my - h * self.scale / 2.).round() as i32,
            r.w,
            r.h,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Anchor {
    window: Id,
    reference: Reference,
    /// The reference as drawn when the window started following it.
    at: Drawn,
    /// How the window was drawn at the last sync, for when the reference is gone.
    last: Map,
}

/// The mode, the config's value it came from, and every following window's anchor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Follows {
    mode: FloatingFollow,
    config: FloatingFollow,
    anchors: Vec<Anchor>,
}

impl Follows {
    pub fn new(mode: FloatingFollow) -> Self {
        Self {
            mode,
            config: mode,
            anchors: Vec::new(),
        }
    }
}

impl Desktop {
    /// How floating windows follow the canvas camera now.
    pub fn floating_follow(&self) -> FloatingFollow {
        self.follows.mode
    }

    /// Makes floating windows follow the camera as `mode` says, until the config changes its
    /// `floating-follow`.
    pub(super) fn set_floating_follow(&mut self, mode: FloatingFollow) {
        self.follows.mode = mode;
        self.sync_follow();
    }

    /// The config's `floating-follow`: it takes over when it differs from the last one read.
    pub(super) fn follow_config(&mut self, mode: FloatingFollow) {
        if self.follows.config != mode {
            self.follows.config = mode;
            self.set_floating_follow(mode);
        }
    }

    /// Where `reference` is drawn now: the canvas's grid origin through its camera, or the
    /// middle of the parent window as drawn.
    fn drawn_ref(&self, reference: Reference) -> Option<Drawn> {
        match reference {
            Reference::Canvas(out) => {
                let o = self.output(out)?;
                let cam = self.camera(o);
                let (wa, inset) = (o.work_area(), self.inset());
                Some(Drawn {
                    pos: (
                        f64::from(wa.x) + inset - cam.view.0,
                        f64::from(wa.y) + inset - cam.view.1,
                    ),
                    scale: cam.scale,
                })
            }
            Reference::Parent(p) => {
                let w = self.windows.get(&p)?;
                let o = self.output(w.output?)?;
                if !w.is_tiled() || w.fullscreen {
                    return None;
                }
                let r = self.window_rect(p)?;
                let s = self.camera(o).scale;
                Some(Drawn {
                    pos: (
                        f64::from(r.x) + f64::from(r.w) * s / 2.,
                        f64::from(r.y) + f64::from(r.h) * s / 2.,
                    ),
                    scale: s,
                })
            }
        }
    }

    /// The tiled window floating window `id` is a dialog of, directly or through other dialogs.
    fn tiled_ancestor(&self, id: Id) -> Option<Id> {
        let mut child = self.windows.get(&id)?;
        for _ in 0..16 {
            let parent = self.windows.get(&child.parent?)?;
            if parent.is_tiled() {
                return Some(parent.id);
            }
            child = parent;
        }
        None
    }

    /// What floating window `id` follows in the current mode.
    fn wanted_reference(&self, id: Id) -> Option<Reference> {
        let w = self.windows.get(&id)?;
        if w.fullscreen || !matches!(w.place, Some(Place::Floating { pos: Some(_), .. })) {
            return None;
        }
        match self.follows.mode {
            FloatingFollow::Screen => None,
            FloatingFollow::Canvas => w.output.map(Reference::Canvas),
            FloatingFollow::Parent => self.tiled_ancestor(id).map(Reference::Parent),
        }
    }

    /// How floating window `id` is drawn now, when it follows something.
    pub(super) fn follow_map(&self, id: Id) -> Option<Map> {
        let a = self.follows.anchors.iter().find(|a| a.window == id)?;
        Some(
            self.drawn_ref(a.reference)
                .map_or(a.last, |now| Map::between(a.at, now)),
        )
    }

    /// Gives each floating window the anchor the mode asks for. A window starts following where
    /// it is drawn, so nothing moves; one that stops following, or whose reference changed or
    /// went, stays where it was drawn last, at 1:1 about its middle.
    pub(super) fn sync_follow(&mut self) {
        let ids: Vec<Id> = self
            .windows
            .values()
            .filter(|w| w.is_floating())
            .map(|w| w.id)
            .collect();
        let mut anchors = std::mem::take(&mut self.follows.anchors);
        anchors.retain(|a| ids.contains(&a.window));
        for id in ids {
            let wanted = self.wanted_reference(id);
            if let Some(idx) = anchors.iter().position(|a| a.window == id) {
                let a = &mut anchors[idx];
                let now = self.drawn_ref(a.reference);
                if let Some(now) = now.filter(|_| Some(a.reference) == wanted) {
                    a.last = Map::between(a.at, now);
                    continue;
                }
                let a = anchors.remove(idx);
                let map = now.map_or(a.last, |now| Map::between(a.at, now));
                self.put_where_drawn(id, map);
            }
            let Some(reference) = wanted else { continue };
            let Some(at) = self.drawn_ref(reference) else {
                continue;
            };
            anchors.push(Anchor {
                window: id,
                reference,
                at,
                last: Map::between(at, at),
            });
        }
        self.follows.anchors = anchors;
    }

    /// Moves floating window `id` so that its middle is where `map` draws it.
    fn put_where_drawn(&mut self, id: Id, map: Map) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let (
            Some(Place::Floating {
                anchor,
                pos: Some(p),
            }),
            Some(size),
        ) = (w.place, w.size)
        else {
            return;
        };
        let r = map.rect(Rect::new(p.0, p.1, size.0, size.1));
        let pos = (
            r.x + (f64::from(size.0) * (map.scale - 1.) / 2.).round() as i32,
            r.y + (f64::from(size.1) * (map.scale - 1.) / 2.).round() as i32,
        );
        w.place = Some(Place::Floating {
            anchor,
            pos: Some(pos),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use newm_canvas::Config as CanvasConfig;
    use niri_config::{Action, PresetSize};

    use super::super::{Desktop, Settings};
    use super::*;

    fn settle(d: &mut Desktop, now: Instant) {
        d.manage(now);
        for (id, size) in d.proposals() {
            d.set_asked(id, size);
            let size = if size == (0, 0) { (300, 200) } else { size };
            d.on_dimensions(id, size.0, size.1);
        }
    }

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            gaps: 10.,
            default_width: Some(PresetSize::Fixed(600)),
            default_height: Some(PresetSize::Proportion(0.5)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        d.add_window(1);
        settle(&mut d, now);
        d.add_window(2);
        d.window_mut(2).unwrap().parent = Some(1);
        settle(&mut d, now);
        d.render(now);
        (d, now)
    }

    fn drawn(d: &mut Desktop, id: Id, now: Instant) -> ((i32, i32), f64) {
        let f = d.render(now).into_iter().find(|f| f.id == id).unwrap();
        (f.pos, f.scale)
    }

    #[test]
    fn the_modes_cycle_and_the_config_takes_over_only_when_it_changes() {
        let (mut d, now) = desktop();
        assert_eq!(d.floating_follow(), FloatingFollow::Screen);
        let names: Vec<&str> = (0..3)
            .map(|_| {
                d.act(Action::CycleFloatingFollow, now);
                d.floating_follow().name()
            })
            .collect();
        assert_eq!(names, ["parent", "canvas", "screen"]);
        d.act(Action::SetFloatingFollow(FloatingFollow::Canvas), now);
        d.follow_config(FloatingFollow::Screen);
        assert_eq!(
            d.floating_follow(),
            FloatingFollow::Canvas,
            "same config value"
        );
        d.follow_config(FloatingFollow::Parent);
        assert_eq!(d.floating_follow(), FloatingFollow::Parent);
    }

    #[test]
    fn a_dialog_follows_its_parent_and_stays_where_drawn_when_it_stops() {
        let (mut d, now) = desktop();
        assert!(d.window(2).unwrap().is_floating(), "a dialog floats");
        let (home, _) = drawn(&mut d, 2, now);
        d.act(Action::SetFloatingFollow(FloatingFollow::Parent), now);
        assert_eq!(drawn(&mut d, 2, now).0, home, "following moves nothing");
        let (p0, _) = drawn(&mut d, 1, now);
        d.pan((600, 300), (-120., 40.), now);
        d.pan_end(now);
        let (p1, _) = drawn(&mut d, 1, now);
        let (moved, s) = drawn(&mut d, 2, now);
        assert_eq!(s, 1.);
        assert_eq!(
            (moved.0 - home.0, moved.1 - home.1),
            (p1.0 - p0.0, p1.1 - p0.1),
            "the dialog keeps its offset to its parent"
        );
        d.act(Action::SetFloatingFollow(FloatingFollow::Screen), now);
        assert_eq!(
            drawn(&mut d, 2, now).0,
            moved,
            "it stays where it was drawn"
        );
        d.pan((600, 300), (100., 0.), now);
        d.pan_end(now);
        assert_eq!(drawn(&mut d, 2, now).0, moved, "and no longer follows");
    }

    #[test]
    fn canvas_mode_scales_floating_windows_with_the_zoom_and_matches_clicks_as_drawn() {
        let (mut d, now) = desktop();
        d.act(Action::SetFloatingFollow(FloatingFollow::Canvas), now);
        let (home, _) = drawn(&mut d, 2, now);
        d.act(Action::ZoomOut, now);
        let (pos, s) = drawn(&mut d, 2, now);
        assert!(s < 1., "drawn at the camera's scale: {s}");
        assert_ne!(pos, home);
        let inside = (pos.0 + (300. * s) as i32 - 3, pos.1 + 3);
        assert_eq!(d.window_at(inside.0, inside.1), Some(2), "matched as drawn");
        let beyond = (pos.0 + 300 - 3, pos.1 + (200. * s) as i32 + 3);
        assert_ne!(
            d.window_at(beyond.0, beyond.1),
            Some(2),
            "not at its 1:1 size"
        );
        d.act(Action::SetFloatingFollow(FloatingFollow::Screen), now);
        d.act(Action::ToggleOverview, now);
        let (pos, s) = drawn(&mut d, 2, now);
        assert!(s < 1., "the overview draws it smaller: {s}");
        let beyond = (pos.0 + 300 - 3, pos.1 + (200. * s) as i32 + 3);
        assert_ne!(
            d.window_at(beyond.0, beyond.1),
            Some(2),
            "in the overview a click is matched as drawn"
        );
    }
}
