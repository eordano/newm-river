//! How a new tiled window gets its span ("ask the app, then memory"). Its
//! first configure asks for a size only on an axis a window rule sizes and sends 0 elsewhere, so
//! the app picks its size there; once the app commits, the window takes its tiles: that size
//! rounded up to whole tiles with its border and gaps, as a minimum is, and cut to what fits. An
//! app that picks nothing useful (0 on an axis, or never asked) opens at the span of the most
//! recently focused open window of its app on the canvas, else the span its app's last window
//! closed or moved away with (a [`SPAN_MEMORY`]-app memory per canvas), else the canvas's default
//! size (unless an empty rule opts out), else one tile. A claimed placeholder keeps priority. The
//! window then takes one more configure, its cell; the usage log's `settle` line counts them.

use std::time::{Duration, Instant};

use newm_canvas::{Canvas, Id, Rect as Tiles};
use newm_kde::apps;
use niri_config::PresetSize;
use serde_json::{json, Value};

use super::usage::{now_ms, since_last_open, TELEMETRY};
use super::{Desktop, Output, OutputId, Window};

/// How many apps' last spans a canvas remembers.
pub const SPAN_MEMORY: usize = 64;

/// How long a new window has to take the size of its cell before the usage log calls it
/// unsettled.
pub const SETTLE_WAIT: Duration = Duration::from_secs(5);

/// The span a new tiled window's rules set along each axis, while it waits off its canvas for its
/// app's first commit: its first configure asks for these axes only.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Opening {
    pub rule: (Option<i32>, Option<i32>),
}

/// A new window's way to the size of its cell, for the usage log's `settle` line: when it
/// mapped, how many sizes were asked of it, and the last one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Watch {
    pub since: Instant,
    pub configures: u32,
    pub asked: Option<(i32, i32)>,
}

/// The span a preset size takes along one axis of `canvas`, to the nearest tile, 1 to 64.
pub(super) fn preset_span(canvas: &Canvas, size: PresetSize, horizontal: bool) -> i32 {
    let (tw, th) = canvas.base_tile();
    let (sw, sh) = canvas.screen();
    let (tile, screen) = if horizontal { (tw, sw) } else { (th, sh) };
    let n = match size {
        PresetSize::Fixed(px) => f64::from(px) / tile,
        PresetSize::Proportion(p) => p * screen / tile,
    };
    (n.round() as i32).clamp(1, 64)
}

/// Remembers `span` as the last one of `app`'s windows on `o`; past [`SPAN_MEMORY`] apps the
/// least recent goes.
pub(super) fn remember_span(o: &mut Output, app: String, span: (i32, i32)) {
    let app = apps::canonical(&app);
    o.span_memory.retain(|(a, _)| *a != app);
    o.span_memory.push((app, span));
    if o.span_memory.len() > SPAN_MEMORY {
        o.span_memory.remove(0);
    }
}

fn remembered_span(o: &Output, app: &str) -> Option<(i32, i32)> {
    o.span_memory
        .iter()
        .rev()
        .find(|(a, _)| a == app)
        .map(|&(_, span)| span)
}

impl Desktop {
    /// The spans `w`'s window rules set (`default-column-width`, `default-window-height`).
    pub(super) fn rule_span(canvas: &Canvas, w: &Window) -> (Option<i32>, Option<i32>) {
        let rule = |size: Option<Option<PresetSize>>, horizontal| {
            size.flatten().map(|s| preset_span(canvas, s, horizontal))
        };
        (
            rule(w.rules.default_width, true),
            rule(w.rules.default_height, false),
        )
    }

    /// The size an opening window's first configure asks for: on an axis a rule sizes, that
    /// span's cell (cut to what fits) less the border; 0 elsewhere, where the app picks.
    pub(super) fn first_configure(&self, o: &Output, w: &Window, op: Opening) -> (i32, i32) {
        let (rw, rh) = op.rule;
        let want = Some((rw.unwrap_or(1), rh.unwrap_or(1)));
        let (sw, sh) = o.canvas.open_span(0., 0., want);
        let (cw, ch) = self.tiled_size(o, w, Tiles::new(0, 0, sw, sh));
        (
            if rw.is_some() { cw } else { 0 },
            if rh.is_some() { ch } else { 0 },
        )
    }

    /// The span of the most recently focused open tiled window of `app` on `o`, other than `id`.
    fn sibling_span(&self, o: &Output, app: &str, id: Id) -> Option<(i32, i32)> {
        o.canvas.recent().iter().find_map(|&s| {
            let w = self.windows.get(&s)?;
            let ours = s != id && w.is_tiled() && !w.fullscreen;
            if !ours
                || !w
                    .app_id
                    .as_deref()
                    .is_some_and(|id| apps::canonical(id) == app)
            {
                return None;
            }
            o.canvas.rect(s).map(|r| (r.w, r.h))
        })
    }

    /// Opening windows whose app committed a size take their tiles.
    pub(super) fn open_mapped(&mut self) {
        let ready: Vec<Id> = self
            .windows
            .values()
            .filter(|w| w.opening.is_some() && w.size.is_some())
            .map(|w| w.id)
            .collect();
        for id in ready {
            self.open_one(id);
        }
    }

    /// Puts opening window `id` on its canvas (see the module doc), logged as `open` with where
    /// its span came from.
    pub(super) fn open_one(&mut self, id: Id) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let (Some(op), Some(out)) = (w.opening.take(), w.output) else {
            return;
        };
        let w = &self.windows[&id];
        let Some(o) = self.output(out) else {
            return;
        };
        if o.canvas.rect(id).is_some() {
            return;
        }
        let got = w.size.unwrap_or_default();
        let asked = w.asked;
        let pad = f64::from(w.rules.border.width * 2) + self.inset() * 2. + 1.;
        let need = o
            .canvas
            .need(f64::from(got.0) + pad, f64::from(got.1) + pad);
        let picked = |asked: Option<i32>, got: i32| asked == Some(0) && got > 0;
        let memory = w.app_id.as_deref().map(apps::canonical).and_then(|app| {
            self.sibling_span(o, &app, id)
                .map(|s| (s, "sibling"))
                .or_else(|| remembered_span(o, &app).map(|s| (s, "last-closed")))
        });
        let default =
            |rule: Option<Option<PresetSize>>, size: Option<PresetSize>, horizontal| match rule {
                Some(None) => None,
                _ => size.map(|s| preset_span(&o.canvas, s, horizontal)),
            };
        let axis = |rule: Option<i32>,
                    app: Option<i32>,
                    memory: Option<(i32, &'static str)>,
                    default: Option<i32>| {
            rule.map(|n| (n, "rule"))
                .or(app.map(|n| (n, "app")))
                .or(memory)
                .or(default.map(|n| (n, "default")))
                .unwrap_or((1, "tile"))
        };
        let (sw, from_w) = axis(
            op.rule.0,
            picked(asked.map(|a| a.0), got.0).then_some(need.0),
            memory.map(|((n, _), s)| (n, s)),
            default(w.rules.default_width, self.settings.default_width, true),
        );
        let (sh, from_h) = axis(
            op.rule.1,
            picked(asked.map(|a| a.1), got.1).then_some(need.1),
            memory.map(|((_, n), s)| (n, s)),
            default(w.rules.default_height, self.settings.default_height, false),
        );
        let mut fields = serde_json::Map::new();
        fields.insert(
            "size_source".into(),
            if from_w == from_h {
                json!(from_w)
            } else {
                json!({ "w": from_w, "h": from_h })
            },
        );
        fields.insert("app_px".into(), json!([got.0, got.1]));
        fields.insert("since_last_open_ms".into(), json!(since_last_open()));
        let (min_w, min_h) = self.tile_min(w);
        let focus = self.focus == Some(id);
        let before = o.canvas.focus();
        self.logged_with(
            "open",
            Some(out),
            Some(id),
            |c| {
                c.add(id, min_w, min_h, Some((sw, sh)));
                if !focus && let Some(prev) = before {
                    c.focus_id(prev);
                }
            },
            |d, _| open_fields(d, fields, out, id),
        );
    }

    /// A new tiled window mapped: the usage log follows it to the size of its cell. Without
    /// telemetry no window is followed, so no settle deadline wakes the loop.
    pub(super) fn watch(&mut self, id: Id, now: Instant) {
        if !TELEMETRY {
            return;
        }
        if let Some(w) = self.windows.get_mut(&id)
            && w.is_tiled()
        {
            w.watch = Some(Watch {
                since: now,
                configures: u32::from(w.asked.is_some()),
                asked: w.asked,
            });
        }
    }

    /// Logs a `settle` line once watched window `id` shows the size asked of it last, its cell:
    /// how many sizes were asked of it and the time since it mapped.
    pub(super) fn check_settled(&mut self, id: Id, now: Instant) {
        if !TELEMETRY {
            return;
        }
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let (Some(watch), Some(asked)) = (w.watch, w.asked) else {
            return;
        };
        if asked.0 <= 0 || asked.1 <= 0 || w.size != Some(asked) {
            return;
        }
        w.watch = None;
        let line = self.settle_line("settle", id, watch, now);
        self.emit(line);
    }

    /// Logs an `unsettled` line for each new window that has not shown its cell's size
    /// [`SETTLE_WAIT`] after it mapped, and stops following it.
    pub(super) fn watch_unsettled(&mut self, now: Instant) {
        if !TELEMETRY {
            return;
        }
        let late: Vec<(Id, Watch)> = self
            .windows
            .values()
            .filter_map(|w| Some((w.id, w.watch?)))
            .filter(|(_, w)| now.saturating_duration_since(w.since) >= SETTLE_WAIT)
            .collect();
        for (id, watch) in late {
            if let Some(w) = self.windows.get_mut(&id) {
                w.watch = None;
            }
            let line = self.settle_line("unsettled", id, watch, now);
            self.emit(line);
        }
    }

    /// When the first watched window is due to be called unsettled.
    pub(super) fn watch_deadline(&self) -> Option<Instant> {
        if !TELEMETRY {
            return None;
        }
        self.windows
            .values()
            .filter_map(|w| w.watch)
            .map(|w| w.since + SETTLE_WAIT)
            .min()
    }

    fn settle_line(&self, op: &str, id: Id, watch: Watch, now: Instant) -> Value {
        json!({
            "t": now_ms(),
            "op": op,
            "app": self.app_of(id),
            "window": id,
            "configures": watch.configures,
            "ms": now.saturating_duration_since(watch.since).as_millis() as u64,
        })
    }
}

/// The open line's fields that are known once the window is on its canvas.
fn open_fields(
    d: &Desktop,
    mut fields: serde_json::Map<String, Value>,
    out: OutputId,
    id: Id,
) -> Value {
    let span = d
        .output(out)
        .and_then(|o| o.canvas.rect(id))
        .map(|r| [r.w, r.h]);
    fields.insert("span".into(), json!(span));
    fields.insert("focused".into(), json!(d.focus == Some(id)));
    Value::Object(fields)
}

#[cfg(test)]
mod tests {
    use newm_canvas::Config as CanvasConfig;
    use niri_config::Config;

    use super::super::{Rect, Settings};
    use super::*;

    /// A 1428x920 output, tiles of 119x113, gaps 10, border 1, and the canvas's default size
    /// (960 px by 0.6667 of the screen: 8x5 tiles) when `default`.
    fn desktop(rules: &str, default: bool) -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            gaps: 10.,
            default_width: default.then_some(PresetSize::Fixed(960)),
            default_height: default.then_some(PresetSize::Proportion(0.6667)),
            ..Settings::default()
        };
        let rules = Config::parse_mem(rules).unwrap().window_rules;
        let mut d = Desktop::new(settings, rules, now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1428, 920));
        (d, now)
    }

    /// Opens window `id` of `app` as river does: placed in a manage sequence, its first configure
    /// answered by the app, which keeps `picks` on an axis left to it (0: it picks nothing), then
    /// its cell. Returns what the first configure asked.
    fn open(d: &mut Desktop, id: Id, app: &str, picks: (i32, i32), now: Instant) -> (i32, i32) {
        d.add_window(id);
        d.window_mut(id).unwrap().app_id = Some(app.into());
        d.manage(now);
        let first = d.proposals().into_iter().find(|p| p.0 == id).unwrap().1;
        d.set_asked(id, first);
        let pick = |asked: i32, picked: i32| if asked == 0 { picked } else { asked };
        d.on_dimensions(id, pick(first.0, picks.0), pick(first.1, picks.1));
        for _ in 0..3 {
            d.manage(now);
            for (w, size) in d.proposals() {
                d.set_asked(w, size);
                d.on_dimensions(w, size.0, size.1);
            }
        }
        first
    }

    fn span(d: &Desktop, id: Id) -> (i32, i32) {
        let r = d.output(1).unwrap().canvas.rect(id).unwrap();
        (r.w, r.h)
    }

    #[cfg(feature = "telemetry")]
    fn last(d: &Desktop, op: &str) -> Value {
        d.motion
            .lines
            .iter()
            .rev()
            .find(|l| l["op"] == op)
            .cloned()
            .unwrap_or_else(|| panic!("no {op} line: {:#?}", d.motion.lines))
    }

    /// A window no rule sizes is asked nothing at first and opens at the size it picked, rounded
    /// up to whole tiles with its border and gaps (300 + 13 px over 119 px tiles, 250 + 13 over
    /// 113), not at the canvas's default size; then it takes one more configure, its cell.
    #[test]
    fn the_app_picks_and_its_size_rounds_up_to_whole_tiles() {
        let (mut d, now) = desktop("", true);
        let first = open(&mut d, 1, "term", (300, 250), now);
        assert_eq!(first, (0, 0), "the first configure asks nothing");
        assert_eq!(span(&d, 1), (3, 3));
        assert_eq!(
            d.window(1).unwrap().asked,
            Some((3 * 119 - 12, 3 * 113 - 12))
        );
        #[cfg(feature = "telemetry")]
        {
            let open = last(&d, "open");
            assert_eq!(open["size_source"], "app", "{open:#}");
            assert_eq!(open["app_px"], json!([300, 250]), "{open:#}");
            assert_eq!(open["span"], json!([3, 3]), "{open:#}");
            assert_eq!(open["focused"], true, "{open:#}");
            assert!(open["op_us"].is_u64(), "{open:#}");
            let settle = last(&d, "settle");
            assert_eq!(settle["configures"], 2, "0x0, then the cell: {settle:#}");
        }
    }

    /// An app that picks no size opens at the span of the most recently focused open window of
    /// its app on the canvas, before the canvas's default size.
    #[test]
    fn a_sibling_of_the_same_app_gives_its_span() {
        let (mut d, now) = desktop("", true);
        open(&mut d, 1, "term", (300, 250), now);
        open(&mut d, 2, "other", (0, 0), now);
        assert_eq!(span(&d, 2), (8, 5), "another app: the default");
        #[cfg(feature = "telemetry")]
        assert_eq!(last(&d, "open")["size_source"], "default");
        open(&mut d, 3, "term", (0, 0), now);
        assert_eq!(span(&d, 3), (3, 3), "term's open window's span");
        #[cfg(feature = "telemetry")]
        assert_eq!(last(&d, "open")["size_source"], "sibling");
    }

    /// With none of its app's windows open, an app that picks no size opens at the span its
    /// app's last window here closed with.
    #[test]
    fn the_last_closed_window_gives_its_span() {
        let (mut d, now) = desktop("", true);
        open(&mut d, 1, "term", (300, 250), now);
        d.remove_window(1, now);
        open(&mut d, 2, "term", (0, 0), now);
        assert_eq!(span(&d, 2), (3, 3), "the closed window's span");
        #[cfg(feature = "telemetry")]
        assert_eq!(last(&d, "open")["size_source"], "last-closed");
    }

    /// The span memory folds the case of app ids.
    #[test]
    fn the_span_memory_folds_the_case_of_app_ids() {
        let (mut d, now) = desktop("", true);
        open(&mut d, 1, "Term", (300, 250), now);
        open(&mut d, 2, "term", (0, 0), now);
        assert_eq!(span(&d, 2), (3, 3), "Term's open window's span");
        d.remove_window(1, now);
        d.remove_window(2, now);
        open(&mut d, 3, "TERM", (0, 0), now);
        assert_eq!(span(&d, 3), (3, 3), "the closed window's span");
        #[cfg(feature = "telemetry")]
        assert_eq!(last(&d, "open")["size_source"], "last-closed");
    }

    /// A window rule's size beats the size the app picks on its axis, and the first configure
    /// asks for it there; the app still picks on the other axis.
    #[test]
    fn a_rule_beats_the_app() {
        let rule = r#"
            window-rule {
                match app-id="^ruled$"
                default-column-width { fixed 480; }
            }
        "#;
        let (mut d, now) = desktop(rule, true);
        let first = open(&mut d, 1, "ruled", (300, 250), now);
        assert_eq!(
            first,
            (4 * 119 - 12, 0),
            "480 px: 4 tiles; the height left to the app"
        );
        assert_eq!(span(&d, 1), (4, 3));
        #[cfg(feature = "telemetry")]
        assert_eq!(
            last(&d, "open")["size_source"],
            json!({ "w": "rule", "h": "app" })
        );
    }

    /// An app that picks no size, with no window of its app to go by, opens at the canvas's
    /// default size, and at one tile without one.
    #[test]
    fn no_useful_size_falls_back_to_the_default_then_one_tile() {
        let (mut d, now) = desktop("", true);
        open(&mut d, 1, "new", (0, 0), now);
        assert_eq!(span(&d, 1), (8, 5));
        #[cfg(feature = "telemetry")]
        assert_eq!(last(&d, "open")["size_source"], "default");

        let (mut d, now) = desktop("", false);
        open(&mut d, 1, "new", (0, 0), now);
        assert_eq!(span(&d, 1), (1, 1));
        #[cfg(feature = "telemetry")]
        assert_eq!(last(&d, "open")["size_source"], "tile");
    }

    /// An app that keeps a larger size than its cell grows its span through the placement
    /// rules, and the usage log says so.
    #[test]
    fn an_app_growing_its_window_is_logged() {
        let (mut d, now) = desktop("", false);
        open(&mut d, 1, "a", (0, 0), now);
        open(&mut d, 2, "b", (0, 0), now);
        let h = d.window(1).unwrap().asked.unwrap().1;
        assert!(d.on_dimensions(1, 300, h), "a kept size grows the window");
        d.manage(now);
        assert_eq!(span(&d, 1).0, 3, "300 + 13 px take 3 tiles");
        #[cfg(feature = "telemetry")]
        {
            let grow = last(&d, "app-grow");
            assert_eq!(grow["cause"], "kept-commit", "{grow:#}");
            assert_eq!(grow["pushed"], false, "{grow:#}");
        }
    }
}
