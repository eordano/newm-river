//! Restored windows find their saved spots by title. A
//! window takes the placeholder whose saved title is closest to its own when it maps
//! ([`crate::placeholder::Placeholders::claim`]), but many apps title their windows only
//! afterwards: a terminal once tmux or ssh attaches, a browser once its restored tab loads. Until
//! [`REMATCH_FOR`] after a restore starts, a window whose title changes trades spots with the
//! placeholder or restored window of its app that fits the titles better.
//!
//! newm-river has a canvas per output and a session restores them all at once, so the claim at
//! map time takes the best placeholder on any output (its window opens on that output), and a
//! trade may cross outputs: each side moves to the other's canvas, output and tiles, a
//! placeholder as a placeholder that the restore of its new canvas then waits for. A fullscreen
//! window keeps its output, as river fullscreens it there; floating windows hold no spot.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use newm_canvas::Id;
use newm_kde::apps;
use serde_json::json;

use super::usage::{now_ms, TELEMETRY};
use super::{Desktop, OutputId};

/// How long after a restore starts its windows may still trade spots as their titles arrive: an
/// ssh window can wait for a security key touch.
pub const REMATCH_FOR: Duration = Duration::from_secs(90);

/// The score from which a title picks a placeholder at map time rather than the app alone.
pub const TITLE_MATCH: f64 = 0.5;

/// How much better a trade must fit the titles than the spots held now.
const MARGIN: f64 = 0.25;

/// The saved spot a restored window took: its app and the title saved with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub app_id: String,
    pub title: Option<String>,
}

/// How well `now`, a window's title, fits `saved`, the title saved with a spot: 1 when they are
/// equal, else at most 0.99 times the share of their lowercase alphanumeric words they have in
/// common, so a spinner glyph, an unread count or a "1 new item" still finds the spot.
pub fn title_score(saved: Option<&str>, now: Option<&str>) -> f64 {
    let (Some(saved), Some(now)) = (saved, now) else {
        return 0.;
    };
    if saved == now {
        return 1.;
    }
    let words = |s: &str| -> BTreeSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let (a, b) = (words(saved), words(now));
    let union = a.union(&b).count();
    if union == 0 {
        return 0.;
    }
    0.99 * a.intersection(&b).count() as f64 / union as f64
}

/// What a retitled window trades its spot with, and the output that is on.
#[derive(Debug, Clone, Copy)]
enum Trade {
    Placeholder(Id, OutputId),
    Window(Id, OutputId),
}

impl Desktop {
    /// Window `id` got a new title: during a restore, it trades spots with the placeholder or
    /// restored window of its app, on any output, whose saved title fits it better (see the
    /// module docs). Returns whether it moved.
    pub fn retitled(&mut self, id: Id, now: Instant) -> bool {
        if !self.placeholders.rematching(now) {
            return false;
        }
        let Some(w) = self.windows.get(&id).filter(|w| w.is_tiled()) else {
            return false;
        };
        let (Some(app), Some(out)) = (w.app_id.clone(), w.output) else {
            return false;
        };
        let on_canvas = |d: &Self, out: OutputId, id: Id| {
            d.output(out).is_some_and(|o| o.canvas.rect(id).is_some())
        };
        if !on_canvas(self, out, id) {
            return false;
        }
        // Whether a trade may take it to `to` with a window that is fullscreen or not.
        let crosses = |to: OutputId, other_full: bool| to == out || !(w.fullscreen || other_full);
        let title = w.title.clone();
        let title = title.as_deref();
        let sx = w.slot.as_ref().and_then(|s| s.title.clone());
        let cur = title_score(sx.as_deref(), title);
        if cur >= 1. {
            return false;
        }

        let mut best: Option<(Trade, f64)> = None;
        let mut consider = |trade: Trade, gain: f64| {
            if gain > MARGIN && best.is_none_or(|(_, g)| gain > g) {
                best = Some((trade, gain));
            }
        };
        for p in self.placeholders.iter() {
            if apps::same(&p.app_id, &app)
                && crosses(p.output, false)
                && on_canvas(self, p.output, p.id)
            {
                consider(
                    Trade::Placeholder(p.id, p.output),
                    title_score(p.title.as_deref(), title) - cur,
                );
            }
        }
        for (&y, e) in &self.windows {
            let (Some(slot), Some(at)) = (&e.slot, e.output) else {
                continue;
            };
            if y == id
                || !apps::same(&slot.app_id, &app)
                || !e.is_tiled()
                || !crosses(at, e.fullscreen)
            {
                continue;
            }
            if !on_canvas(self, at, y) {
                continue;
            }
            let other = e.title.as_deref();
            let sy = slot.title.as_deref();
            let gain = title_score(sy, title) + title_score(sx.as_deref(), other)
                - cur
                - title_score(sy, other);
            consider(Trade::Window(y, at), gain);
        }
        let Some((trade, gain)) = best else {
            return false;
        };
        let to = match trade {
            Trade::Window(_, to) | Trade::Placeholder(_, to) => to,
        };
        let name = |d: &Self, o: OutputId| d.output(o).map(|o| o.name.clone());
        let outputs = (to != out).then(|| (name(self, out), name(self, to)));

        match trade {
            Trade::Window(y, _) => {
                self.swap_spots((out, id), (to, y), now);
                let a = self.windows.get_mut(&id).and_then(|w| w.slot.take());
                let b = self.windows.get_mut(&y).and_then(|w| w.slot.take());
                if let Some(w) = self.windows.get_mut(&id) {
                    w.slot = b;
                }
                if let Some(w) = self.windows.get_mut(&y) {
                    w.slot = a;
                }
                self.placeholders.retarget_focus(id, y);
            }
            Trade::Placeholder(pid, _) => {
                self.swap_spots((out, id), (to, pid), now);
                let Some(p) = self.placeholders.get_mut(pid) else {
                    return true;
                };
                let took = Slot {
                    app_id: p.app_id.clone(),
                    title: p.title.take(),
                };
                let left = self.windows.get_mut(&id).and_then(|w| w.slot.replace(took));
                match left {
                    // The placeholder now waits on the saved spot the window left.
                    Some(left) => {
                        if let Some(p) = self.placeholders.get_mut(pid) {
                            p.app_id = left.app_id;
                            p.title = left.title;
                        }
                        self.placeholders.retarget_focus(id, pid);
                    }
                    // The window left a spot no session saved, on `out`: nothing waits there.
                    None => {
                        self.placeholders.take(pid, id);
                        self.apply(Some(out), |c| c.remove(pid));
                    }
                }
            }
        }
        if TELEMETRY {
            let mut line = json!({
                "t": now_ms(),
                "op": "restore-rematch",
                "app": apps::canonical(&app),
                "with": match trade { Trade::Window(..) => "window", Trade::Placeholder(..) => "placeholder" },
                "gain": (gain * 100.).round() / 100.,
            });
            if let Some((from, to)) = outputs {
                line["output"] = json!(from);
                line["with_output"] = json!(to);
            }
            self.emit(line);
        }
        self.finish_restores(now);
        true
    }

    /// Exchanges the tiles of canvas ids `a` and `b`, each on its output, each keeping its own
    /// minimum (a placeholder has none). On one canvas the focus stays on its window. Across two,
    /// each moves to the other's canvas and output, a canvas whose focus left focuses what took
    /// its place, and the keyboard focus, its canvas and the active output follow a window it is
    /// on.
    fn swap_spots(&mut self, (out, a): (OutputId, Id), (to, b): (OutputId, Id), now: Instant) {
        let min = |d: &Self, id: Id| d.windows.get(&id).map_or((0., 0.), |w| d.tile_min(w));
        let (min_a, min_b) = (min(self, a), min(self, b));
        if out != to {
            self.logged("rematch", Some(out), Some(a), |c| {
                c.rename(a, b);
                c.set_min(b, min_b.0, min_b.1);
            });
            self.logged("rematch", Some(to), Some(a), |c| {
                c.rename(b, a);
                c.set_min(a, min_a.0, min_a.1);
            });
            self.rehome(a, to);
            self.rehome(b, out);
            if let Some(f) = self.focus.filter(|&f| f == a || f == b) {
                self.focus_window(f, now);
            }
            return;
        }
        let tmp = self.placeholders.alloc();
        let focus = self.output(out).and_then(|o| o.canvas.focus());
        self.logged("rematch", Some(out), Some(a), |c| {
            c.rename(a, tmp);
            c.rename(b, a);
            c.rename(tmp, b);
            c.set_min(a, min_a.0, min_a.1);
            c.set_min(b, min_b.0, min_b.1);
            if let Some(f) = focus.filter(|&f| f == a || f == b) {
                c.focus_id(f);
            }
        });
    }

    /// Canvas id `id` went to `out`'s canvas in a trade: a window's output, or a placeholder's
    /// and the restore that waits for it.
    fn rehome(&mut self, id: Id, out: OutputId) {
        self.forget_glides(id);
        match self.windows.get_mut(&id) {
            Some(w) => {
                w.output = Some(out);
                self.held.remove(&id);
                self.shrinking.retain(|&s| s != id);
            }
            None => self.placeholders.rehome(id, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use newm_canvas::{Config as CanvasConfig, Rect as Tiles};
    use niri_config::PresetSize;

    use super::super::{Rect, Settings};
    use super::*;
    use crate::placeholder::{RestoreCanvas, RestoreWindow};

    #[test]
    fn equal_titles_score_one_and_only_they_do() {
        assert_eq!(title_score(Some("desk: ~"), Some("desk: ~")), 1.);
        assert!(title_score(Some("a b"), Some("b a")) < 1.);
        assert_eq!(title_score(None, Some("a")), 0.);
        assert_eq!(title_score(Some("a"), None), 0.);
        assert_eq!(title_score(Some("--"), Some("::")), 0.);
    }

    #[test]
    fn a_changed_marker_still_fits_better_than_another_window() {
        let saved = "* Alex Doe (DM) - Acme - Slack - Chromium";
        let same = "Alex Doe (DM) - Acme - 1 new item - Slack - Chromium";
        let other = "Sam Roe (DM) - Acme - Slack - Chromium";
        let page = "refactor(parser): read config files lazily - Chromium";
        assert!(title_score(Some(saved), Some(same)) >= TITLE_MATCH);
        assert!(title_score(Some(saved), Some(same)) > title_score(Some(saved), Some(other)));
        assert!(title_score(Some(saved), Some(page)) < TITLE_MATCH);
        let spinner = title_score(Some("desk: ✳ todo-app"), Some("desk: ✶ todo-app"));
        assert!(spinner > 0.9, "{spinner}");
    }

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            default_width: Some(PresetSize::Fixed(960)),
            default_height: Some(PresetSize::Proportion(0.6667)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        (d, now)
    }

    /// The windows' size requests answered as an app would, until none is asked a new size.
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

    /// A window of `app_id` titled `title` opens as river would.
    fn open(d: &mut Desktop, id: Id, app_id: &str, title: &str, now: Instant) {
        d.add_window(id);
        let w = d.window_mut(id).unwrap();
        w.app_id = Some(app_id.into());
        w.title = Some(title.into());
        settle(d, now);
    }

    /// Window `id` gets a new title, as an app gives one after it maps.
    fn retitle(d: &mut Desktop, id: Id, title: &str, now: Instant) -> bool {
        d.window_mut(id).unwrap().title = Some(title.into());
        let moved = d.retitled(id, now);
        settle(d, now);
        moved
    }

    fn at(d: &Desktop, id: Id) -> Tiles {
        d.output(1).unwrap().canvas.rect(id).unwrap()
    }

    fn saved(app_id: &str, title: &str, rect: Tiles) -> RestoreWindow {
        RestoreWindow {
            app_id: app_id.into(),
            title: Some(title.into()),
            rect,
        }
    }

    const A: Tiles = Tiles::new(0, 0, 4, 2);
    const B: Tiles = Tiles::new(5, 0, 4, 2);
    const C: Tiles = Tiles::new(0, 3, 4, 2);
    const D: Tiles = Tiles::new(5, 3, 4, 2);

    /// A restore moves windows to their saved spots as titles arrive.
    #[test]
    fn restored_windows_move_to_their_saved_spots_as_titles_arrive() {
        let (mut d, now) = desktop();
        let canvas = RestoreCanvas {
            windows: vec![
                saved("term", "desk: ✳ a", A),
                saved("term", "build: ✳ b", B),
                saved("browser", "* Slack - Chromium", C),
                saved("browser", "PR 1 - Chromium", D),
            ],
            zoom: 0,
            view: (0., 0.),
            focus: Some(1),
        };
        d.restore(Some("OUT-1"), canvas, Duration::from_secs(20), now);

        open(&mut d, 1, "term", "ghostty", now);
        open(&mut d, 2, "term", "ghostty", now);
        assert_eq!(at(&d, 1), A, "no title yet: the first spot");
        assert_eq!(at(&d, 2), B);

        assert!(retitle(&mut d, 1, "build: ✶ b", now));
        assert_eq!(at(&d, 1), B, "its title takes it to its spot");
        assert_eq!(at(&d, 2), A, "the other gets the one it left");
        #[cfg(feature = "telemetry")]
        let traded = |d: &Desktop, with: &str| {
            d.motion
                .lines
                .iter()
                .any(|l| l["op"] == "restore-rematch" && l["with"] == with && l["app"] == "term")
        };
        #[cfg(feature = "telemetry")]
        assert!(traded(&d, "window"), "{:#?}", d.motion.lines);
        assert!(!retitle(&mut d, 2, "desk: ✳ a", now), "already home");
        assert_eq!(at(&d, 2), A);

        open(&mut d, 3, "browser", "New Tab - Chromium", now);
        assert_eq!(at(&d, 3), C);
        assert!(retitle(&mut d, 3, "PR 1 - Chromium", now));
        assert_eq!(at(&d, 3), D, "it trades with the placeholder still waiting");
        assert!(d.placeholders.is_restoring(), "the Slack spot still waits");
        open(&mut d, 4, "browser", "Slack - 1 new item - Chromium", now);
        assert_eq!(at(&d, 4), C, "the spot the PR left");
        assert!(!d.placeholders.is_restoring());
        assert_eq!(d.focus(), Some(1), "the saved focus follows the spot");
        assert!(
            d.placeholder_frames().is_empty(),
            "no placeholder left over"
        );
        assert_eq!(d.output(1).unwrap().canvas.windows().len(), 4);
    }

    /// A restore stops trading spots after a while.
    #[test]
    fn restored_windows_stop_trading_spots_after_a_while() {
        let (mut d, now) = desktop();
        let canvas = RestoreCanvas {
            windows: vec![saved("term", "a", A), saved("term", "b", B)],
            zoom: 0,
            view: (0., 0.),
            focus: None,
        };
        d.restore(Some("OUT-1"), canvas, Duration::from_secs(20), now);
        open(&mut d, 1, "term", "x", now);
        open(&mut d, 2, "term", "y", now);
        let later = now + Duration::from_secs(91);
        assert!(!retitle(&mut d, 1, "b", later));
        assert_eq!(at(&d, 1), A, "settled: stays");
    }

    /// Retitles outside a restore move nothing.
    #[test]
    fn retitles_outside_a_restore_move_nothing() {
        let (mut d, now) = desktop();
        open(&mut d, 1, "term", "a", now);
        open(&mut d, 2, "other", "b", now);
        let before = (at(&d, 1), at(&d, 2));
        assert!(!retitle(&mut d, 1, "b", now));
        assert_eq!((at(&d, 1), at(&d, 2)), before);
    }

    /// A window that opened on its own, no saved spot behind it, trades with a placeholder that
    /// fits its title: the spot it left is nobody's, so nothing waits there.
    #[test]
    fn an_unrestored_window_takes_a_waiting_spot_and_leaves_none() {
        let (mut d, now) = desktop();
        open(&mut d, 1, "term", "x", now);
        let own = at(&d, 1);
        let canvas = RestoreCanvas {
            windows: vec![saved("term", "build: b", B), saved("term", "desk: a", D)],
            zoom: 0,
            view: (0., 0.),
            focus: Some(0),
        };
        d.restore(Some("OUT-1"), canvas, Duration::from_secs(20), now);
        assert!(retitle(&mut d, 1, "build: b", now));
        assert_ne!(at(&d, 1), own);
        assert_eq!(d.placeholder_frames().len(), 1, "only the desk spot waits");
        open(&mut d, 2, "term", "desk: a", now);
        assert!(!d.placeholders.is_restoring());
        assert_eq!(d.focus(), Some(1), "the saved focus went with the spot");
    }

    /// Two 1920x1080 outputs side by side, OUT-1 then OUT-2.
    fn two() -> (Desktop, Instant) {
        let (mut d, now) = desktop();
        d.add_output(2);
        d.set_output_area(2, Rect::new(1920, 0, 1920, 1080));
        (d, now)
    }

    fn put_back(d: &mut Desktop, out: &str, windows: Vec<RestoreWindow>, now: Instant) {
        let canvas = RestoreCanvas {
            windows,
            zoom: 0,
            view: (0., 0.),
            focus: Some(0),
        };
        d.restore(Some(out), canvas, Duration::from_secs(20), now);
    }

    /// The output window `id` is on and its tiles there.
    fn on(d: &Desktop, id: Id) -> (OutputId, Tiles) {
        let out = d.window(id).unwrap().output.unwrap();
        (out, d.output(out).unwrap().canvas.rect(id).unwrap())
    }

    #[cfg(feature = "telemetry")]
    fn crossed(d: &Desktop, with: &str) -> bool {
        d.motion.lines.iter().any(|l| {
            l["op"] == "restore-rematch"
                && l["with"] == with
                && l["output"] == "OUT-1"
                && l["with_output"] == "OUT-2"
        })
    }

    fn check(d: &Desktop) {
        for o in d.outputs() {
            o.canvas.check().unwrap();
        }
    }

    /// The window saved on OUT-2 maps first and takes OUT-1's spot by its app (no title yet);
    /// once the titles arrive, each window is on its own saved output and tiles.
    #[test]
    fn restored_windows_cross_outputs_to_their_saved_spots_as_titles_arrive() {
        let (mut d, now) = two();
        let slack = saved("browser", "* Slack - Chromium", C);
        put_back(
            &mut d,
            "OUT-1",
            vec![saved("term", "desk: ✳ a", A), slack],
            now,
        );
        put_back(&mut d, "OUT-2", vec![saved("term", "build: ✳ b", B)], now);

        open(&mut d, 1, "term", "ghostty", now);
        open(&mut d, 2, "term", "ghostty", now);
        assert_eq!(on(&d, 1), (1, A), "no title yet: the first spot, on OUT-1");
        assert_eq!(on(&d, 2), (2, B));

        assert!(retitle(&mut d, 1, "build: ✶ b", now));
        assert_eq!(
            on(&d, 1),
            (2, B),
            "its title takes it to its output and tiles"
        );
        assert_eq!(on(&d, 2), (1, A), "the other gets the spot it left");
        #[cfg(feature = "telemetry")]
        assert!(crossed(&d, "window"), "{:#?}", d.motion.lines);
        assert!(!retitle(&mut d, 2, "desk: ✳ a", now), "already home");
        assert_eq!(d.output(2).unwrap().canvas.focus(), Some(1));
        assert_eq!(d.focus(), Some(2), "the keyboard focus followed its window");
        assert_eq!(d.active_output(), Some(1));

        open(&mut d, 3, "browser", "Slack - Chromium", now);
        assert_eq!(on(&d, 3), (1, C));
        assert!(!d.placeholders.is_restoring());
        assert_eq!(
            d.output(1).unwrap().canvas.focus(),
            Some(2),
            "OUT-1's saved focus followed its spot"
        );
        check(&d);
    }

    /// A restored window on OUT-1 trades with the placeholder on OUT-2 that waits for its title:
    /// the placeholder moves to OUT-1 and waits there for the window of the spot it took, and
    /// OUT-2's restore ends.
    #[test]
    fn a_restored_window_trades_with_a_placeholder_on_another_output() {
        let (mut d, now) = two();
        let slack = saved("browser", "* Slack - Chromium", C);
        put_back(
            &mut d,
            "OUT-1",
            vec![saved("term", "desk: a", A), slack],
            now,
        );
        put_back(&mut d, "OUT-2", vec![saved("term", "build: b", B)], now);

        open(&mut d, 1, "term", "ghostty", now);
        assert_eq!(on(&d, 1), (1, A));
        assert!(retitle(&mut d, 1, "build: b", now));
        assert_eq!(on(&d, 1), (2, B));
        #[cfg(feature = "telemetry")]
        assert!(crossed(&d, "placeholder"), "{:#?}", d.motion.lines);
        let waiting: Vec<_> = d
            .placeholders
            .iter()
            .filter(|p| p.app_id == "term")
            .map(|p| (p.output, p.title.clone(), at(&d, p.id)))
            .collect();
        assert_eq!(waiting, vec![(1, Some("desk: a".into()), A)]);
        assert_eq!(
            d.output(2).unwrap().canvas.focus(),
            Some(1),
            "OUT-2's restore ended on its saved focus"
        );
        assert!(d.placeholders.is_restoring(), "OUT-1 still waits");

        open(&mut d, 2, "term", "ghostty", now);
        assert_eq!(on(&d, 2), (1, A), "the spot that went to OUT-1");
        open(&mut d, 3, "browser", "Slack - Chromium", now);
        assert!(!d.placeholders.is_restoring());
        assert_eq!(d.output(1).unwrap().canvas.focus(), Some(2));
        assert!(d.placeholder_frames().is_empty());
        check(&d);
    }

    #[test]
    fn restored_windows_stop_crossing_outputs_after_a_while() {
        let (mut d, now) = two();
        put_back(&mut d, "OUT-1", vec![saved("term", "a", A)], now);
        put_back(&mut d, "OUT-2", vec![saved("term", "b", B)], now);
        open(&mut d, 1, "term", "x", now);
        open(&mut d, 2, "term", "y", now);
        assert!(!retitle(&mut d, 1, "b", now + Duration::from_secs(91)));
        assert_eq!(on(&d, 1), (1, A), "settled: stays");
        assert_eq!(on(&d, 2), (2, B));
    }

    /// A fullscreen window keeps its output: river fullscreens it there.
    #[test]
    fn a_fullscreen_window_trades_on_its_output_only() {
        let (mut d, now) = two();
        put_back(&mut d, "OUT-1", vec![saved("term", "a", A)], now);
        put_back(&mut d, "OUT-2", vec![saved("term", "b", B)], now);
        open(&mut d, 1, "term", "x", now);
        open(&mut d, 2, "term", "y", now);
        d.set_fullscreen(2, true);
        assert!(!retitle(&mut d, 1, "b", now));
        assert!(!retitle(&mut d, 2, "a", now));
        assert_eq!((on(&d, 1), on(&d, 2)), ((1, A), (2, B)));
        d.set_fullscreen(2, false);
        assert!(retitle(&mut d, 2, "a", now));
        assert_eq!((on(&d, 1), on(&d, 2)), ((2, B), (1, A)));
    }

    /// OUT-2 leaves mid-restore: its placeholders and restore go with it, its window opens again
    /// on OUT-1 holding no saved spot, and nothing trades toward the spots that left.
    #[test]
    fn an_output_that_leaves_mid_restore_takes_its_spots_along() {
        let (mut d, now) = two();
        put_back(&mut d, "OUT-1", vec![saved("term", "desk: a", A)], now);
        let b = vec![saved("term", "build: b", B), saved("term", "build: c", D)];
        put_back(&mut d, "OUT-2", b, now);
        open(&mut d, 1, "term", "ghostty", now);
        open(&mut d, 2, "term", "ghostty", now);
        assert_eq!((on(&d, 1), on(&d, 2)), ((1, A), (2, B)));

        d.remove_output(2);
        settle(&mut d, now);
        assert_eq!(
            d.window(2).unwrap().output,
            Some(1),
            "placed again on OUT-1"
        );
        assert!(d.output(1).unwrap().canvas.rect(2).is_some());
        assert!(d.placeholders.iter().all(|p| p.output == 1));
        assert!(!d.placeholders.is_restoring());

        let placed = on(&d, 2);
        assert!(
            !retitle(&mut d, 1, "build: b", now),
            "its spot left, and where window 2 opened again is no saved spot"
        );
        assert!(!retitle(&mut d, 2, "build: c", now), "nor is that one here");
        assert_eq!((on(&d, 1), on(&d, 2)), ((1, A), placed));
        check(&d);
    }
}
